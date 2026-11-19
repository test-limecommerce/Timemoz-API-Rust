//! Port `src/mcp-api/projects/*` (`GET /mcp-api/projects`, `GET /mcp-api/projects/:uuid`).
//!
//! Audit C5: project wajib terdaftar di company pemanggil (semua role); non-manager hanya
//! project tempat ia menjadi anggota.

use axum::Extension;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::Uri;
use mz_admin_mcp::body::V;
use mz_core::time::JsDate;
use mz_core::uuid_rules::parse_uuid_pipe;
use mz_core::{ApiError, ApiResult, AppState};
use serde_json::{Value, json};
use sqlx::{Postgres, QueryBuilder};

use crate::common::{
    McpCaller, envelope, num, project_in_company_sql, resolve_mcp_caller, round2, skip_of,
};
use crate::dto::{finish, opt_uuid, pagination, query_of, str_of, truthy_str};
use crate::pat::PatUser;

/// `projectClient[0].client` / `projectCompany[0].company` -> `{ uuid, name }` atau `null`.
fn pair_json(uuid: Option<String>, name: Option<String>) -> Value {
    match (uuid, name) {
        (Some(u), Some(n)) => json!({ "uuid": u, "name": n }),
        _ => Value::Null,
    }
}

/// `LEFT JOIN LATERAL` client pertama + company pemanggil (`take: 1`, tanpa `orderBy` di Node ->
/// diurutkan `id` supaya stabil).
fn push_lateral_joins(qb: &mut QueryBuilder<'_, Postgres>, company_id: i32) {
    qb.push(
        r#" LEFT JOIN LATERAL (SELECT c.uuid::text AS uuid, c.name AS name
                                 FROM "projectClient" pcl JOIN client c ON c.id = pcl."clientId"
                                WHERE pcl."projectId" = p.id ORDER BY pcl.id LIMIT 1) cl ON true
            LEFT JOIN LATERAL (SELECT co.uuid::text AS uuid, co.name AS name
                                 FROM "projectCompany" pco JOIN company co ON co.id = pco."companyId"
                                WHERE pco."projectId" = p.id AND pco."companyId" = "#,
    )
    .push_bind(company_id)
    .push(" ORDER BY pco.id LIMIT 1) cp ON true");
}

