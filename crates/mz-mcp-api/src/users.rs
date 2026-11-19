//! Port `src/mcp-api/users/*` (`GET /mcp-api/users/me`, `GET /mcp-api/users`).

use std::collections::HashMap;

use axum::Extension;
use axum::Json;
use axum::extract::State;
use axum::http::Uri;
use mz_admin_mcp::body::V;
use mz_core::enums::{BaseRole, effective_role_code};
use mz_core::{ApiError, ApiResult, AppState};
use serde_json::{Value, json};
use sqlx::{Postgres, QueryBuilder};

use crate::common::{envelope, resolve_mcp_caller, skip_of};
use crate::dto::{finish, pagination, query_of, truthy_str};
use crate::pat::PatUser;
use mz_core::roles::built_in_base_role_of;

/// Node: src/mcp-api/users/users.controller.ts:20 (`GET me`)
pub async fn h_me(
    State(state): State<AppState>,
    Extension(user): Extension<PatUser>,
) -> ApiResult<Json<Value>> {
    let caller = resolve_mcp_caller(&state.db, user.user_id).await?;
    Ok(Json(envelope(
        json!({
            "uuid": caller.uuid,
            "name": caller.name,
            "username": caller.username,
            "roles": caller.roles,
            "company": caller.company_json(),
        }),
        None,
    )))
}

const ROLE_CODES: [&str; 6] = [
    "SYSTEM_ADMIN",
    "SUPER_ADMIN",
    "ADMIN",
    "MANAGER",
    "STAFF",
    "GUEST",
];

/// `roleCodeFilter([code])` sebagai potongan SQL atas alias `al` (`accessLevel`): baris bawaan
/// ber-`code` itu ATAU role custom yang `baseRole`-nya padanan `code`. `status` tidak disaring.
pub fn push_role_code_filter(qb: &mut QueryBuilder<'_, Postgres>, code: &str) {
    let base: Vec<String> = built_in_base_role_of(code)
        .map(|b| vec![b.to_owned()])
        .unwrap_or_default();
    qb.push(r#"((al."isSystem" AND al.code = "#)
        .push_bind(code.to_owned())
        .push(r#") OR (NOT al."isSystem" AND al."baseRole"::text = ANY("#)
        .push_bind(base)
        .push(")))");
}

/// Node: src/mcp-api/users/users.controller.ts:26 + users.service.ts:28 (`GET`, MANAGER+)
pub async fn h_list(
    State(state): State<AppState>,
    Extension(user): Extension<PatUser>,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let q = query_of(uri.query());
    // `ListUsersDto`: keyword, role, lalu `PaginationDto` (page, perPage).
    let mut v = V::default();
    v.opt_string("keyword", q.get("keyword"));
    v.opt_is_in("role", q.get("role"), &ROLE_CODES);
    let (page, per_page) = pagination(&mut v, &q, 500, 500);
    finish(v)?;

    let caller = resolve_mcp_caller(&state.db, user.user_id).await?;
    if !caller.is_manager {
        return Err(ApiError::forbidden("MANAGER+ role required"));
    }
    let company_id = caller.company_id;
    let skip = skip_of(page, per_page)?;

    // Selalu dibatasi ke company pemanggil (audit C5).
    let push_where = |qb: &mut QueryBuilder<'_, Postgres>| {
        qb.push(
            r#" WHERE u.status = 1 AND EXISTS (SELECT 1 FROM "userCompany" mc WHERE mc."userId" = u.id AND mc."companyId" = "#,
        )
        .push_bind(company_id)
        .push(" AND mc.status = 1)");
        if let Some(kw) = truthy_str(&q, "keyword") {
            let pattern = format!("%{kw}%");
            qb.push(" AND (u.name ILIKE ")
                .push_bind(pattern.clone())
                .push(" OR u.username ILIKE ")
                .push_bind(pattern)
                .push(")");
        }
        if let Some(role) = truthy_str(&q, "role") {
            qb.push(
                r#" AND EXISTS (SELECT 1 FROM "userAccessLevel" ual JOIN "accessLevel" al ON al.id = ual."accessLevelId"
                    WHERE ual."userId" = u.id AND ual.status = 1 AND ual."companyId" = "#,
            )
            .push_bind(company_id)
            .push(" AND ");
            push_role_code_filter(qb, role);
            qb.push(")");
        }
    };

    let mut count = QueryBuilder::<Postgres>::new("SELECT count(*) FROM users u");
    push_where(&mut count);
    let total: i64 = count.build_query_scalar().fetch_one(&state.db).await?;

    let mut list =
        QueryBuilder::<Postgres>::new("SELECT u.id, u.uuid::text, u.name, u.username FROM users u");
    push_where(&mut list);
    list.push(" ORDER BY u.name ASC, u.id ASC LIMIT ")
        .push_bind(per_page)
        .push(" OFFSET ")
        .push_bind(skip);
    let items: Vec<(i32, String, String, String)> =
        list.build_query_as().fetch_all(&state.db).await?;

    let ids: Vec<i32> = items.iter().map(|r| r.0).collect();
    let role_rows: Vec<(i32, String, bool, Option<String>)> = if ids.is_empty() {
        Vec::new()
    } else {
        sqlx::query_as(
            r#"SELECT ual."userId", al.code, al."isSystem", al."baseRole"::text
                 FROM "userAccessLevel" ual
                 JOIN "accessLevel" al ON al.id = ual."accessLevelId"
                WHERE ual."userId" = ANY($1) AND ual.status = 1 AND ual."companyId" = $2
                ORDER BY ual.id"#,
        )
        .bind(&ids)
        .bind(company_id)
        .fetch_all(&state.db)
        .await?
    };
    let mut roles_by_user: HashMap<i32, Vec<String>> = HashMap::new();
    for (uid, code, is_system, base) in role_rows {
        roles_by_user
            .entry(uid)
            .or_default()
            .push(effective_role_code(
                &code,
                is_system,
                base.and_then(|b| b.parse::<BaseRole>().ok()),
            ));
    }

    let data: Vec<Value> = items
        .into_iter()
        .map(|(id, uuid, name, username)| {
            json!({
                "uuid": uuid,
                "name": name,
                "username": username,
                "roles": roles_by_user.remove(&id).unwrap_or_default(),
            })
        })
        .collect();
    Ok(Json(envelope(
        Value::Array(data),
        Some(json!({ "page": page, "perPage": per_page, "total": total })),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_filter_sql_shape() {
        let mut qb = QueryBuilder::<Postgres>::new("SELECT 1 FROM x al WHERE ");
        push_role_code_filter(&mut qb, "MANAGER");
        let sql = qb.sql().to_string();
        assert!(sql.contains(r#"al."isSystem" AND al.code = $1"#));
        assert!(sql.contains(r#"NOT al."isSystem" AND al."baseRole"::text = ANY($2)"#));
    }

    #[test]
    fn base_role_mapping() {
        assert_eq!(built_in_base_role_of("ADMIN"), Some("ADMIN"));
        assert_eq!(built_in_base_role_of("SUPER_ADMIN"), None);
        assert_eq!(built_in_base_role_of("GUEST"), None);
    }
}
