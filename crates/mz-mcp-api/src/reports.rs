//! Port `src/mcp-api/reports/*` (`GET /mcp-api/reports/project/:uuid`, `GET /mcp-api/reports/team`).
//!
//! Hasil di-cache 5 menit di Redis lewat kunci `cacheManager` Node yang SAMA
//! (`mcp:report:project:...`, `mcp:report:team:...`; nilai = JSON string, TTL 300000 ms).
//!
//! Urutan koleksi meniru objek JS: kunci bilangan bulat (`userId`, `taskId`, `projectId`) beriterasi
//! menaik (BTreeMap), `__none__` (tanpa task) paling akhir.

use std::collections::{BTreeMap, BTreeSet};

use axum::Extension;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::Uri;
use fred::prelude::*;
use fred::types::Expiration;
use mz_admin_mcp::body::V;
use mz_core::time::{JsDate, now_naive, v8_date};
use mz_core::uuid_rules::parse_uuid_pipe;
use mz_core::{ApiError, ApiResult, AppState};
use serde_json::{Value, json};
use sqlx::{Postgres, QueryBuilder};

use crate::common::{
    envelope, iso_date10, js_round, num, resolve_mcp_caller, round2, secs_between,
};
use crate::dto::{date_string, finish, opt_date_string, query_of, str_of};
use crate::pat::PatUser;

const CACHE_TTL_MS: i64 = 5 * 60 * 1000;
const DAY_MS: i64 = 86_400_000;

fn internal(msg: &str) -> ApiError {
    ApiError::internal(anyhow::anyhow!("{msg}"))
}

async fn cache_get(state: &AppState, key: &str) -> ApiResult<Option<Value>> {
    let raw: Option<String> = state.redis.next().get(key).await?;
    match raw {
        Some(r) => Ok(Some(serde_json::from_str(&r).map_err(ApiError::internal)?)),
        None => Ok(None),
    }
}

async fn cache_set(state: &AppState, key: &str, value: &Value) -> ApiResult<()> {
    let raw = serde_json::to_string(value).map_err(ApiError::internal)?;
    state
        .redis
        .next()
        .set::<(), _, _>(key, raw, Some(Expiration::PX(CACHE_TTL_MS)), None, false)
        .await?;
    Ok(())
}

/// Kunci cache laporan project (Node: reports.service.ts:41).
pub fn project_cache_key(
    user_id: i32,
    company_id: i32,
    elevated: bool,
    project_uuid: &str,
    from10: &str,
    to10: &str,
) -> String {
    format!(
        "mcp:report:project:{user_id}:{company_id}:{}:{project_uuid}:{from10}:{to10}",
        if elevated { 'm' } else { 's' }
    )
}

/// Kunci cache laporan tim (Node: reports.service.ts:197). `uuids` diurut seperti `Array.sort()`.
pub fn team_cache_key(
    user_id: i32,
    company_id: i32,
    from10: &str,
    to10: &str,
    uuids: Option<&[String]>,
) -> String {
    let uuids_key = match uuids {
        Some(u) => {
            let mut s: Vec<&str> = u.iter().map(String::as_str).collect();
            // `Array.prototype.sort()` default = urutan unit kode UTF-16.
            s.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
            s.join(",")
        }
        None => "all".to_owned(),
    };
    format!("mcp:report:team:{user_id}:{company_id}:{from10}:{to10}:{uuids_key}")
}

type ProjectTimelineRow = (
    JsDate,
    JsDate,
    Option<bool>,
    Option<i32>,
    Option<String>,
    Option<String>,
    Option<JsDate>,
    Option<i32>,
    Option<String>,
    Option<String>,
);

struct UserAgg {
    uuid: String,
    name: String,
    seconds: f64,
    entries: i64,
}

struct TaskAgg {
    task: Value,
    seconds: f64,
}

