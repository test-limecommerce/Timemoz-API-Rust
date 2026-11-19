//! Port `src/reports/reports.service.ts` (tanpa cron). Query Prisma -> SQL mentah; bentuk JSON mengikuti Node.

use std::collections::HashMap;

use axum::http::StatusCode;
use chrono::{Duration, NaiveDateTime};
use mz_core::enums::sees_whole_company;
use mz_core::time::{JsDate, now_naive, parse_js_date, timeline_duration_seconds};
use mz_core::{ApiError, ApiResult, AppState};
use serde_json::{Value, json};
use sqlx::{FromRow, PgPool, Postgres, QueryBuilder};

use super::builder::{AppRow, TimelineRow, build_work_report_employees};
use super::dto::{CreateShareReportDto, QueryReportDto, SendReportDto};
use super::pdf::work_report::{ReportData, Sections, generate};
use super::support::{
    OrderedMap, assert_report_range, billable_filter_sql, clamp_to_history, date_part,
    history_min_allowed_date, parse_id_list, share_report_screenshot_path,
    share_report_stored_range, shift_months_js, timeline_is_billable,
};
use super::xlsx;
use crate::division::{ViewerScope, resolve_viewer_scope, role_code_in_company, scope_forbidden};
use crate::screenshots::access::{is_owner_company_key, screenshot_image_path};

const DAY_MS: i64 = 24 * 60 * 60 * 1000;
/// `screenshotRetentionDays` >= ini = placeholder "unlimited" (paket pro).
const UNLIMITED_RETENTION_DAYS: i32 = 36500;
/// Batas id per query agregat aplikasi.
const APP_USAGE_CHUNK: usize = 5000;
const STATUS_FINISHED: i32 = 3;
const SHARE_EXPIRES_IN_DAYS_DEFAULT: i64 = 365;

// ─── query timeline bersama ────────────────────────────────────────────────────

#[derive(FromRow)]
struct TlRow {
    id: i32,
    uuid: String,
    note: String,
    #[sqlx(rename = "startAt")]
    start_at: JsDate,
    #[sqlx(rename = "endAt")]
    end_at: JsDate,
    #[sqlx(rename = "startAtUtc")]
    start_at_utc: Option<JsDate>,
    #[sqlx(rename = "endAtUtc")]
    end_at_utc: Option<JsDate>,
    timezone: Option<String>,
    billable: Option<bool>,
    #[sqlx(rename = "projectId")]
    project_id: i32,
    #[sqlx(rename = "projectUuid")]
    project_uuid: String,
    #[sqlx(rename = "projectName")]
    project_name: String,
    #[sqlx(rename = "projectBillable")]
    project_billable: bool,
    #[sqlx(rename = "takenDate")]
    taken_date: JsDate,
    #[sqlx(rename = "userId")]
    user_id: i32,
    #[sqlx(rename = "userUuid")]
    user_uuid: String,
    username: String,
    #[sqlx(rename = "userName")]
    user_name: String,
}

impl From<TlRow> for TimelineRow {
    fn from(r: TlRow) -> Self {
        Self {
            id: r.id,
            uuid: r.uuid,
            note: Some(r.note),
            start_at: r.start_at,
            end_at: r.end_at,
            start_at_utc: r.start_at_utc,
            end_at_utc: r.end_at_utc,
            timezone: r.timezone,
            billable: r.billable,
            project_id: r.project_id,
            project_uuid: r.project_uuid,
            project_name: r.project_name,
            project_billable: r.project_billable,
            taken_date: r.taken_date,
            user_id: r.user_id,
            user_name: r.user_name,
            user_username: r.username,
        }
    }
}

/// Satu baris timeline plus uuid user (dibutuhkan halaman share publik).
struct TlFull {
    row: TimelineRow,
    user_uuid: String,
}

/// Filter `trackingTimeline` bersama `find`, export, dan share.
struct TimelineFilter<'a> {
    company_id: i32,
    project_ids: &'a [i32],
    /// `Some` = filter user diterapkan walau kosong (kosong = tidak ada hasil).
    user_ids: Option<&'a [i32]>,
    billable_sql: Option<&'static str>,
    /// `YYYY-MM-DD`
    start_date: &'a str,
    end_date: &'a str,
}

