//! Port `src/mcp-api/audit-logs/*` (`GET /mcp-api/audit-logs`, `JwtAuthGuard`,
//! `@PermissionExempt('self-scoped')`): log pemakaian PAT milik pemanggil (BUKAN audit log company).
//! Log pemakaian token OAuth ditulis ke tabel lain dan tidak dilist di sini (sama dengan Node).

use axum::Json;
use axum::extract::State;
use axum::http::Uri;
use mz_admin_mcp::body::V;
use mz_core::gate::RequestCtx;
use mz_core::time::{JsDate, v8_date};
use mz_core::{ApiError, ApiResult, AppState};
use serde_json::{Value, json};
use sqlx::{Postgres, QueryBuilder};

use crate::common::{envelope, skip_of};
use crate::dto::{finish, opt_date_string, opt_uuid, pagination, query_of, str_of};

const DAY_MS: i64 = 86_400_000;

type LogRow = (
    String,
    String,
    String,
    Option<i32>,
    Option<i32>,
    JsDate,
    String,
    String,
    String,
);

/// Node: src/mcp-api/audit-logs/audit-logs.controller.ts:11 + audit-logs.service.ts:39 (`GET`)
pub async fn h_list(
    State(state): State<AppState>,
    ctx: RequestCtx,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    let q = query_of(uri.query());
    // `ListAuditLogsDto`: tokenUuid, dateFrom, dateTo, page (min 1), perPage (1..200, default 50).
    let mut v = V::default();
    opt_uuid(&mut v, &q, "tokenUuid");
    opt_date_string(&mut v, "dateFrom", q.get("dateFrom"));
    opt_date_string(&mut v, "dateTo", q.get("dateTo"));
    let (page, per_page) = pagination(&mut v, &q, 50, 200);
    finish(v)?;

    let mut token_id: Option<i32> = None;
    if let Some(tu) = str_of(&q, "tokenUuid").filter(|s| !s.is_empty()) {
        let found: Option<i32> = sqlx::query_scalar(
            r#"SELECT id FROM "mcpAccessToken" WHERE uuid = $1::uuid AND "userId" = $2 LIMIT 1"#,
        )
        .bind(tu)
        .bind(user_id)
        .fetch_optional(&state.db)
        .await?;
        match found {
            Some(id) => token_id = Some(id),
            None => {
                return Ok(Json(envelope(
                    Value::Array(Vec::new()),
                    Some(json!({ "page": page, "perPage": per_page, "total": 0 })),
                )));
            }
        }
    }

    // `new Date(...)` tak terparse -> Prisma menolak `Invalid Date` (500).
    let invalid = || ApiError::internal(anyhow::anyhow!("Invalid Date"));
    let date_from: Option<JsDate> = match str_of(&q, "dateFrom").filter(|s| !s.is_empty()) {
        Some(s) => Some(JsDate(v8_date(s).ok_or_else(invalid)?)),
        None => None,
    };
    let date_to: Option<JsDate> = match str_of(&q, "dateTo").filter(|s| !s.is_empty()) {
        Some(s) => {
            let d = v8_date(s).ok_or_else(invalid)?;
            Some(JsDate::from_millis(JsDate(d).timestamp_millis() + DAY_MS).ok_or_else(invalid)?)
        }
        None => None,
    };

    let push_where = |qb: &mut QueryBuilder<'_, Postgres>| {
        qb.push(r#" WHERE l."userId" = "#).push_bind(user_id);
        if let Some(t) = token_id {
            qb.push(r#" AND l."tokenId" = "#).push_bind(t);
        }
        if let Some(f) = date_from {
            qb.push(r#" AND l."createdAt" >= "#).push_bind(f);
        }
        if let Some(t) = date_to {
            qb.push(r#" AND l."createdAt" <= "#).push_bind(t);
        }
    };

    let mut count = QueryBuilder::<Postgres>::new(r#"SELECT count(*) FROM "mcpAccessTokenLog" l"#);
    push_where(&mut count);
    let total: i64 = count.build_query_scalar().fetch_one(&state.db).await?;

    let skip = skip_of(page, per_page)?;
    let mut list = QueryBuilder::<Postgres>::new(
        r#"SELECT l.uuid::text, l.method, l.path, l."statusCode", l."durationMs", l."createdAt",
                  t.uuid::text, t.name, t."tokenPrefix"
             FROM "mcpAccessTokenLog" l
             JOIN "mcpAccessToken" t ON t.id = l."tokenId""#,
    );
    push_where(&mut list);
    list.push(r#" ORDER BY l."createdAt" DESC, l.id DESC LIMIT "#)
        .push_bind(per_page)
        .push(" OFFSET ")
        .push_bind(skip);
    let rows: Vec<LogRow> = list.build_query_as().fetch_all(&state.db).await?;

    let data: Vec<Value> = rows
        .into_iter()
        .map(
            |(uuid, method, path, status, duration, created, t_uuid, t_name, t_prefix)| {
                json!({
                    "uuid": uuid,
                    "method": method,
                    "path": path,
                    "statusCode": status,
                    "durationMs": duration,
                    "createdAt": created,
                    "token": { "uuid": t_uuid, "name": t_name, "tokenPrefix": t_prefix },
                })
            },
        )
        .collect();
    Ok(Json(envelope(
        Value::Array(data),
        Some(json!({ "page": page, "perPage": per_page, "total": total })),
    )))
}
