//! Port `src/mcp-api/tasks/*` (`GET /mcp-api/tasks`).

use axum::Extension;
use axum::Json;
use axum::extract::State;
use axum::http::Uri;
use mz_admin_mcp::body::V;
use mz_core::time::JsDate;
use mz_core::{ApiResult, AppState};
use serde_json::{Value, json};
use sqlx::{Postgres, QueryBuilder};

use crate::common::{envelope, find_company_project_id, resolve_mcp_caller, skip_of};
use crate::dto::{finish, opt_uuid, pagination, query_of, str_of, truthy_str};
use crate::pat::PatUser;

type TaskRow = (
    String,
    String,
    Option<String>,
    Option<i32>,
    Option<JsDate>,
    String,
    Option<String>,
    String,
    String,
);

/// Node: src/mcp-api/tasks/tasks.controller.ts:21 + tasks.service.ts:14 (`GET`)
pub async fn h_list(
    State(state): State<AppState>,
    Extension(user): Extension<PatUser>,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let q = query_of(uri.query());
    // `ListTasksDto`: projectUuid, assigneeUuid, status, keyword, lalu `PaginationDto`.
    let mut v = V::default();
    opt_uuid(&mut v, &q, "projectUuid");
    opt_uuid(&mut v, &q, "assigneeUuid");
    v.opt_string("status", q.get("status"));
    v.opt_string("keyword", q.get("keyword"));
    let (page, per_page) = pagination(&mut v, &q, 500, 500);
    finish(v)?;

    let caller = resolve_mcp_caller(&state.db, user.user_id).await?;
    let company_id = caller.company_id;

    let project_id: Option<i32> = match str_of(&q, "projectUuid") {
        Some(p) => Some(find_company_project_id(&state.db, company_id, p).await?),
        None => None,
    };

    let push_where = |qb: &mut QueryBuilder<'_, Postgres>| {
        qb.push(r#" WHERE t."companyId" = "#).push_bind(company_id);
        if !caller.is_manager {
            qb.push(
                r#" AND EXISTS (SELECT 1 FROM "projectUser" pu WHERE pu."projectId" = t."projectId" AND pu."userId" = "#,
            )
            .push_bind(user.user_id)
            .push(" AND pu.status = 1)");
        }
        if let Some(pid) = project_id {
            qb.push(r#" AND t."projectId" = "#).push_bind(pid);
        }
        if let Some(s) = truthy_str(&q, "status") {
            qb.push(" AND t.status ILIKE ").push_bind(format!("%{s}%"));
        }
        if let Some(k) = truthy_str(&q, "keyword") {
            qb.push(" AND t.name ILIKE ").push_bind(format!("%{k}%"));
        }
    };

    let mut count = QueryBuilder::<Postgres>::new("SELECT count(*) FROM task t");
    push_where(&mut count);
    let total: i64 = count.build_query_scalar().fetch_one(&state.db).await?;

    let skip = skip_of(page, per_page)?;
    let mut list = QueryBuilder::<Postgres>::new(
        r#"SELECT t.uuid::text, t.name, t.status, t.priority, t."dueDate", t.source, t."externalUrl",
                  p.uuid::text, p.name
             FROM task t JOIN project p ON p.id = t."projectId""#,
    );
    push_where(&mut list);
    list.push(r#" ORDER BY t."createdAt" DESC, t.id DESC LIMIT "#)
        .push_bind(per_page)
        .push(" OFFSET ")
        .push_bind(skip);
    let rows: Vec<TaskRow> = list.build_query_as().fetch_all(&state.db).await?;

    let data: Vec<Value> = rows
        .into_iter()
        .map(
            |(uuid, name, status, priority, due, source, external_url, p_uuid, p_name)| {
                json!({
                    "uuid": uuid,
                    "name": name,
                    "status": status,
                    "priority": priority,
                    "dueDate": due,
                    "source": source,
                    "externalUrl": external_url,
                    "project": { "uuid": p_uuid, "name": p_name },
                })
            },
        )
        .collect();
    Ok(Json(envelope(
        Value::Array(data),
        Some(json!({ "page": page, "perPage": per_page, "total": total })),
    )))
}