async fn fetch_timelines(db: &PgPool, f: &TimelineFilter<'_>) -> sqlx::Result<Vec<TlFull>> {
    let mut qb = QueryBuilder::<Postgres>::new(
        r#"SELECT tt.id, tt.uuid::text AS uuid, tt.note, tt."startAt", tt."endAt", tt."startAtUtc", tt."endAtUtc",
                  tt.timezone, tt.billable, tt."projectId", p.uuid::text AS "projectUuid", p.name AS "projectName",
                  p.billable AS "projectBillable", tr."takenDate"::timestamp AS "takenDate", tr."userId",
                  u.uuid::text AS "userUuid", u.username, u.name AS "userName"
           FROM "trackingTimeline" tt
           JOIN project p ON p.id = tt."projectId"
           JOIN tracking tr ON tr.id = tt."trackingId"
           JOIN users u ON u.id = tr."userId"
           WHERE tt.status = 1 AND tr.status = 1
             AND EXISTS (SELECT 1 FROM "projectCompany" pc WHERE pc."projectId" = tt."projectId" AND pc."companyId" = "#,
    );
    qb.push_bind(f.company_id);
    qb.push(r#") AND EXISTS (SELECT 1 FROM "userCompany" uc WHERE uc."userId" = tr."userId" AND uc."companyId" = "#);
    qb.push_bind(f.company_id);
    qb.push(")");
    if !f.project_ids.is_empty() {
        qb.push(r#" AND tt."projectId" = ANY("#)
            .push_bind(f.project_ids.to_vec())
            .push(")");
    }
    if let Some(users) = f.user_ids {
        qb.push(r#" AND tr."userId" = ANY("#)
            .push_bind(users.to_vec())
            .push(")");
    }
    if let Some(b) = f.billable_sql {
        qb.push(" AND ").push(b);
    }
    qb.push(r#" AND tr."takenDate" >= "#)
        .push_bind(f.start_date.to_owned())
        .push("::date");
    qb.push(r#" AND tr."takenDate" <= "#)
        .push_bind(f.end_date.to_owned())
        .push("::date");
    qb.push(r#" ORDER BY tt."startAt" ASC, tt.id ASC"#);
    let rows: Vec<TlRow> = qb.build_query_as().fetch_all(db).await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let user_uuid = r.user_uuid.clone();
            TlFull {
                row: r.into(),
                user_uuid,
            }
        })
        .collect())
}

/// Screenshot FINISHED per timeline: timeline id -> [(uuid, takenAt, createdAt)] (urut id).
async fn fetch_screenshots(
    db: &PgPool,
    timeline_ids: &[i32],
) -> sqlx::Result<HashMap<i32, Vec<(String, JsDate, JsDate)>>> {
    let rows: Vec<(i32, String, JsDate, JsDate)> = sqlx::query_as(
        r#"SELECT s."trackingTimelineId", s.uuid::text, s."takenAt", s."createdAt"
           FROM screenshot s WHERE s.status = $1 AND s."trackingTimelineId" = ANY($2) ORDER BY s.id"#,
    )
    .bind(STATUS_FINISHED)
    .bind(timeline_ids)
    .fetch_all(db)
    .await?;
    let mut map: HashMap<i32, Vec<(String, JsDate, JsDate)>> = HashMap::new();
    for (tl, uuid, taken, created) in rows {
        map.entry(tl).or_default().push((uuid, taken, created));
    }
    Ok(map)
}

fn ctx_err(what: &str) -> ApiError {
    ApiError::internal(anyhow::anyhow!("{what}"))
}

/// `Date.toISOString().split('T')[0]` dari `JsDate`.
fn jd(d: JsDate) -> String {
    date_part(d.0)
}

// ─── find (GET /reports) ───────────────────────────────────────────────────────

#[derive(FromRow)]
struct CompanyCtx {
    #[sqlx(rename = "dataHistoryMonths")]
    data_history_months: i32,
}

/// `resolveNonAdminUserIds`: Leader divisi -> anggota divisinya; selain itu `[userId]`.
async fn resolve_non_admin_user_ids(
    db: &PgPool,
    user_id: i32,
    company_id: i32,
    requested: Vec<i32>,
) -> ApiResult<Vec<i32>> {
    let scope = resolve_viewer_scope(db, user_id, company_id).await?;
    let ViewerScope::Division { user_ids } = scope else {
        return Ok(vec![user_id]);
    };
    if requested.is_empty() {
        return Ok(user_ids);
    }
    if !requested.iter().all(|id| user_ids.contains(id)) {
        return Err(scope_forbidden());
    }
    Ok(requested)
}

fn user_json(r: &TimelineRow) -> Value {
    json!({"id": r.user_id, "username": r.user_username, "name": r.user_name})
}

fn project_json_full(r: &TimelineRow) -> Value {
    json!({"id": r.project_id, "uuid": r.project_uuid, "name": r.project_name, "billable": r.project_billable})
}

struct Merged {
    uuid: String,
    uuids: Vec<String>,
    notes: Vec<String>,
    start_at: JsDate,
    end_at: JsDate,
    start_at_utc: Option<JsDate>,
    end_at_utc: Option<JsDate>,
    timezone: Option<String>,
    project_id: i32,
    project: Value,
    billable: bool,
    screenshot: Vec<Value>,
    taken_date: JsDate,
    user_id: i32,
    user: Value,
    total: i64,
}

/// `merged note`: hanya note yang ada isinya (bukan kosong / "-"), unik, digabung " | ".
fn merge_notes(raw: &[String]) -> String {
    let mut meaningful: Vec<String> = Vec::new();
    for r in raw {
        let n = r.trim();
        if n.is_empty() || n == "-" {
            continue;
        }
        if !meaningful.iter().any(|m| m == n) {
            meaningful.push(n.to_owned());
        }
    }
    if meaningful.is_empty() {
        "-".to_owned()
    } else {
        meaningful.join(" | ")
    }
}

/// Node: reports.service.ts `find`.
pub async fn find(state: &AppState, query: &QueryReportDto, user_id: i32) -> ApiResult<Value> {
    assert_report_range(Some(query.start_at()), Some(query.end_at()))?;
    let db = &state.db;

    let company_id = mz_core::company::find_company_id(db, user_id)
        .await?
        .ok_or_else(|| ctx_err("Cannot read properties of undefined (reading 'companyId')"))?;
    let ctx: CompanyCtx =
        sqlx::query_as(r#"SELECT "dataHistoryMonths" FROM company WHERE id = $1"#)
            .bind(company_id)
            .fetch_one(db)
            .await?;
    let is_admin = sees_whole_company(
        role_code_in_company(db, user_id, company_id)
            .await?
            .as_deref(),
    );

    let now = now_naive();
    let requested_start =
        parse_js_date(query.start_at()).ok_or_else(|| ctx_err("Invalid time value"))?;
    let start_date = clamp_to_history(requested_start, Some(ctx.data_history_months), now);
    let end_date = parse_js_date(query.end_at()).ok_or_else(|| ctx_err("Invalid time value"))?;
    let start_str = date_part(start_date);
    let end_str = date_part(end_date);

    let mut user_ids = parse_id_list(query.user.as_deref());
    if !is_admin {
        user_ids = resolve_non_admin_user_ids(db, user_id, company_id, user_ids).await?;
    }
    let project_ids = parse_id_list(query.project.as_deref());

    let timelines = fetch_timelines(
        db,
        &TimelineFilter {
            company_id,
            project_ids: &project_ids,
            user_ids: (!user_ids.is_empty()).then_some(user_ids.as_slice()),
            billable_sql: billable_filter_sql(query.billable_filter.as_deref()),
            start_date: &start_str,
            end_date: &end_str,
        },
    )
    .await?;
    let ids: Vec<i32> = timelines.iter().map(|t| t.row.id).collect();
    let shots = fetch_screenshots(db, &ids).await?;

    let mut sum_second_total = 0i64;
    let mut billable_seconds = 0i64;
    let mut non_billable_seconds = 0i64;
    let mut timeline_map: HashMap<String, i64> = HashMap::new();
    let mut tracking_projects: OrderedMap<String, (Value, Value, i64)> = OrderedMap::new();
    let mut employee_second: OrderedMap<i32, (Value, i64)> = OrderedMap::new();
    let mut notes_map: OrderedMap<String, (String, String, i64)> = OrderedMap::new();
    let mut project_summary: OrderedMap<i32, (Value, i64)> = OrderedMap::new();
    let mut merged: OrderedMap<String, Merged> = OrderedMap::new();

    for t in &timelines {
        let r = &t.row;
        let taken = jd(r.taken_date);
        let start = r.start_at_utc.unwrap_or(r.start_at);
        let duration =
            timeline_duration_seconds(r.start_at, r.end_at, r.start_at_utc, r.end_at_utc);
        sum_second_total += duration;
        if timeline_is_billable(r.billable, r.project_billable) {
            billable_seconds += duration;
        } else {
            non_billable_seconds += duration;
        }

        *timeline_map.entry(taken.clone()).or_insert(0) += duration;
        let p_key = format!("{}-{}", r.project_id, r.user_id);
        tracking_projects
            .entry_or_insert_with(p_key, || (project_json_full(r), user_json(r), 0))
            .2 += duration;
        employee_second
            .entry_or_insert_with(r.user_id, || (user_json(r), 0))
            .1 += duration;
        notes_map
            .entry_or_insert_with(
                format!("{}-{}", r.note.clone().unwrap_or_default(), r.user_id),
                || (r.note.clone().unwrap_or_default(), r.user_name.clone(), 0),
            )
            .2 += duration;
        project_summary
            .entry_or_insert_with(r.project_id, || (project_json_full(r), 0))
            .1 += duration;

        let hh_mm = start.0.format("%H:%M").to_string();
        let merge_key = format!("{}-{}-{}-{}", r.user_id, r.project_id, taken, hh_mm);
        let screenshot: Vec<Value> = shots
            .get(&r.id)
            .map(|v| {
                v.iter()
                    .map(|(uuid, taken_at, _)| {
                        json!({"uuid": uuid, "screenshotUrl": screenshot_image_path(uuid, "display"), "takenAt": taken_at})
                    })
                    .collect()
            })
            .unwrap_or_default();

        if merged.contains_key(&merge_key) {
            if let Some(e) = merged.get_mut(&merge_key) {
                if r.start_at.timestamp_millis() < e.start_at.timestamp_millis() {
                    e.start_at = r.start_at;
                    e.start_at_utc = r.start_at_utc;
                }
                if r.end_at.timestamp_millis() > e.end_at.timestamp_millis() {
                    e.end_at = r.end_at;
                    e.end_at_utc = r.end_at_utc;
                }
                e.screenshot.extend(screenshot);
                e.uuids.push(r.uuid.clone());
                e.notes.push(r.note.clone().unwrap_or_default());
                e.total += duration;
            }
        } else {
            merged.entry_or_insert_with(merge_key, || Merged {
                uuid: r.uuid.clone(),
                uuids: vec![r.uuid.clone()],
                notes: vec![r.note.clone().unwrap_or_default()],
                start_at: r.start_at,
                end_at: r.end_at,
                start_at_utc: r.start_at_utc,
                end_at_utc: r.end_at_utc,
                timezone: r.timezone.clone(),
                project_id: r.project_id,
                project: project_json_full(r),
                billable: r.billable.unwrap_or(r.project_billable),
                screenshot,
                taken_date: r.taken_date,
                user_id: r.user_id,
                user: user_json(r),
                total: duration,
            });
        }
    }

    let merged_trackings: Vec<Value> = merged
        .into_values()
        .into_iter()
        .map(|m| {
            json!({
                "uuid": m.uuid,
                "uuids": m.uuids,
                "note": merge_notes(&m.notes),
                "startAt": m.start_at,
                "endAt": m.end_at,
                "startAtUtc": m.start_at_utc,
                "endAtUtc": m.end_at_utc,
                "timezone": m.timezone,
                "projectId": m.project_id,
                "project": m.project,
                "billable": m.billable,
                "screenshot": m.screenshot,
                "tracking": {
                    "takenDate": m.taken_date,
                    "userId": m.user_id,
                    "user": m.user,
                    "totalTakenInSecond": m.total,
                },
            })
        })
        .collect();

    // Timeline harian: `setDate(+1)` dari startDate (jam hari ikut terbawa).
    let mut day_rows: Vec<Value> = Vec::new();
    let mut current: NaiveDateTime = start_date;
    while current <= end_date {
        let d = date_part(current);
        day_rows.push(json!({"takenDate": d, "totalTakenInSecond": timeline_map.get(&d).copied().unwrap_or(0)}));
        current += Duration::days(1);
    }

    let employee_rows: Vec<Value> = employee_second
        .values()
        .map(|(user, total)| json!({"user": user, "totalTakenInSecond": total}))
        .collect();
    // Bilangan bulat (Math.round JS): serde_json f64 akan tampil `67.0`, Node `67`.
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    let billable_pct: i64 = if sum_second_total > 0 {
        ((billable_seconds as f64 / sum_second_total as f64) * 100.0 + 0.5).floor() as i64
    } else {
        0
    };

    Ok(json!({
        "success": true,
        "message": "success get report data",
        "data": {
            "trackings": merged_trackings,
            "totalTakenInSecond": sum_second_total,
            "billableSeconds": billable_seconds,
            "nonBillableSeconds": non_billable_seconds,
            "billablePercentage": billable_pct,
            "timelines": day_rows,
            "trackingProjects": tracking_projects.values()
                .map(|(p, u, t)| json!({"project": p, "user": u, "totalTakenInSecond": t})).collect::<Vec<_>>(),
            "trackingEmployees": employee_rows.clone(),
            "trackingNotes": notes_map.values()
                .map(|(n, e, d)| json!({"notes": n, "employee": e, "duration": d})).collect::<Vec<_>>(),
            "employeeSecond": employee_rows,
            "projectSecond": project_summary.values()
                .map(|(p, t)| json!({"project": p, "totalTakenInSecond": t})).collect::<Vec<_>>(),
        },
    }))
}

// ─── export (Excel / PDF) ──────────────────────────────────────────────────────

pub struct DetailedRow {
    pub date: String,
    pub employee: String,
    pub project: String,
    pub notes: String,
    pub seconds: i64,
    pub billable: bool,
}

pub struct ProjectRow {
    pub project: String,
    pub employee: String,
    pub seconds: i64,
}

pub struct EmployeeRow {
    pub employee: String,
    pub seconds: i64,
}

/// `ReportExportSource`.
pub struct ExportSource {
    pub company_name: String,
    pub generated_by: String,
    pub start_date: String,
    pub end_date: String,
    pub detailed: Vec<DetailedRow>,
    pub projects: Vec<ProjectRow>,
    pub employees: Vec<EmployeeRow>,
    pub time_zone: Option<String>,
    pub timelines: Vec<TimelineRow>,
}

#[derive(FromRow)]
struct ExportCtx {
    name: String,
    username: String,
    #[sqlx(rename = "companyName")]
    company_name: String,
    #[sqlx(rename = "timeZone")]
    time_zone: String,
    #[sqlx(rename = "dataHistoryMonths")]
    data_history_months: i32,
}

/// Node: `buildReportExportData`.
pub async fn build_export_source(
    state: &AppState,
    query: &QueryReportDto,
    user_id: i32,
) -> ApiResult<ExportSource> {
    assert_report_range(Some(query.start_at()), Some(query.end_at()))?;
    let db = &state.db;
    let not_attached = || ApiError::forbidden("User is not attached to a company");

    let company_id = mz_core::company::find_company_id(db, user_id)
        .await?
        .ok_or_else(not_attached)?;
    let ctx: ExportCtx = sqlx::query_as(
        r#"SELECT u.name, u.username, c.name AS "companyName", c."timeZone", c."dataHistoryMonths"
           FROM users u, company c WHERE u.id = $1 AND c.id = $2"#,
    )
    .bind(user_id)
    .bind(company_id)
    .fetch_optional(db)
    .await?
    .ok_or_else(not_attached)?;
    let role_code = role_code_in_company(db, user_id, company_id)
        .await?
        .ok_or_else(not_attached)?;
    let company_name = if ctx.company_name.is_empty() {
        "Company".to_owned()
    } else {
        ctx.company_name.clone()
    };
    let sees_company_wide = sees_whole_company(Some(&role_code));

    let requested_start =
        parse_js_date(query.start_at()).ok_or_else(|| ctx_err("Invalid time value"))?;
    let start_date = clamp_to_history(requested_start, Some(ctx.data_history_months), now_naive());
    let end_date = parse_js_date(query.end_at()).ok_or_else(|| ctx_err("Invalid time value"))?;
    let start_str = date_part(start_date);
    let end_str = date_part(end_date);

    let mut user_ids = parse_id_list(query.user.as_deref());
    if !sees_company_wide {
        user_ids = resolve_non_admin_user_ids(db, user_id, company_id, user_ids).await?;
    } else if user_ids.is_empty() {
        user_ids = sqlx::query_scalar(
            r#"SELECT u.id FROM users u
               WHERE u.status = 1 AND EXISTS (SELECT 1 FROM "userCompany" uc WHERE uc."userId" = u.id AND uc."companyId" = $1)"#,
        )
        .bind(company_id)
        .fetch_all(db)
        .await?;
    }
    let project_ids = parse_id_list(query.project.as_deref());

    let timelines = fetch_timelines(
        db,
        &TimelineFilter {
            company_id,
            project_ids: &project_ids,
            user_ids: Some(&user_ids),
            billable_sql: billable_filter_sql(query.billable_filter.as_deref()),
            start_date: &start_str,
            end_date: &end_str,
        },
    )
    .await?;

    let mut project_user: OrderedMap<String, (i32, i32, i64)> = OrderedMap::new();
    let mut employee_map: OrderedMap<i32, i64> = OrderedMap::new();
    let mut project_ref: HashMap<i32, String> = HashMap::new();
    let mut user_ref: HashMap<i32, String> = HashMap::new();
    struct Row {
        date: String,
        employee: String,
        project: String,
        notes: Vec<String>,
        seconds: i64,
        billable: bool,
    }
    let mut merged: OrderedMap<String, Row> = OrderedMap::new();

    for t in &timelines {
        let r = &t.row;
        let start = r.start_at_utc.unwrap_or(r.start_at);
        let seconds = timeline_duration_seconds(r.start_at, r.end_at, r.start_at_utc, r.end_at_utc);
        project_ref.insert(r.project_id, r.project_name.clone());
        user_ref.insert(r.user_id, r.user_name.clone());
        let date_only = jd(r.taken_date);
        let is_billable = timeline_is_billable(r.billable, r.project_billable);
        let raw_note = r.note.clone().unwrap_or_default();
        let note = if raw_note.is_empty() || raw_note == "null" {
            "-".to_owned()
        } else {
            raw_note
        };

        let key = format!(
            "{}-{}-{}-{}",
            r.user_id,
            r.project_id,
            date_only,
            start.0.format("%H:%M")
        );
        if merged.contains_key(&key) {
            if let Some(e) = merged.get_mut(&key) {
                e.notes.push(note);
                e.seconds += seconds;
            }
        } else {
            merged.entry_or_insert_with(key, || Row {
                date: date_only,
                employee: r.user_name.clone(),
                project: r.project_name.clone(),
                notes: vec![note],
                seconds,
                billable: is_billable,
            });
        }
        project_user
            .entry_or_insert_with(format!("{}-{}", r.project_id, r.user_id), || {
                (r.project_id, r.user_id, 0)
            })
            .2 += seconds;
        *employee_map.entry_or_insert_with(r.user_id, || 0) += seconds;
    }

    let detailed: Vec<DetailedRow> = merged
        .into_values()
        .into_iter()
        .map(|row| DetailedRow {
            date: row.date,
            employee: row.employee,
            project: row.project,
            notes: merge_notes(&row.notes),
            seconds: row.seconds,
            billable: row.billable,
        })
        .collect();
    let projects: Vec<ProjectRow> = project_user
        .values()
        .map(|(p, u, s)| ProjectRow {
            project: project_ref.get(p).cloned().unwrap_or_default(),
            employee: user_ref.get(u).cloned().unwrap_or_default(),
            seconds: *s,
        })
        .collect();
    let employees: Vec<EmployeeRow> = employee_map
        .keys()
        .zip(employee_map.values())
        .map(|(u, s)| EmployeeRow {
            employee: user_ref.get(u).cloned().unwrap_or_default(),
            seconds: *s,
        })
        .collect();

    Ok(ExportSource {
        company_name,
        generated_by: if ctx.name.is_empty() {
            ctx.username
        } else {
            ctx.name
        },
        start_date: start_str,
        end_date: end_str,
        detailed,
        projects,
        employees,
        time_zone: Some(ctx.time_zone).filter(|z| !z.is_empty()),
        timelines: timelines.into_iter().map(|t| t.row).collect(),
    })
}

/// `fetchAppUsage`: detik per (timeline, nama aplikasi) dari tabel `app`.
pub async fn fetch_app_usage(db: &PgPool, timeline_ids: &[i32]) -> sqlx::Result<Vec<AppRow>> {
    let mut ids: Vec<i32> = timeline_ids.to_vec();
    ids.sort_unstable();
    ids.dedup();
    let mut rows = Vec::new();
    for chunk in ids.chunks(APP_USAGE_CHUNK) {
        let part: Vec<(i32, String, f64)> = sqlx::query_as(
            r#"SELECT "trackingTimelineId", name,
                      SUM(GREATEST(EXTRACT(EPOCH FROM ("endAt" - "startAt")), 0))::float8 AS seconds
               FROM app WHERE "trackingTimelineId" = ANY($1) GROUP BY "trackingTimelineId", name"#,
        )
        .bind(chunk)
        .fetch_all(db)
        .await?;
        rows.extend(part.into_iter().map(|(id, name, seconds)| AppRow {
            tracking_timeline_id: id,
            name,
            seconds,
        }));
    }
    Ok(rows)
}

/// Node: `generateExcel`.
pub async fn generate_excel(
    state: &AppState,
    query: &QueryReportDto,
    user_id: i32,
) -> ApiResult<Vec<u8>> {
    let data = build_export_source(state, query, user_id).await?;
    xlsx::build(&data).map_err(ApiError::internal)
}

/// Node: `generateReportPdf`.
pub async fn generate_report_pdf(
    state: &AppState,
    query: &QueryReportDto,
    user_id: i32,
) -> ApiResult<Vec<u8>> {
    let data = build_export_source(state, query, user_id).await?;
    let ids: Vec<i32> = data.timelines.iter().map(|t| t.id).collect();
    let app_rows = fetch_app_usage(&state.db, &ids).await?;
    let employees = build_work_report_employees(&data.timelines, &app_rows);
    let report = ReportData {
        company_name: data.company_name,
        start_date: data.start_date,
        end_date: data.end_date,
        generated_at: chrono::Utc::now(),
        generated_by: data.generated_by,
        time_zone: data.time_zone,
        employees,
        sections: Sections::default(),
    };
    tokio::task::spawn_blocking(move || generate(&report))
        .await
        .map_err(ApiError::internal)
}

// ─── kuota export ──────────────────────────────────────────────────────────────

/// Pelepas kuota: kembalikan satu slot bila pembuatan file gagal.
pub struct QuotaClaim {
    company_id: i32,
}

impl QuotaClaim {
    pub async fn release(self, db: &PgPool) {
        let _ = sqlx::query(
            r#"UPDATE company SET "reportExportsUsedThisMonth" = "reportExportsUsedThisMonth" - 1
               WHERE id = $1 AND "reportExportsUsedThisMonth" > 0"#,
        )
        .bind(self.company_id)
        .execute(db)
        .await;
    }
}

/// `claimExportQuota`: klaim satu slot kuota export bulanan secara atomik.
pub async fn claim_export_quota(state: &AppState, user_id: i32) -> ApiResult<QuotaClaim> {
    let db = &state.db;
    let company_id = mz_core::company::find_company_id(db, user_id).await?;
    let row: Option<(i32, i32, Option<JsDate>)> = match company_id {
        Some(c) => {
            sqlx::query_as(r#"SELECT id, "reportExportsPerMonth", "reportExportsResetAt" FROM company WHERE id = $1"#)
                .bind(c)
                .fetch_optional(db)
                .await?
        }
        None => None,
    };
    let Some((id, per_month, reset_at)) = row else {
        return Err(ApiError::Plain {
            status: StatusCode::FORBIDDEN,
            message: "Company not found".into(),
        });
    };

    let now = now_naive();
    let mut reset = reset_at.map(|d| d.0);
    // Awal periode baru: reset counter sebelum klaim.
    if reset.is_none_or(|r| now > r) {
        let next = shift_months_js(now, 1);
        reset = Some(next);
        sqlx::query(
            r#"UPDATE company SET "reportExportsUsedThisMonth" = 0, "reportExportsResetAt" = $2
               WHERE id = $1 AND ("reportExportsResetAt" IS NULL OR "reportExportsResetAt" < $3)"#,
        )
        .bind(id)
        .bind(next)
        .bind(now)
        .execute(db)
        .await?;
    }

    // -1 = tanpa batas: lewati filter `lt`, counter tetap naik.
    let claimed = if per_month == -1 {
        sqlx::query(r#"UPDATE company SET "reportExportsUsedThisMonth" = "reportExportsUsedThisMonth" + 1 WHERE id = $1"#)
            .bind(id)
            .execute(db)
            .await?
    } else {
        sqlx::query(
            r#"UPDATE company SET "reportExportsUsedThisMonth" = "reportExportsUsedThisMonth" + 1
               WHERE id = $1 AND "reportExportsUsedThisMonth" < $2"#,
        )
        .bind(id)
        .bind(per_month)
        .execute(db)
        .await?
    };
    if claimed.rows_affected() == 0 {
        let resets_on = reset.map(date_part).unwrap_or_default();
        return Err(ApiError::Plain {
            status: StatusCode::PAYMENT_REQUIRED,
            message: format!(
                "Export quota exceeded. You have {per_month} exports per month. Resets on {resets_on}"
            ),
        });
    }
    Ok(QuotaClaim { company_id: id })
}

// ─── share report ──────────────────────────────────────────────────────────────

/// Node: `findAllShareReport`.
pub async fn find_all_share_report(
    state: &AppState,
    query: &HashMap<String, String>,
    user_id: i32,
) -> ApiResult<Value> {
    let is_day = |v: Option<&String>| {
        v.is_some_and(|s| {
            s.len() == 10
                && s.bytes().enumerate().all(|(i, b)| {
                    if i == 4 || i == 7 {
                        b == b'-'
                    } else {
                        b.is_ascii_digit()
                    }
                })
        })
    };
    if !is_day(query.get("startAt")) || !is_day(query.get("endAt")) {
        return Err(ApiError::bad_request(
            "startAt and endAt must be YYYY-MM-DD",
        ));
    }
    let start = query.get("startAt").cloned().unwrap_or_default();
    let end = query.get("endAt").cloned().unwrap_or_default();

    let run = async {
        let (s0, s1) =
            share_report_stored_range(&start, &start).ok_or_else(|| anyhow::anyhow!("bad date"))?;
        let (e0, e1) =
            share_report_stored_range(&end, &end).ok_or_else(|| anyhow::anyhow!("bad date"))?;
        let rows: Vec<(i32, String, String, JsDate, JsDate, JsDate)> = sqlx::query_as(
            r#"SELECT id, uuid::text, name, "startAt", "endAt", "expiresAt" FROM "shareReport"
               WHERE "userId" = $1 AND "startAt" >= $2 AND "startAt" <= $3 AND "endAt" >= $4 AND "endAt" <= $5"#,
        )
        .bind(user_id)
        .bind(s0)
        .bind(s1)
        .bind(e0)
        .bind(e1)
        .fetch_all(&state.db)
        .await?;
        Ok::<_, anyhow::Error>(
            rows.into_iter()
                .map(|(id, uuid, name, s, e, x)| {
                    json!({"id": id, "uuid": uuid, "name": name, "startAt": s, "endAt": e, "expiresAt": x})
                })
                .collect::<Vec<_>>(),
        )
    };
    Ok(match run.await {
        Ok(data) => {
            json!({"success": true, "message": "success get all share reports", "data": data})
        }
        Err(error) => {
            tracing::error!(%error, "Failed to get share reports");
            json!({"success": false, "message": "Failed to get share reports", "data": Value::Null})
        }
    })
}

#[derive(FromRow)]
struct ShareRow {
    id: i32,
    #[sqlx(rename = "userId")]
    user_id: i32,
    uuid: String,
    name: String,
    #[sqlx(rename = "startAt")]
    start_at: JsDate,
    #[sqlx(rename = "endAt")]
    end_at: JsDate,
    #[sqlx(rename = "expiresAt")]
    expires_at: JsDate,
    #[sqlx(rename = "companyId")]
    company_id: Option<i32>,
}

/// Share aktif + konteks pembuatnya.
struct ActiveShare {
    share: ShareRow,
    /// (userId, username, name)
    users: Vec<(i32, String, String)>,
    /// (projectId, name, uuid)
    projects: Vec<(i32, String, String)>,
    company_id: i32,
    company_uuid: String,
    retention_days: Option<i32>,
    min_taken_date: Option<String>,
    sees_whole_company: bool,
}

impl ActiveShare {
    /// `shareReportUserIds`.
    fn user_ids(&self) -> Vec<i32> {
        if self.sees_whole_company {
            self.users.iter().map(|u| u.0).collect()
        } else {
            vec![self.share.user_id]
        }
    }

    fn project_ids(&self) -> Vec<i32> {
        self.projects.iter().map(|p| p.0).collect()
    }

    /// `shareReportDateRange` (UTC).
    fn date_range(&self) -> (String, String) {
        (jd(self.share.start_at), jd(self.share.end_at))
    }

    /// Rentang efektif `shareReportTimelineWhere` (batas riwayat memotong awal).
    fn effective_range(&self) -> (String, String) {
        let (start, end) = self.date_range();
        let start = match &self.min_taken_date {
            Some(min) if *min > start => min.clone(),
            _ => start,
        };
        (start, end)
    }
}

/// `loadActiveShare`: kedaluwarsa dicek di query yang sama dengan pencarian share.
async fn load_active_share(state: &AppState, uuid: &str) -> ApiResult<Option<ActiveShare>> {
    let db = &state.db;
    let share: Option<ShareRow> = sqlx::query_as(
        r#"SELECT id, "userId", uuid::text AS uuid, name, "startAt", "endAt", "expiresAt", "companyId"
           FROM "shareReport" WHERE uuid = $1::uuid AND "expiresAt" > $2 LIMIT 1"#,
    )
    .bind(uuid)
    .bind(now_naive())
    .fetch_optional(db)
    .await?;
    let Some(share) = share else { return Ok(None) };
    let Some(company_id) = share.company_id else {
        return Ok(None);
    };

    // Company dikunci di share; pembuat yang sudah keluar dari company itu -> link mati.
    let membership: Option<(String, Option<i32>, i32)> = sqlx::query_as(
        r#"SELECT c.uuid::text, c."screenshotRetentionDays", c."dataHistoryMonths"
           FROM "userCompany" uc JOIN company c ON c.id = uc."companyId"
           WHERE uc."userId" = $1 AND uc."companyId" = $2 AND uc.status = 1 LIMIT 1"#,
    )
    .bind(share.user_id)
    .bind(company_id)
    .fetch_optional(db)
    .await?;
    let Some((company_uuid, retention_days, history_months)) = membership else {
        return Ok(None);
    };

    let users: Vec<(i32, String, String)> = sqlx::query_as(
        r#"SELECT sru."userId", u.username, u.name FROM "shareReportUsers" sru
           JOIN users u ON u.id = sru."userId" WHERE sru."shareId" = $1 ORDER BY sru.id"#,
    )
    .bind(share.id)
    .fetch_all(db)
    .await?;
    let projects: Vec<(i32, String, String)> = sqlx::query_as(
        r#"SELECT srp."projectId", p.name, p.uuid::text FROM "shareReportProjects" srp
           JOIN project p ON p.id = srp."projectId" WHERE srp."shareId" = $1 ORDER BY srp.id"#,
    )
    .bind(share.id)
    .fetch_all(db)
    .await?;

    let min_taken_date = history_min_allowed_date(Some(history_months), now_naive()).map(date_part);
    let sees = sees_whole_company(
        role_code_in_company(db, share.user_id, company_id)
            .await?
            .as_deref(),
    );
    Ok(Some(ActiveShare {
        share,
        users,
        projects,
        company_id,
        company_uuid,
        retention_days,
        min_taken_date,
        sees_whole_company: sees,
    }))
}

async fn share_timelines(db: &PgPool, active: &ActiveShare) -> sqlx::Result<Vec<TlFull>> {
    let user_ids = active.user_ids();
    let project_ids = active.project_ids();
    let (start, end) = active.effective_range();
    fetch_timelines(
        db,
        &TimelineFilter {
            company_id: active.company_id,
            project_ids: &project_ids,
            user_ids: (!user_ids.is_empty()).then_some(user_ids.as_slice()),
            billable_sql: None,
            start_date: &start,
            end_date: &end,
        },
    )
    .await
}

/// Node: `findShareReport` (halaman share PUBLIK).
pub async fn find_share_report(state: &AppState, uuid: &str) -> Value {
    match find_share_report_inner(state, uuid).await {
        Ok(v) => v,
        Err(error) => {
            tracing::error!(%error, "Failed to get share report");
            json!({"success": false, "message": "Failed to get share report", "data": Value::Null})
        }
    }
}

async fn find_share_report_inner(state: &AppState, uuid: &str) -> anyhow::Result<Value> {
    let Some(active) = load_active_share(state, uuid)
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))?
    else {
        return Ok(
            json!({"success": false, "message": "Share report not found", "data": Value::Null}),
        );
    };
    let (start_date, end_date) = active.date_range();
    let timelines = share_timelines(&state.db, &active).await?;
    let ids: Vec<i32> = timelines.iter().map(|t| t.row.id).collect();
    let shots = fetch_screenshots(&state.db, &ids).await?;

    let mut sum = 0i64;
    let mut final_trackings: Vec<Value> = Vec::new();
    let mut project_map: OrderedMap<i32, (Value, i64)> = OrderedMap::new();
    let mut employee_map: OrderedMap<i32, (Value, i64)> = OrderedMap::new();
    let mut timeline_map: HashMap<String, i64> = HashMap::new();
    let mut project_employee: OrderedMap<String, (Value, Value, i64, Vec<Value>)> =
        OrderedMap::new();

    for t in &timelines {
        let r = &t.row;
        let t_date = jd(r.taken_date);
        // VALIDASI DOUBLE: tanggal di luar rentang tidak ikut.
        if t_date < start_date || t_date > end_date {
            continue;
        }
        let duration =
            timeline_duration_seconds(r.start_at, r.end_at, r.start_at_utc, r.end_at_utc);
        if duration <= 0 {
            continue;
        }
        sum += duration;
        let screenshots: Vec<Value> = shots
            .get(&r.id)
            .map(|v| {
                v.iter()
                    .map(|(sid, _, created)| {
                        json!({"uuid": sid, "url": share_report_screenshot_path(&active.share.uuid, sid), "createdAt": created})
                    })
                    .collect()
            })
            .unwrap_or_default();
        let project = json!({"id": r.project_id, "uuid": r.project_uuid, "name": r.project_name});
        let user = json!({"id": r.user_id, "username": r.user_username, "name": r.user_name, "uuid": t.user_uuid});

        final_trackings.push(json!({
            "uuid": r.uuid,
            "note": r.note,
            "startAt": r.start_at,
            "endAt": r.end_at,
            "startAtUtc": r.start_at_utc,
            "endAtUtc": r.end_at_utc,
            "timezone": r.timezone,
            "billable": r.billable,
            "project": project,
            "screenshot": screenshots,
            "tracking": {"takenDate": r.taken_date, "user": user, "totalTakenInSecond": duration},
        }));

        *timeline_map.entry(t_date).or_insert(0) += duration;
        project_map
            .entry_or_insert_with(r.project_id, || (project.clone(), 0))
            .1 += duration;
        employee_map
            .entry_or_insert_with(r.user_id, || (user.clone(), 0))
            .1 += duration;
        let pe = project_employee
            .entry_or_insert_with(format!("{}-{}", r.project_id, r.user_id), || {
                (project.clone(), user.clone(), 0, Vec::new())
            });
        pe.2 += duration;
        pe.3.extend(screenshots);
    }

    // Garis waktu harian (YYYY-MM-DD, inklusif).
    let mut timelines_out: Vec<Value> = Vec::new();
    let mut cur = parse_js_date(&start_date).ok_or_else(|| anyhow::anyhow!("bad start"))?;
    let end_limit = parse_js_date(&end_date).ok_or_else(|| anyhow::anyhow!("bad end"))?;
    while cur <= end_limit {
        let d = date_part(cur);
        timelines_out.push(json!({"takenDate": d, "totalTakenInSecond": timeline_map.get(&d).copied().unwrap_or(0)}));
        cur += Duration::days(1);
    }

    // Absen dihitung untuk user yang benar-benar tampil di report.
    let employee_ids: Vec<i32> = employee_map.keys().copied().collect();
    let absent: Vec<(i32, i64)> = sqlx::query_as(
        r#"SELECT "userId", COUNT(id) FROM tracking
           WHERE "userId" = ANY($1) AND "isAbsent" = TRUE AND status = 1
             AND "takenDate" >= $2::date AND "takenDate" <= $3::date
           GROUP BY "userId""#,
    )
    .bind(&employee_ids)
    .bind(&start_date)
    .bind(&end_date)
    .fetch_all(&state.db)
    .await?;
    let absent_map: HashMap<i32, i64> = absent.into_iter().collect();
    let tracking_employees: Vec<Value> = employee_map
        .keys()
        .zip(employee_map.values())
        .map(|(id, (user, total))| {
            json!({"user": user, "totalTakenInSecond": total, "absent": absent_map.get(id).copied().unwrap_or(0)})
        })
        .collect();

    Ok(json!({
        "success": true,
        "message": "success get report data",
        "data": {
            "uuid": active.share.uuid,
            "name": active.share.name,
            "startAt": active.share.start_at,
            "endAt": active.share.end_at,
            "expiresAt": active.share.expires_at,
            "users": active.users.iter().map(|(_, username, name)| json!({"users": {"username": username, "name": name}})).collect::<Vec<_>>(),
            "projects": active.projects.iter().map(|(_, name, uuid)| json!({"projects": {"name": name, "uuid": uuid}})).collect::<Vec<_>>(),
            "trackings": final_trackings,
            "trackingProjects": project_employee.values()
                .map(|(p, u, t, s)| json!({"project": p, "user": u, "totalTakenInSecond": t, "screenshots": s})).collect::<Vec<_>>(),
            "trackingEmployees": tracking_employees,
            "totalTakenInSecond": sum,
            "timelines": timelines_out,
            "employeeSecond": employee_map.values().map(|(u, t)| json!({"user": u, "totalTakenInSecond": t})).collect::<Vec<_>>(),
            "projectSecond": project_map.values().map(|(p, t)| json!({"project": p, "totalTakenInSecond": t})).collect::<Vec<_>>(),
        },
    }))
}

/// Pilihan key screenshot untuk proxy share: varian display (compressed untuk tier gratis).
pub fn share_screenshot_ref(
    retention_days: Option<i32>,
    is_compressed: bool,
    compressed_key: Option<&str>,
    file_fullpath: &str,
) -> String {
    let is_free = retention_days.is_some_and(|r| r > 0 && r < UNLIMITED_RETENTION_DAYS);
    match compressed_key {
        Some(k) if is_free && is_compressed && !k.is_empty() => k.to_owned(),
        _ => file_fullpath.to_owned(),
    }
}

/// Node: `streamShareScreenshot` — kunci key + ref; streaming dilakukan pemanggil.
/// 404 `File not found` untuk semua kegagalan.
pub async fn resolve_share_screenshot_key(
    state: &AppState,
    share_uuid: &str,
    screenshot_uuid: &str,
) -> ApiResult<String> {
    let not_found = || ApiError::not_found("File not found");
    let active = load_active_share(state, share_uuid)
        .await?
        .ok_or_else(not_found)?;

    // Screenshot wajib tampil di report: cakupan sama dengan `findShareReport`.
    let user_ids = active.user_ids();
    let project_ids = active.project_ids();
    let (start, end) = active.effective_range();
    let mut qb = QueryBuilder::<Postgres>::new(
        r#"SELECT s."fileFullpath", s."compressedFileKey", s."isCompressed"
           FROM screenshot s
           JOIN "trackingTimeline" tt ON tt.id = s."trackingTimelineId"
           JOIN tracking tr ON tr.id = tt."trackingId"
           JOIN project p ON p.id = tt."projectId"
           WHERE s.uuid = "#,
    );
    qb.push_bind(screenshot_uuid.to_owned())
        .push("::uuid AND s.status = ")
        .push_bind(STATUS_FINISHED);
    qb.push(" AND tt.status = 1 AND tr.status = 1");
    qb.push(r#" AND EXISTS (SELECT 1 FROM "projectCompany" pc WHERE pc."projectId" = tt."projectId" AND pc."companyId" = "#)
        .push_bind(active.company_id)
        .push(")");
    qb.push(r#" AND EXISTS (SELECT 1 FROM "userCompany" uc WHERE uc."userId" = tr."userId" AND uc."companyId" = "#)
        .push_bind(active.company_id)
        .push(")");
    if !project_ids.is_empty() {
        qb.push(r#" AND tt."projectId" = ANY("#)
            .push_bind(project_ids)
            .push(")");
    }
    if !user_ids.is_empty() {
        qb.push(r#" AND tr."userId" = ANY("#)
            .push_bind(user_ids)
            .push(")");
    }
    qb.push(r#" AND tr."takenDate" >= "#)
        .push_bind(start)
        .push("::date");
    qb.push(r#" AND tr."takenDate" <= "#)
        .push_bind(end)
        .push("::date");
    qb.push(" LIMIT 1");
    let shot: Option<(String, Option<String>, bool)> =
        qb.build_query_as().fetch_optional(&state.db).await?;
    let Some((file_fullpath, compressed_key, is_compressed)) = shot else {
        return Err(not_found());
    };

    let reference = share_screenshot_ref(
        active.retention_days,
        is_compressed,
        compressed_key.as_deref(),
        &file_fullpath,
    );
    let bucket = state.config.var("GCP_BUCKET_NAME");
    let key = mz_files::to_object_key(&reference, bucket.as_deref()).map_err(|_| not_found())?;
    // Key wajib milik company pembuat: `fileFullpath` pernah bisa diisi bebas.
    if !is_owner_company_key(&key, &active.company_uuid) {
        tracing::warn!(
            "Share {share_uuid}: screenshot {screenshot_uuid} key outside company prefix"
        );
        return Err(not_found());
    }
    Ok(key)
}

/// Kegagalan `createShareReport`: HttpException dilempar; lainnya jadi `success: false`.
enum ShareErr {
    Http(ApiError),
    Other(String),
}

impl From<sqlx::Error> for ShareErr {
    fn from(e: sqlx::Error) -> Self {
        Self::Other(e.to_string())
    }
}

impl From<ApiError> for ShareErr {
    fn from(e: ApiError) -> Self {
        Self::Http(e)
    }
}

/// Node: `createShareReport`.
pub async fn create_share_report(
    state: &AppState,
    payload: &CreateShareReportDto,
    user_id: i32,
) -> ApiResult<Value> {
    // Batas rentang yang sama dengan report/export (di luar try: harus 400).
    assert_report_range(Some(payload.start_at()), Some(payload.end_at()))?;

    let run = async {
        let (start_at, end_at) = share_report_stored_range(payload.start_at(), payload.end_at())
            .ok_or_else(|| ShareErr::Other("Invalid time value".into()))?;
        let expires_in_days = payload
            .expires_in_days()
            .unwrap_or(SHARE_EXPIRES_IN_DAYS_DEFAULT);
        let expires_at = now_naive() + Duration::milliseconds(expires_in_days * DAY_MS);

        let mut tx = state.db.begin().await?;
        let company_id = mz_core::company::find_company_id(&mut *tx, user_id).await?;
        let Some(company_id) = company_id else {
            return Err(ShareErr::Http(ApiError::forbidden(
                "No company found for user",
            )));
        };
        let creator_role = role_code_in_company_conn(&mut tx, user_id, company_id).await?;

        let requested = payload.employee_ids();
        let project_ids = payload.project_ids();
        let employee_ids: Vec<i32> = if !sees_whole_company(creator_role.as_deref()) {
            if requested.iter().any(|id| *id != user_id) {
                return Err(ShareErr::Http(ApiError::coded(
                    StatusCode::FORBIDDEN,
                    "SHARE_OTHERS_FORBIDDEN",
                    "You can only share your own report data",
                )));
            }
            vec![user_id]
        } else {
            requested
        };

        if !employee_ids.is_empty() {
            let members: i64 = sqlx::query_scalar(
                r#"SELECT COUNT(DISTINCT "userId") FROM "userCompany" WHERE "companyId" = $1 AND "userId" = ANY($2)"#,
            )
            .bind(company_id)
            .bind(&employee_ids)
            .fetch_one(&mut *tx)
            .await?;
            if usize::try_from(members).ok() != Some(employee_ids.len()) {
                return Err(ShareErr::Http(ApiError::bad_request(
                    "Some employees are not members of your company",
                )));
            }
        }
        if !project_ids.is_empty() {
            let projects: i64 = sqlx::query_scalar(
                r#"SELECT COUNT(*) FROM project p WHERE p.id = ANY($1)
                   AND EXISTS (SELECT 1 FROM "projectCompany" pc WHERE pc."projectId" = p.id AND pc."companyId" = $2)"#,
            )
            .bind(&project_ids)
            .bind(company_id)
            .fetch_one(&mut *tx)
            .await?;
            if usize::try_from(projects).ok() != Some(project_ids.len()) {
                return Err(ShareErr::Http(ApiError::bad_request(
                    "Some projects do not belong to your company",
                )));
            }
        }

        // TODO(audit): shareReport bukan model ber-audit; tidak ada entri audit.
        let created: ShareRow = sqlx::query_as(
            r#"INSERT INTO "shareReport" (name, "userId", "startAt", "endAt", "expiresAt", "companyId")
               VALUES ($1, $2, $3, $4, $5, $6)
               RETURNING id, "userId", uuid::text AS uuid, name, "startAt", "endAt", "expiresAt", "companyId""#,
        )
        .bind(payload.name())
        .bind(user_id)
        .bind(start_at)
        .bind(end_at)
        .bind(expires_at)
        .bind(company_id)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query(
            r#"INSERT INTO "shareReportUsers" ("shareId", "userId") SELECT $1, UNNEST($2::int[])"#,
        )
        .bind(created.id)
        .bind(&employee_ids)
        .execute(&mut *tx)
        .await?;
        sqlx::query(r#"INSERT INTO "shareReportProjects" ("shareId", "projectId") SELECT $1, UNNEST($2::int[])"#)
            .bind(created.id)
            .bind(&project_ids)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;

        Ok::<Value, ShareErr>(json!({
            "success": true,
            "message": "Success create share report",
            "data": {
                "id": created.id,
                "userId": created.user_id,
                "uuid": created.uuid,
                "name": created.name,
                "startAt": created.start_at,
                "endAt": created.end_at,
                "expiresAt": created.expires_at,
                "companyId": created.company_id,
            },
        }))
    };

    match run.await {
        Ok(v) => Ok(v),
        Err(ShareErr::Http(e)) => Err(e),
        Err(ShareErr::Other(message)) => Ok(json!({
            "success": false,
            "message": if message.is_empty() { "Failed to create share report".to_owned() } else { message },
            "data": Value::Null,
        })),
    }
}

/// `roleCodeInCompany` di dalam transaksi.
async fn role_code_in_company_conn(
    conn: &mut sqlx::PgConnection,
    user: i32,
    company: i32,
) -> sqlx::Result<Option<String>> {
    let row: Option<(String, bool, Option<String>, i32)> = sqlx::query_as(
        r#"SELECT al.code, al."isSystem", al."baseRole"::text, al.status
           FROM "userAccessLevel" ual JOIN "accessLevel" al ON al.id = ual."accessLevelId"
           WHERE ual."userId" = $1 AND ual."companyId" = $2 AND ual.status = 1
           ORDER BY ual.id DESC LIMIT 1"#,
    )
    .bind(user)
    .bind(company)
    .fetch_optional(conn)
    .await?;
    Ok(row.and_then(|(code, is_system, base, status)| {
        if !is_system && status != 1 {
            None
        } else {
            Some(crate::division::effective_code(
                &code,
                is_system,
                base.as_deref(),
            ))
        }
    }))
}

/// Node: `deleteShareReport` — cabut link share (pembuat, atau role company-wide di company share).
pub async fn delete_share_report(state: &AppState, uuid: &str, caller_id: i32) -> ApiResult<Value> {
    let not_found = || ApiError::not_found("Share report not found");
    let share: Option<(i32, i32, Option<i32>)> = sqlx::query_as(
        r#"SELECT id, "userId", "companyId" FROM "shareReport" WHERE uuid = $1::uuid"#,
    )
    .bind(uuid)
    .fetch_optional(&state.db)
    .await?;
    let Some((share_id, owner_id, company_id)) = share else {
        return Err(not_found());
    };

    if owner_id != caller_id {
        let allowed = match company_id {
            Some(c) => {
                let active: Option<i32> = sqlx::query_scalar(
                    r#"SELECT "companyId" FROM "userCompany" WHERE "userId" = $1 AND "companyId" = $2 AND status = 1 LIMIT 1"#,
                )
                .bind(caller_id)
                .bind(c)
                .fetch_optional(&state.db)
                .await?;
                match active {
                    Some(c) => sees_whole_company(
                        role_code_in_company(&state.db, caller_id, c)
                            .await?
                            .as_deref(),
                    ),
                    None => false,
                }
            }
            None => false,
        };
        if !allowed {
            return Err(not_found());
        }
    }

    // Anak dulu: relasi shareReportUsers/Projects -> shareReport tanpa cascade.
    // TODO(audit): shareReport bukan model ber-audit.
    let mut tx = state.db.begin().await?;
    sqlx::query(r#"DELETE FROM "shareReportUsers" WHERE "shareId" = $1"#)
        .bind(share_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(r#"DELETE FROM "shareReportProjects" WHERE "shareId" = $1"#)
        .bind(share_id)
        .execute(&mut *tx)
        .await?;
    let removed = sqlx::query(r#"DELETE FROM "shareReport" WHERE id = $1"#)
        .bind(share_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    // Dicabut request lain di antara cek dan hapus.
    if removed.rows_affected() == 0 {
        return Err(not_found());
    }
    Ok(json!({"success": true, "message": "Share report revoked", "data": Value::Null}))
}

// ─── kirim report lewat email ──────────────────────────────────────────────────

/// `formatDuration`: `2h 5m` / `45m`.
fn format_duration(seconds: i64) -> String {
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m")
    }
}

/// `MailService.sendReportEmail` (Mailtrap + lampiran PDF). `true` bila terkirim.
async fn send_report_email(
    state: &AppState,
    to: &str,
    payload: &SendReportDto,
    sender_name: &str,
    company_name: &str,
    start_date: &str,
    end_date: &str,
    pdf: &[u8],
) -> bool {
    let html = match mz_core::mail::render(
        "report",
        &json!({
            "senderName": sender_name,
            "companyName": company_name,
            "message": payload.message(),
            "startDate": start_date,
            "endDate": end_date,
        }),
    ) {
        Ok(h) => h,
        Err(error) => {
            tracing::error!(%error, "send report email: template error");
            return false;
        }
    };
    use mz_core::mail::{Attachment, Message, Recipient};
    let filename = format!("Time-Report-{start_date}-to-{end_date}.pdf");
    mz_core::mail::deliver(
        state,
        "send report email",
        &Message {
            to: &[Recipient {
                email: to,
                name: None,
            }],
            subject: payload.subject(),
            text: payload.message(),
            html: &html,
            category: "Time Tracking Report",
            attachments: &[Attachment {
                filename: &filename,
                mime: "application/pdf",
                content: pdf,
            }],
            // axios `timeout: 30000` di `sendReportEmail`.
            timeout: std::time::Duration::from_secs(30),
        },
    )
    .await
    .success
}

/// Node: `sendReport` (setelah pemeriksaan `clientProofExportAllowed` di controller).
pub async fn send_report(
    state: &AppState,
    user_id: i32,
    payload: &SendReportDto,
) -> ApiResult<Value> {
    match send_report_inner(state, user_id, payload).await {
        Ok(v) => Ok(v),
        // HttpException dilempar ulang; sisanya jadi `{ success: false }`.
        Err(SendErr::Http(e)) => Err(e),
        Err(SendErr::Other(error)) => {
            tracing::error!(%error, "SEND_REPORT_ERROR");
            Ok(json!({"success": false, "message": "Failed to send report"}))
        }
    }
}

enum SendErr {
    Http(ApiError),
    Other(String),
}

impl From<ApiError> for SendErr {
    fn from(e: ApiError) -> Self {
        Self::Http(e)
    }
}

impl From<sqlx::Error> for SendErr {
    fn from(e: sqlx::Error) -> Self {
        Self::Other(e.to_string())
    }
}

async fn send_report_inner(
    state: &AppState,
    user_id: i32,
    payload: &SendReportDto,
) -> Result<Value, SendErr> {
    let db = &state.db;
    let Some(company_id) = mz_core::company::find_company_id(db, user_id).await? else {
        return Ok(json!({"success": false, "message": "User or company not found"}));
    };
    let ctx: Option<ExportCtx> = sqlx::query_as(
        r#"SELECT u.name, u.username, c.name AS "companyName", c."timeZone", c."dataHistoryMonths"
           FROM users u, company c WHERE u.id = $1 AND c.id = $2"#,
    )
    .bind(user_id)
    .bind(company_id)
    .fetch_optional(db)
    .await?;
    let Some(ctx) = ctx else {
        return Ok(json!({"success": false, "message": "User or company not found"}));
    };
    let is_admin = sees_whole_company(
        role_code_in_company(db, user_id, company_id)
            .await?
            .as_deref(),
    );

    assert_report_range(Some(payload.start_date()), Some(payload.end_date()))?;
    let requested_start = parse_js_date(payload.start_date())
        .ok_or_else(|| SendErr::Other("Invalid time value".into()))?;
    let start_dt = clamp_to_history(requested_start, Some(ctx.data_history_months), now_naive());
    let end_dt = parse_js_date(payload.end_date())
        .ok_or_else(|| SendErr::Other("Invalid time value".into()))?;
    let (start_date, end_date) = (date_part(start_dt), date_part(end_dt));

    // Non-admin hanya boleh mengirim report miliknya sendiri.
    let mut user_ids = payload.user_ids();
    if !is_admin {
        user_ids = vec![user_id];
    } else if user_ids.is_empty() {
        user_ids = sqlx::query_scalar(
            r#"SELECT "userId" FROM "userCompany" WHERE "companyId" = $1 AND status = 1"#,
        )
        .bind(company_id)
        .fetch_all(db)
        .await?;
    }
    let mut project_ids = payload.project_ids();
    if project_ids.is_empty() {
        project_ids = sqlx::query_scalar(
            r#"SELECT "projectId" FROM "projectCompany" WHERE "companyId" = $1 AND status = 1"#,
        )
        .bind(company_id)
        .fetch_all(db)
        .await?;
    }
    let sections = payload.sections();

    let timelines = fetch_timelines(
        db,
        &TimelineFilter {
            company_id,
            project_ids: &project_ids,
            user_ids: Some(&user_ids),
            billable_sql: None,
            start_date: &start_date,
            end_date: &end_date,
        },
    )
    .await?;
    // `projectId in []` = tidak ada hasil (Node: `in: projectIds`).
    let timelines: Vec<TimelineRow> = if project_ids.is_empty() {
        Vec::new()
    } else {
        timelines.into_iter().map(|t| t.row).collect()
    };
    let ids: Vec<i32> = timelines.iter().map(|t| t.id).collect();
    let app_rows = fetch_app_usage(db, &ids).await?;
    let employees = build_work_report_employees(&timelines, &app_rows);

    let report_sections = Sections {
        summary: sections.iter().any(|s| s == "company"),
        projects: sections.iter().any(|s| s == "project"),
        employees: sections.iter().any(|s| s == "user"),
    };
    let total_seconds: i64 = employees.iter().map(|e| e.seconds).sum();
    let total_users = employees.len();
    let total_projects = {
        let mut p: Vec<i32> = timelines.iter().map(|t| t.project_id).collect();
        p.sort_unstable();
        p.dedup();
        p.len()
    };
    let generated_by = if ctx.name.is_empty() {
        ctx.username.clone()
    } else {
        ctx.name.clone()
    };

    let report = ReportData {
        company_name: ctx.company_name.clone(),
        start_date: start_date.clone(),
        end_date: end_date.clone(),
        generated_at: chrono::Utc::now(),
        generated_by: generated_by.clone(),
        time_zone: Some(ctx.time_zone.clone()).filter(|z| !z.is_empty()),
        employees,
        sections: report_sections,
    };
    let pdf = tokio::task::spawn_blocking(move || generate(&report))
        .await
        .map_err(|e| SendErr::Other(e.to_string()))?;

    // Hasil kirim dinormalkan ke { email, ok }; detail error hanya masuk log.
    let emails = payload.emails();
    let results = futures::future::join_all(emails.iter().map(|email| {
        let (pdf, gb, cn, sd, ed) = (
            &pdf,
            &generated_by,
            &ctx.company_name,
            &start_date,
            &end_date,
        );
        async move {
            let ok = send_report_email(state, email, payload, gb, cn, sd, ed, pdf).await;
            (email.clone(), ok)
        }
    }))
    .await;
    let failed: Vec<Value> = results
        .iter()
        .filter(|(_, ok)| !ok)
        .map(|(e, _)| json!({"email": e, "error": "Failed to send email"}))
        .collect();
    let success_count = emails.len() - failed.len();

    let mut data = json!({
        "totalEmails": emails.len(),
        "successCount": success_count,
        "failedCount": failed.len(),
        "reportSummary": {
            "period": format!("{start_date} to {end_date}"),
            "totalTime": format_duration(total_seconds),
            "totalUsers": total_users,
            "totalProjects": total_projects,
        },
    });
    if !failed.is_empty()
        && let Value::Object(map) = &mut data
    {
        map.insert("failedEmails".into(), Value::Array(failed));
    }
    Ok(json!({
        "success": true,
        "message": format!("Report sent to {success_count} of {} recipients", emails.len()),
        "data": data,
    }))
}

/// `company.clientProofExportAllowed` company aktif pemanggil (controller `POST /reports/send`).
pub async fn client_proof_export_allowed(state: &AppState, user_id: i32) -> ApiResult<bool> {
    let Some(company_id) = mz_core::company::find_company_id(&state.db, user_id).await? else {
        return Ok(false);
    };
    let allowed: Option<bool> =
        sqlx::query_scalar(r#"SELECT "clientProofExportAllowed" FROM company WHERE id = $1"#)
            .bind(company_id)
            .fetch_optional(&state.db)
            .await?;
    Ok(allowed.unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes_merge_rules() {
        let n = |v: &[&str]| merge_notes(&v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>());
        assert_eq!(n(&["a", " a ", "-", "", "b"]), "a | b");
        assert_eq!(n(&["-", ""]), "-");
        assert_eq!(n(&[]), "-");
    }

    #[test]
    fn duration_format() {
        assert_eq!(format_duration(0), "0m");
        assert_eq!(format_duration(2700), "45m");
        assert_eq!(format_duration(7500), "2h 5m");
    }

    #[test]
    fn share_screenshot_variant_rules() {
        assert_eq!(
            share_screenshot_ref(Some(7), true, Some("compressed/x.jpg"), "x.jpg"),
            "compressed/x.jpg"
        );
        assert_eq!(
            share_screenshot_ref(Some(36500), true, Some("compressed/x.jpg"), "x.jpg"),
            "x.jpg"
        );
        assert_eq!(
            share_screenshot_ref(None, true, Some("compressed/x.jpg"), "x.jpg"),
            "x.jpg"
        );
        assert_eq!(
            share_screenshot_ref(Some(7), false, Some("compressed/x.jpg"), "x.jpg"),
            "x.jpg"
        );
        assert_eq!(share_screenshot_ref(Some(7), true, None, "x.jpg"), "x.jpg");
    }
}