type ListRow = (
    String,
    String,
    String,
    i32,
    bool,
    i32,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

/// Node: src/mcp-api/projects/projects.controller.ts:27 + projects.service.ts:11 (`GET`)
pub async fn h_list(
    State(state): State<AppState>,
    Extension(user): Extension<PatUser>,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let q = query_of(uri.query());
    // `ListProjectsDto`: search, status, clientUuid, lalu `PaginationDto`.
    let mut v = V::default();
    v.opt_string("search", q.get("search"));
    v.opt_is_in("status", q.get("status"), &["active", "archived"]);
    opt_uuid(&mut v, &q, "clientUuid");
    let (page, per_page) = pagination(&mut v, &q, 500, 500);
    finish(v)?;

    let caller = resolve_mcp_caller(&state.db, user.user_id).await?;
    let company_id = caller.company_id;
    let status_filter: i32 = if str_of(&q, "status") == Some("archived") {
        2
    } else {
        1
    };

    // `clientUuid` tak dikenal diabaikan diam-diam (sama dengan Node).
    let client_id: Option<i32> = match truthy_str(&q, "clientUuid") {
        Some(cu) => {
            sqlx::query_scalar("SELECT id FROM client WHERE uuid = $1::uuid")
                .bind(cu)
                .fetch_optional(&state.db)
                .await?
        }
        None => None,
    };

    let push_where = |qb: &mut QueryBuilder<'_, Postgres>| {
        qb.push(" WHERE p.status = ")
            .push_bind(status_filter)
            .push(" AND EXISTS (SELECT 1 FROM \"projectCompany\" pcx WHERE pcx.\"projectId\" = p.id AND pcx.\"companyId\" = ")
            .push_bind(company_id)
            .push(")");
        if !caller.is_manager {
            qb.push(" AND EXISTS (SELECT 1 FROM \"projectUser\" pu WHERE pu.\"projectId\" = p.id AND pu.\"userId\" = ")
                .push_bind(user.user_id)
                .push(")");
        }
        if let Some(s) = truthy_str(&q, "search") {
            let pattern = format!("%{s}%");
            qb.push(" AND (p.name ILIKE ")
                .push_bind(pattern.clone())
                .push(" OR p.code ILIKE ")
                .push_bind(pattern)
                .push(")");
        }
        if let Some(cid) = client_id {
            qb.push(" AND EXISTS (SELECT 1 FROM \"projectClient\" pcc WHERE pcc.\"projectId\" = p.id AND pcc.\"clientId\" = ")
                .push_bind(cid)
                .push(")");
        }
    };

    let mut count = QueryBuilder::<Postgres>::new("SELECT count(*) FROM project p");
    push_where(&mut count);
    let total: i64 = count.build_query_scalar().fetch_one(&state.db).await?;

    let skip = skip_of(page, per_page)?;
    let mut list = QueryBuilder::<Postgres>::new(
        r#"SELECT p.uuid::text, p.code, p.name, p.status, p.billable, p."totalHour",
                  cl.uuid, cl.name, cp.uuid, cp.name
             FROM project p"#,
    );
    push_lateral_joins(&mut list, company_id);
    push_where(&mut list);
    list.push(r#" ORDER BY p."createdAt" DESC, p.id DESC LIMIT "#)
        .push_bind(per_page)
        .push(" OFFSET ")
        .push_bind(skip);
    let rows: Vec<ListRow> = list.build_query_as().fetch_all(&state.db).await?;

    let data: Vec<Value> = rows
        .into_iter()
        .map(
            |(uuid, code, name, status, billable, total_hour, cl_u, cl_n, cp_u, cp_n)| {
                json!({
                    "uuid": uuid,
                    "code": code,
                    "name": name,
                    "status": if status == 1 { "active" } else { "archived" },
                    "billable": billable,
                    "totalHourTracked": total_hour,
                    "client": pair_json(cl_u, cl_n),
                    "company": pair_json(cp_u, cp_n),
                })
            },
        )
        .collect();
    Ok(Json(envelope(
        Value::Array(data),
        Some(json!({ "page": page, "perPage": per_page, "total": total })),
    )))
}

type DetailRow = (
    i32,
    String,
    String,
    String,
    JsDate,
    i32,
    bool,
    i32,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

/// Anggota project aktif (`projectUser` status 1): `{ uuid, name }`; user null -> `{}`.
async fn members_json(state: &AppState, project_id: i32) -> ApiResult<Vec<Value>> {
    let rows: Vec<(Option<String>, Option<String>)> = sqlx::query_as(
        r#"SELECT u.uuid::text, u.name
             FROM "projectUser" pu LEFT JOIN users u ON u.id = pu."userId"
            WHERE pu."projectId" = $1 AND pu.status = 1
            ORDER BY pu.id"#,
    )
    .bind(project_id)
    .fetch_all(&state.db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(uuid, name)| match (uuid, name) {
            (Some(u), Some(n)) => json!({ "uuid": u, "name": n }),
            _ => json!({}),
        })
        .collect())
}

/// Node: src/mcp-api/projects/projects.controller.ts:33 + projects.service.ts:90 (`GET :uuid`)
pub async fn h_detail(
    State(state): State<AppState>,
    Extension(user): Extension<PatUser>,
    Path(raw): Path<String>,
) -> ApiResult<Json<Value>> {
    let uuid = parse_uuid_pipe(&raw)?;
    let caller: McpCaller = resolve_mcp_caller(&state.db, user.user_id).await?;
    let company_id = caller.company_id;

    let sql = format!(
        r#"SELECT p.id, p.uuid::text, p.code, p.name, p."startDate"::timestamp, p."totalHour",
                  p.billable, p.status, cl.uuid, cl.name, cp.uuid, cp.name
             FROM project p
             LEFT JOIN LATERAL (SELECT c.uuid::text AS uuid, c.name AS name
                                  FROM "projectClient" pcl JOIN client c ON c.id = pcl."clientId"
                                 WHERE pcl."projectId" = p.id ORDER BY pcl.id LIMIT 1) cl ON true
             LEFT JOIN LATERAL (SELECT co.uuid::text AS uuid, co.name AS name
                                  FROM "projectCompany" pco JOIN company co ON co.id = pco."companyId"
                                 WHERE pco."projectId" = p.id AND pco."companyId" = $2
                                 ORDER BY pco.id LIMIT 1) cp ON true
            WHERE p.uuid = $1 AND {}
            LIMIT 1"#,
        project_in_company_sql("p", "$2")
    );
    let row: Option<DetailRow> = sqlx::query_as(&sql)
        .bind(uuid)
        .bind(company_id)
        .fetch_optional(&state.db)
        .await?;
    let Some((
        id,
        p_uuid,
        code,
        name,
        start_date,
        total_hour,
        _billable,
        _status,
        cl_u,
        cl_n,
        cp_u,
        cp_n,
    )) = row
    else {
        return Err(ApiError::not_found("Project not found"));
    };

    if !caller.is_manager {
        let is_member: Option<i32> = sqlx::query_scalar(
            r#"SELECT id FROM "projectUser" WHERE "projectId" = $1 AND "userId" = $2 AND status = 1 LIMIT 1"#,
        )
        .bind(id)
        .bind(user.user_id)
        .fetch_optional(&state.db)
        .await?;
        if is_member.is_none() {
            return Err(ApiError::not_found("Project not found"));
        }
    }

    let budget: Option<(f64, i32, i32)> = sqlx::query_as(
        r#"SELECT "budgetHours", "currentBurnedSeconds", "alertThreshold" FROM "projectBudget" WHERE "projectId" = $1"#,
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?;
    let (total_tasks, done_tasks): (i64, i64) = sqlx::query_as(
        r#"SELECT count(*), count(*) FILTER (WHERE lower(status) = 'done') FROM task WHERE "projectId" = $1"#,
    )
    .bind(id)
    .fetch_one(&state.db)
    .await?;
    let members = members_json(&state, id).await?;

    let budget_json = budget.map_or(Value::Null, |(hours, burned, threshold)| {
        let used_hour = f64::from(burned) / 3600.0;
        json!({
            "totalHour": num(hours),
            "usedHour": num(round2(used_hour)),
            "alertThreshold": threshold,
        })
    });

    Ok(Json(envelope(
        json!({
            "uuid": p_uuid,
            "code": code,
            "name": name,
            "startDate": start_date,
            "totalHourTracked": total_hour,
            "budget": budget_json,
            "members": members,
            "client": pair_json(cl_u, cl_n),
            "company": pair_json(cp_u, cp_n),
            "tasksCount": {
                "total": total_tasks,
                "open": total_tasks - done_tasks,
                "done": done_tasks,
            },
        }),
        None,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pair_json_null_when_missing() {
        assert_eq!(pair_json(None, None), Value::Null);
        assert_eq!(
            pair_json(Some("u".into()), Some("n".into())),
            json!({ "uuid": "u", "name": "n" })
        );
    }
}