/// Node: src/mcp-api/reports/reports.controller.ts:23 + reports.service.ts:27 (`GET project/:uuid`)
pub async fn h_project(
    State(state): State<AppState>,
    Extension(user): Extension<PatUser>,
    Path(raw): Path<String>,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    parse_uuid_pipe(&raw)?;
    let q = query_of(uri.query());
    // `ProjectReportDto`.
    let mut v = V::default();
    opt_date_string(&mut v, "dateFrom", q.get("dateFrom"));
    opt_date_string(&mut v, "dateTo", q.get("dateTo"));
    finish(v)?;

    let caller = resolve_mcp_caller(&state.db, user.user_id).await?;
    let company_id = caller.company_id;

    let now = JsDate(now_naive());
    let default_from = now.timestamp_millis() - 30 * DAY_MS;
    let from_ms = match str_of(&q, "dateFrom") {
        Some(s) => v8_date(s).map(|d| JsDate(d).timestamp_millis()),
        None => Some(default_from),
    };
    let to_ms = match str_of(&q, "dateTo") {
        Some(s) => v8_date(s).map(|d| JsDate(d).timestamp_millis()),
        None => Some(now.timestamp_millis()),
    };
    // `toISOString()` pada Invalid Date -> RangeError -> 500.
    let (Some(from_ms), Some(to_ms)) = (from_ms, to_ms) else {
        return Err(internal("Invalid time value"));
    };
    let (Some(from10), Some(to10)) = (iso_date10(from_ms), iso_date10(to_ms)) else {
        return Err(internal("Invalid time value"));
    };

    let cache_key = project_cache_key(
        user.user_id,
        company_id,
        caller.is_manager,
        &raw,
        &from10,
        &to10,
    );
    if let Some(cached) = cache_get(&state, &cache_key).await? {
        return Ok(Json(cached));
    }

    // Audit C5: project wajib terdaftar di company pemanggil, untuk semua role.
    let project: Option<(i32, String, String, String)> = sqlx::query_as(
        r#"SELECT p.id, p.uuid::text, p.code, p.name
             FROM project p
            WHERE p.uuid = $1::uuid
              AND EXISTS (SELECT 1 FROM "projectCompany" pcx
                           WHERE pcx."projectId" = p.id AND pcx."companyId" = $2)
            LIMIT 1"#,
    )
    .bind(&raw)
    .bind(company_id)
    .fetch_optional(&state.db)
    .await?;
    let Some((project_id, p_uuid, p_code, p_name)) = project else {
        return Err(ApiError::not_found("Project not found"));
    };

    if !caller.is_manager {
        let is_member: Option<i32> = sqlx::query_scalar(
            r#"SELECT id FROM "projectUser" WHERE "projectId" = $1 AND "userId" = $2 AND status = 1 LIMIT 1"#,
        )
        .bind(project_id)
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
    .bind(project_id)
    .fetch_optional(&state.db)
    .await?;

    let to_plus =
        JsDate::from_millis(to_ms + DAY_MS).ok_or_else(|| internal("Invalid time value"))?;
    let from_date = JsDate::from_millis(from_ms).ok_or_else(|| internal("Invalid time value"))?;
    let rows: Vec<ProjectTimelineRow> = sqlx::query_as(
        r#"SELECT tl."startAt", tl."endAt", tl.billable, tl."taskId", t.uuid::text, t.name,
                  tr."takenDate"::timestamp, u.id, u.uuid::text, u.name
             FROM "trackingTimeline" tl
             LEFT JOIN task t ON t.id = tl."taskId"
             LEFT JOIN tracking tr ON tr.id = tl."trackingId"
             LEFT JOIN users u ON u.id = tr."userId"
            WHERE tl."projectId" = $1 AND tl.status = 1 AND tl."startAt" >= $2 AND tl."endAt" <= $3
            ORDER BY tl.id"#,
    )
    .bind(project_id)
    .bind(from_date)
    .bind(to_plus)
    .fetch_all(&state.db)
    .await?;

    let mut total_seconds = 0.0_f64;
    let mut billable_seconds = 0.0_f64;
    let mut by_user: BTreeMap<i32, UserAgg> = BTreeMap::new();
    let mut by_task: BTreeMap<i32, TaskAgg> = BTreeMap::new();
    let mut none_task: Option<TaskAgg> = None;
    let mut by_day: BTreeMap<String, f64> = BTreeMap::new();
    let mut contributors: BTreeSet<i32> = BTreeSet::new();

    for (start, end, billable, task_id, t_uuid, t_name, taken, uid, u_uuid, u_name) in &rows {
        let secs = secs_between(start.timestamp_millis(), end.timestamp_millis());
        total_seconds += secs;
        if *billable != Some(false) {
            billable_seconds += secs;
        }

        if let Some(uid) = uid.filter(|u| *u != 0) {
            contributors.insert(uid);
            let agg = by_user.entry(uid).or_insert_with(|| UserAgg {
                uuid: u_uuid.clone().unwrap_or_default(),
                name: u_name.clone().unwrap_or_default(),
                seconds: 0.0,
                entries: 0,
            });
            agg.seconds += secs;
            agg.entries += 1;
        }

        let task_json = match (t_uuid, t_name) {
            (Some(u), Some(n)) => json!({ "uuid": u, "name": n }),
            _ => Value::Null,
        };
        let entry = match task_id.filter(|t| *t != 0) {
            Some(tid) => by_task.entry(tid).or_insert_with(|| TaskAgg {
                task: task_json,
                seconds: 0.0,
            }),
            None => none_task.get_or_insert_with(|| TaskAgg {
                task: task_json,
                seconds: 0.0,
            }),
        };
        entry.seconds += secs;

        let date = match taken {
            Some(t) => iso_date10(t.timestamp_millis()),
            None => iso_date10(start.timestamp_millis()),
        }
        .unwrap_or_default();
        *by_day.entry(date).or_insert(0.0) += secs;
    }

    let used_hour = match budget {
        Some((_, burned, _)) => f64::from(burned) / 3600.0,
        None => total_seconds / 3600.0,
    };
    let budget_json = budget.map_or(Value::Null, |(hours, _, _)| {
        json!({
            "totalHour": num(hours),
            "usedHour": num(round2(used_hour)),
            "remainingHour": num(round2(hours - used_hour)),
            "usagePercent": num(js_round(used_hour / hours * 100.0)),
        })
    });

    let by_user_json: Vec<Value> = by_user
        .into_values()
        .map(|u| {
            json!({
                "user": { "uuid": u.uuid, "name": u.name },
                "hours": num(round2(u.seconds / 3600.0)),
                "entries": u.entries,
            })
        })
        .collect();
    let by_task_json: Vec<Value> = by_task
        .into_values()
        .chain(none_task)
        .map(|t| json!({ "task": t.task, "hours": num(round2(t.seconds / 3600.0)) }))
        .collect();
    let by_day_json: Vec<Value> = by_day
        .into_iter()
        .map(|(date, secs)| json!({ "date": date, "hours": num(round2(secs / 3600.0)) }))
        .collect();

    let result = envelope(
        json!({
            "project": { "uuid": p_uuid, "name": p_name, "code": p_code },
            "period": { "from": from10, "to": to10 },
            "summary": {
                "totalHours": num(round2(total_seconds / 3600.0)),
                "totalEntries": rows.len(),
                "uniqueContributors": contributors.len(),
                "billableHours": num(round2(billable_seconds / 3600.0)),
            },
            "budget": budget_json,
            "byUser": by_user_json,
            "byTask": by_task_json,
            "byDay": by_day_json,
        }),
        None,
    );
    cache_set(&state, &cache_key, &result).await?;
    Ok(Json(result))
}

/// `@IsOptional() @IsUUID(undefined, { each: true })`: hanya array yang divalidasi per elemen
/// (class-validator mengabaikan `each` pada nilai non-array).
fn validate_user_uuids(v: &mut V, val: Option<&Value>) {
    if let Some(Value::Array(items)) = val {
        let ok = items
            .iter()
            .all(|i| matches!(i, Value::String(s) if mz_core::uuid_rules::is_uuid(s)));
        if !ok {
            v.errors
                .push("each value in userUuids must be a UUID".to_owned());
        }
    }
}

type TeamRow = (
    JsDate,
    JsDate,
    i32,
    Option<String>,
    Option<i32>,
    Option<String>,
    Option<String>,
);

struct TeamUser {
    uuid: String,
    name: String,
    total: f64,
    projects: BTreeMap<i32, (String, f64)>,
}

/// Node: src/mcp-api/reports/reports.controller.ts:32 + reports.service.ts:190 (`GET team`, MANAGER+)
pub async fn h_team(
    State(state): State<AppState>,
    Extension(user): Extension<PatUser>,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let q = query_of(uri.query());
    // `TeamReportDto`.
    let mut v = V::default();
    date_string(&mut v, "dateFrom", q.get("dateFrom"));
    date_string(&mut v, "dateTo", q.get("dateTo"));
    if q.get("userUuids").is_some_and(|x| !x.is_null()) {
        validate_user_uuids(&mut v, q.get("userUuids"));
    }
    finish(v)?;

    let caller = resolve_mcp_caller(&state.db, user.user_id).await?;
    if !caller.is_manager {
        return Err(ApiError::forbidden("MANAGER+ role required"));
    }
    let company_id = caller.company_id;

    let from_ms =
        v8_date(str_of(&q, "dateFrom").unwrap_or_default()).map(|d| JsDate(d).timestamp_millis());
    let to_ms =
        v8_date(str_of(&q, "dateTo").unwrap_or_default()).map(|d| JsDate(d).timestamp_millis());
    let (Some(from_ms), Some(to_ms)) = (from_ms, to_ms) else {
        return Err(internal("Invalid time value"));
    };
    let (Some(from10), Some(to10)) = (iso_date10(from_ms), iso_date10(to_ms)) else {
        return Err(internal("Invalid time value"));
    };

    // `userUuids` non-array (string/objek): Node memecah string per karakter lalu Prisma menolak
    // `in: <string>` -> 500.
    let user_uuids: Option<Vec<String>> = match q.get("userUuids") {
        None | Some(Value::Null) => None,
        Some(Value::Array(items)) => Some(
            items
                .iter()
                .filter_map(|i| i.as_str().map(str::to_owned))
                .collect(),
        ),
        Some(_) => return Err(internal("Invalid `in` filter")),
    };

    let cache_key = team_cache_key(
        user.user_id,
        company_id,
        &from10,
        &to10,
        user_uuids.as_deref(),
    );
    if let Some(cached) = cache_get(&state, &cache_key).await? {
        return Ok(Json(cached));
    }

    // Audit C5: uuid di luar company pemanggil diabaikan diam-diam.
    let target_ids: Option<Vec<i32>> = match user_uuids.as_deref() {
        Some(list) if !list.is_empty() => {
            let ids: Vec<i32> = sqlx::query_scalar(
                r#"SELECT u.id FROM users u
                    WHERE u.uuid = ANY($1::uuid[])
                      AND EXISTS (SELECT 1 FROM "userCompany" mc
                                   WHERE mc."userId" = u.id AND mc."companyId" = $2 AND mc.status = 1)"#,
            )
            .bind(list)
            .bind(company_id)
            .fetch_all(&state.db)
            .await?;
            Some(ids)
        }
        _ => None,
    };

    let to_plus =
        JsDate::from_millis(to_ms + DAY_MS).ok_or_else(|| internal("Invalid time value"))?;
    let from_date = JsDate::from_millis(from_ms).ok_or_else(|| internal("Invalid time value"))?;
    let mut qb = QueryBuilder::<Postgres>::new(
        r#"SELECT tl."startAt", tl."endAt", tl."projectId", p.name, u.id, u.uuid::text, u.name
             FROM "trackingTimeline" tl
             JOIN tracking tr ON tr.id = tl."trackingId"
             LEFT JOIN users u ON u.id = tr."userId"
             LEFT JOIN project p ON p.id = tl."projectId"
            WHERE tl.status = 1 AND tl."startAt" >= "#,
    );
    qb.push_bind(from_date)
        .push(r#" AND tl."endAt" <= "#)
        .push_bind(to_plus)
        .push(
            r#" AND EXISTS (SELECT 1 FROM "projectCompany" pcx WHERE pcx."projectId" = tl."projectId" AND pcx."companyId" = "#,
        )
        .push_bind(company_id)
        .push(")");
    match &target_ids {
        Some(ids) => {
            qb.push(r#" AND tr."userId" = ANY("#)
                .push_bind(ids.clone())
                .push(")");
        }
        None => {
            qb.push(
                r#" AND EXISTS (SELECT 1 FROM "userCompany" mc WHERE mc."userId" = tr."userId" AND mc."companyId" = "#,
            )
            .push_bind(company_id)
            .push(" AND mc.status = 1)");
        }
    }
    qb.push(" ORDER BY tl.id");
    let rows: Vec<TeamRow> = qb.build_query_as().fetch_all(&state.db).await?;

    let mut by_user: BTreeMap<i32, TeamUser> = BTreeMap::new();
    for (start, end, project_id, p_name, uid, u_uuid, u_name) in rows {
        let Some(uid) = uid.filter(|u| *u != 0) else {
            continue;
        };
        let secs = secs_between(start.timestamp_millis(), end.timestamp_millis());
        let agg = by_user.entry(uid).or_insert_with(|| TeamUser {
            uuid: u_uuid.unwrap_or_default(),
            name: u_name.unwrap_or_default(),
            total: 0.0,
            projects: BTreeMap::new(),
        });
        agg.total += secs;
        let p = agg
            .projects
            .entry(project_id)
            .or_insert_with(|| (p_name.unwrap_or_default(), 0.0));
        p.1 += secs;
    }

    let users: Vec<Value> = by_user
        .into_values()
        .map(|u| {
            let projects_count = u.projects.len();
            let mut tops: Vec<(String, f64)> = u.projects.into_values().collect();
            // Urutan stabil `sort((a, b) => b.seconds - a.seconds)`.
            tops.sort_by(|a, b| b.1.total_cmp(&a.1));
            let top_projects: Vec<Value> = tops
                .into_iter()
                .take(5)
                .map(|(name, secs)| json!({ "name": name, "hours": num(round2(secs / 3600.0)) }))
                .collect();
            json!({
                "user": { "uuid": u.uuid, "name": u.name },
                "totalHours": num(round2(u.total / 3600.0)),
                "projectsCount": projects_count,
                "topProjects": top_projects,
            })
        })
        .collect();

    let result = envelope(
        json!({
            "period": { "from": from10, "to": to10 },
            "users": users,
        }),
        None,
    );
    cache_set(&state, &cache_key, &result).await?;
    Ok(Json(result))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_keys_match_node() {
        assert_eq!(
            project_cache_key(7, 3, true, "uuid-x", "2026-11-01", "2026-11-17"),
            "mcp:report:project:7:3:m:uuid-x:2026-11-01:2026-11-17"
        );
        assert_eq!(
            project_cache_key(7, 3, false, "u", "a", "b"),
            "mcp:report:project:7:3:s:u:a:b"
        );
        assert_eq!(
            team_cache_key(1, 2, "a", "b", None),
            "mcp:report:team:1:2:a:b:all"
        );
        assert_eq!(
            team_cache_key(1, 2, "a", "b", Some(&["z".into(), "B".into(), "a".into()])),
            "mcp:report:team:1:2:a:b:B,a,z"
        );
        assert_eq!(
            team_cache_key(1, 2, "a", "b", Some(&[])),
            "mcp:report:team:1:2:a:b:"
        );
    }

    #[test]
    fn user_uuids_validation() {
        let mut v = V::default();
        validate_user_uuids(
            &mut v,
            Some(&json!(["3f2c1e2a-1b2c-4d3e-8f4a-5b6c7d8e9f01"])),
        );
        assert!(v.errors.is_empty());
        validate_user_uuids(&mut v, Some(&json!(["nope"])));
        assert_eq!(v.errors, ["each value in userUuids must be a UUID"]);
        // non-array: dilewati
        let mut v = V::default();
        validate_user_uuids(&mut v, Some(&json!("nope")));
        assert!(v.errors.is_empty());
    }
}
