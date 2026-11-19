//! Port `src/mcp-api/trackings/*` (`GET /mcp-api/trackings/summary`, `GET /mcp-api/trackings/recent`).

use std::collections::BTreeMap;

use axum::Extension;
use axum::Json;
use axum::extract::State;
use axum::http::Uri;
use mz_admin_mcp::body::V;
use mz_core::time::{JsDate, v8_date};
use mz_core::{ApiError, ApiResult, AppState};
use serde_json::{Value, json};
use sqlx::{Postgres, QueryBuilder};

use crate::common::{
    envelope, find_company_project_id, find_member_user_id, iso_date10, js_round, num,
    resolve_mcp_caller, round2, secs_between,
};
use crate::dto::{finish, opt_int, opt_uuid, query_of, required_date_not_empty, str_of};
use crate::pat::PatUser;

const MAX_DATE_RANGE_DAYS: f64 = 366.0;
const DAY_MS: i64 = 86_400_000;

fn internal(msg: &str) -> ApiError {
    ApiError::internal(anyhow::anyhow!("{msg}"))
}

type SummaryRow = (JsDate, JsDate, i32, Option<i32>, i32, JsDate);

/// Node: src/mcp-api/trackings/trackings.controller.ts:22 + trackings.service.ts:23 (`GET summary`)
pub async fn h_summary(
    State(state): State<AppState>,
    Extension(user): Extension<PatUser>,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let q = query_of(uri.query());
    // `TrackingSummaryDto`.
    let mut v = V::default();
    required_date_not_empty(&mut v, "dateFrom", q.get("dateFrom"));
    required_date_not_empty(&mut v, "dateTo", q.get("dateTo"));
    opt_uuid(&mut v, &q, "projectUuid");
    opt_uuid(&mut v, &q, "userUuid");
    v.opt_is_in(
        "groupBy",
        q.get("groupBy"),
        &["user", "project", "task", "day"],
    );
    finish(v)?;

    let caller = resolve_mcp_caller(&state.db, user.user_id).await?;
    let company_id = caller.company_id;

    let from = v8_date(str_of(&q, "dateFrom").unwrap_or_default());
    let to = v8_date(str_of(&q, "dateTo").unwrap_or_default());
    if let (Some(f), Some(t)) = (from, to) {
        if f > t {
            return Err(ApiError::bad_request("dateFrom must be <= dateTo"));
        }
        #[allow(clippy::cast_precision_loss)]
        let diff_days =
            (JsDate(t).timestamp_millis() - JsDate(f).timestamp_millis()) as f64 / DAY_MS as f64;
        if diff_days > MAX_DATE_RANGE_DAYS {
            return Err(ApiError::bad_request("Date range max 366 days"));
        }
    }

    let user_uuid = str_of(&q, "userUuid");
    if !caller.is_manager && user_uuid.is_some_and(|u| u != caller.uuid) {
        return Err(ApiError::forbidden(
            "STAFF can only query their own trackings",
        ));
    }

    // Audit C5: user dan project target wajib milik company pemanggil (404 bila bukan).
    let target_user_id: Option<i32> = if let Some(u) = user_uuid {
        Some(find_member_user_id(&state.db, company_id, u).await?)
    } else if !caller.is_manager {
        Some(user.user_id)
    } else {
        None
    };
    let project_id: Option<i32> = match str_of(&q, "projectUuid") {
        Some(p) => Some(find_company_project_id(&state.db, company_id, p).await?),
        None => None,
    };

    // Tanggal tak terparse -> Prisma menolak `Invalid Date` (500).
    let (Some(from), Some(to)) = (from, to) else {
        return Err(internal("Invalid Date"));
    };
    let to_plus = JsDate::from_millis(JsDate(to).timestamp_millis() + DAY_MS)
        .ok_or_else(|| internal("Invalid Date"))?;

    let mut qb = QueryBuilder::<Postgres>::new(
        r#"SELECT tl."startAt", tl."endAt", tl."projectId", tl."taskId", tr."userId",
                  tr."takenDate"::timestamp
             FROM "trackingTimeline" tl
             JOIN tracking tr ON tr.id = tl."trackingId"
            WHERE tl.status = 1 AND tl."startAt" >= "#,
    );
    qb.push_bind(JsDate(from))
        .push(r#" AND tl."endAt" <= "#)
        .push_bind(to_plus)
        .push(
            r#" AND EXISTS (SELECT 1 FROM "projectCompany" pcx WHERE pcx."projectId" = tl."projectId" AND pcx."companyId" = "#,
        )
        .push_bind(company_id)
        .push(")");
    if let Some(pid) = project_id {
        qb.push(r#" AND tl."projectId" = "#).push_bind(pid);
    }
    if let Some(uid) = target_user_id {
        qb.push(r#" AND tr."userId" = "#).push_bind(uid);
    } else {
        qb.push(
            r#" AND EXISTS (SELECT 1 FROM "userCompany" mc WHERE mc."userId" = tr."userId" AND mc."companyId" = "#,
        )
        .push_bind(company_id)
        .push(" AND mc.status = 1)");
    }
    qb.push(" ORDER BY tl.id");
    let rows: Vec<SummaryRow> = qb.build_query_as().fetch_all(&state.db).await?;

    let group_by = str_of(&q, "groupBy").unwrap_or("day");
    if group_by != "day" {
        let data: Vec<Value> = rows
            .into_iter()
            .map(|(start, end, project_id, task_id, user_id, taken)| {
                json!({
                    "startAt": start,
                    "endAt": end,
                    "projectId": project_id,
                    "taskId": task_id,
                    "tracking": { "userId": user_id, "takenDate": taken },
                })
            })
            .collect();
        return Ok(Json(envelope(Value::Array(data), None)));
    }

    // Per hari (`takenDate`), urut menaik.
    let mut by_day: BTreeMap<String, f64> = BTreeMap::new();
    let mut total_seconds = 0.0_f64;
    for (start, end, _, _, _, taken) in &rows {
        let secs = secs_between(start.timestamp_millis(), end.timestamp_millis());
        total_seconds += secs;
        let date = iso_date10(taken.timestamp_millis()).unwrap_or_default();
        *by_day.entry(date).or_insert(0.0) += secs;
    }
    let data: Vec<Value> = by_day
        .into_iter()
        .map(|(date, secs)| {
            json!({ "date": date, "totalSeconds": num(secs), "totalHours": num(round2(secs / 3600.0)) })
        })
        .collect();
    Ok(Json(envelope(
        Value::Array(data),
        Some(json!({
            "totalSeconds": num(js_round(total_seconds)),
            "totalHours": num(round2(total_seconds / 3600.0)),
        })),
    )))
}

type RecentRow = (
    String,
    String,
    JsDate,
    JsDate,
    JsDate,
    Option<String>,
    Option<String>,
    String,
    String,
    Option<String>,
    Option<String>,
);

/// Node: src/mcp-api/trackings/trackings.controller.ts:27 + trackings.service.ts:121 (`GET recent`)
pub async fn h_recent(
    State(state): State<AppState>,
    Extension(user): Extension<PatUser>,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let q = query_of(uri.query());
    // `TrackingRecentDto`.
    let mut v = V::default();
    opt_uuid(&mut v, &q, "userUuid");
    let limit = opt_int(&mut v, "limit", q.get("limit"), 1, Some(100));
    finish(v)?;

    let caller = resolve_mcp_caller(&state.db, user.user_id).await?;
    let user_uuid = str_of(&q, "userUuid");
    if !caller.is_manager && user_uuid.is_some_and(|u| u != caller.uuid) {
        return Err(ApiError::forbidden(
            "STAFF can only query their own trackings",
        ));
    }
    // Audit C5: dulu uuid user tenant mana pun diterima di sini.
    let target_user_id = match user_uuid {
        Some(u) => find_member_user_id(&state.db, caller.company_id, u).await?,
        None => user.user_id,
    };
    let limit = limit.unwrap_or(20);

    let rows: Vec<RecentRow> = sqlx::query_as(
        r#"SELECT tl.uuid::text, tl.note, tl."startAt", tl."endAt", tr."takenDate"::timestamp,
                  u.uuid::text, u.name, p.uuid::text, p.name, t.uuid::text, t.name
             FROM "trackingTimeline" tl
             JOIN tracking tr ON tr.id = tl."trackingId"
             LEFT JOIN users u ON u.id = tr."userId"
             JOIN project p ON p.id = tl."projectId"
             LEFT JOIN task t ON t.id = tl."taskId"
            WHERE tl.status = 1 AND tr."userId" = $1
              AND EXISTS (SELECT 1 FROM "projectCompany" pcx
                           WHERE pcx."projectId" = tl."projectId" AND pcx."companyId" = $2)
            ORDER BY tl."startAt" DESC, tl.id DESC
            LIMIT $3"#,
    )
    .bind(target_user_id)
    .bind(caller.company_id)
    .bind(limit)
    .fetch_all(&state.db)
    .await?;

    let data: Vec<Value> = rows
        .into_iter()
        .map(
            |(uuid, note, start, end, taken, u_uuid, u_name, p_uuid, p_name, t_uuid, t_name)| {
                let secs = secs_between(start.timestamp_millis(), end.timestamp_millis());
                json!({
                    "uuid": uuid,
                    "takenDate": iso_date10(taken.timestamp_millis()),
                    "startAt": start,
                    "endAt": end,
                    "durationSeconds": num(js_round(secs)),
                    "note": note,
                    "project": { "uuid": p_uuid, "name": p_name },
                    "task": match (t_uuid, t_name) {
                        (Some(u), Some(n)) => json!({ "uuid": u, "name": n }),
                        _ => Value::Null,
                    },
                    "user": match (u_uuid, u_name) {
                        (Some(u), Some(n)) => json!({ "uuid": u, "name": n }),
                        _ => Value::Null,
                    },
                })
            },
        )
        .collect();
    Ok(Json(envelope(Value::Array(data), None)))
}
