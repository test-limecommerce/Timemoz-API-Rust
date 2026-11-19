//! Port `trackings.service.ts`. Fungsi mengembalikan `Value` respons controller
//! (termasuk bentuk `{ success: false, ... }` berstatus 200 yang dipakai Node)
//! atau `ApiError` untuk exception yang dilempar Nest (403, 500).

use std::collections::{BTreeMap, HashMap, HashSet};

use axum::http::StatusCode;
use chrono::{NaiveDate, Utc};
use mz_core::db::is_unique_violation;
use mz_core::enums::sees_whole_company;
use mz_core::time::{JsDate, now_naive, parse_js_date};
use mz_core::{ApiError, ApiResult, AppState};
use serde_json::{Map, Value, json};
use sqlx::{PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::calc::{self, naive_ms};
use crate::division::{
    UserDataDecision, ViewerScope, can_access_user, can_view_user, can_view_user_data,
    latest_role_code_any, leader_may_read_screenshots, resolve_viewer_scope, scope_forbidden,
    user_data_forbidden,
};
use crate::dto::{CountQuery, CreateTrackingDto, TrackingPaginator, UpdateTrackingDto, truthy_str};
use crate::model::{
    FULL_TIMELINE_COLS, FullTimelineRow, IntegrationOut, ListTimelineOut, ListTimelineRow,
    ListUserRow, ProjectRef, ScreenshotOut, TRACKING_COLS, TRACKING_RETURNING, TaskRow,
    TrackingRow, TrackingUserRow, UserCompanyRow,
};

/// Opsi `findAll` untuk poll desktop/mobile.
#[derive(Debug, Clone, Copy, Default)]
pub struct FindAllOptions {
    pub skip_screenshots: bool,
    pub skip_user: bool,
    pub skip_work_limit: bool,
    pub skip_integration: bool,
}

/// `err instanceof ForbiddenException` — dilempar ulang, bukan dibungkus `success:false`.
fn is_forbidden(e: &ApiError) -> bool {
    e.status() == StatusCode::FORBIDDEN
}

/// Prisma menolak string non-UUID untuk kolom `@db.Uuid`.
fn parse_uuid_or_msg(raw: &str) -> Result<Uuid, String> {
    Uuid::parse_str(raw).map_err(|_| {
        format!("Inconsistent column data: Error creating UUID, invalid character: `{raw}`")
    })
}

fn not_found_msg(uuid: &str) -> String {
    format!("Data tracking by uuid {uuid} not found")
}

/// `{ success:false, message, error }` di mana `error` adalah objek `Error` JS
/// yang diserialisasi (`{}`).
fn failure_with_error_obj(message: &str) -> Value {
    json!({ "success": false, "message": message, "error": {} })
}

// ---------------------------------------------------------------------------
// create
// ---------------------------------------------------------------------------

enum Prep {
    UserNotFound,
    Ready {
        user_id: i32,
        target_date: NaiveDate,
    },
}

async fn prepare_create(
    state: &AppState,
    dto: &CreateTrackingDto,
    caller: i32,
) -> Result<Prep, String> {
    let timezone = match &dto.timezone {
        Some(Value::String(s)) => s.as_str(),
        _ => "UTC",
    };
    let mut user_id = caller;
    // 1. Resolve userUuid (audit C4).
    if let Some(raw) = truthy_str(dto.user_uuid.as_ref()) {
        let uuid = parse_uuid_or_msg(raw)?;
        let target: Option<i32> =
            sqlx::query_scalar("SELECT id FROM users WHERE uuid = $1 LIMIT 1")
                .bind(uuid)
                .fetch_optional(&state.db)
                .await
                .map_err(|e| e.to_string())?;
        let Some(t) = target else {
            return Ok(Prep::UserNotFound);
        };
        if !can_access_user(&state.db, caller, t)
            .await
            .map_err(|e| e.to_string())?
        {
            return Ok(Prep::UserNotFound);
        }
        user_id = t;
    }
    // 2. takenDate final.
    let raw = match &dto.taken_date {
        Some(Value::String(s)) => s.as_str(),
        _ => "",
    };
    let taken = parse_js_date(raw).ok_or_else(|| "Invalid Date".to_owned())?;
    let from_given = truthy_str(dto.from.as_ref()).is_some();
    Ok(Prep::Ready {
        user_id,
        target_date: calc::final_taken_date(from_given, taken, timezone, Utc::now()),
    })
}

async fn fetch_tracking(
    db: &PgPool,
    user_id: i32,
    date: NaiveDate,
    only_active: bool,
) -> sqlx::Result<Option<TrackingRow>> {
    let sql = format!(
        r#"SELECT {TRACKING_COLS} FROM tracking t
           WHERE t."userId" = $1 AND t."takenDate" = $2 {}
           ORDER BY t.status DESC, t.id ASC LIMIT 1"#,
        if only_active { "AND t.status = 1" } else { "" }
    );
    sqlx::query_as::<_, TrackingRow>(&sql)
        .bind(user_id)
        .bind(date)
        .fetch_optional(db)
        .await
}

/// Isi `$transaction` di `create`.
async fn create_in_tx(
    db: &PgPool,
    user_id: i32,
    date: NaiveDate,
    is_absent: bool,
) -> sqlx::Result<TrackingRow> {
    let mut tx = db.begin().await?;
    let sql = format!(
        r#"SELECT {TRACKING_COLS} FROM tracking t
           WHERE t."userId" = $1 AND t."takenDate" = $2 AND t.status = 1
           ORDER BY t.id ASC LIMIT 1"#
    );
    let active = sqlx::query_as::<_, TrackingRow>(&sql)
        .bind(user_id)
        .bind(date)
        .fetch_optional(&mut *tx)
        .await?;
    if let Some(row) = active {
        return Ok(row);
    }
    let any_sql = format!(
        r#"SELECT {TRACKING_COLS} FROM tracking t
           WHERE t."userId" = $1 AND t."takenDate" = $2
           ORDER BY t.status DESC, t.id ASC LIMIT 1"#
    );
    let any = sqlx::query_as::<_, TrackingRow>(&any_sql)
        .bind(user_id)
        .bind(date)
        .fetch_optional(&mut *tx)
        .await?;
    let row = if let Some(existing) = any {
        // Aktifkan kembali (status 0 -> 1); `updatedAt` tidak disentuh Prisma.
        let sql = format!(
            r#"UPDATE tracking SET "isAbsent" = $2, status = 1 WHERE id = $1 RETURNING {TRACKING_RETURNING}"#
        );
        sqlx::query_as::<_, TrackingRow>(&sql)
            .bind(existing.id)
            .bind(is_absent)
            .fetch_one(&mut *tx)
            .await?
    } else {
        let sql = format!(
            r#"INSERT INTO tracking ("isAbsent", "takenDate", "userId", status)
               VALUES ($1, $2, $3, 1) RETURNING {TRACKING_RETURNING}"#
        );
        sqlx::query_as::<_, TrackingRow>(&sql)
            .bind(is_absent)
            .bind(date)
            .bind(user_id)
            .fetch_one(&mut *tx)
            .await?
    };
    tx.commit().await?;
    Ok(row)
}

/// Node: src/trackings/trackings.service.ts:107 `create`
pub async fn create(state: &AppState, dto: &CreateTrackingDto, caller: i32) -> ApiResult<Value> {
    let is_absent = matches!(dto.is_absent, Some(Value::Bool(true)));
    let (user_id, target_date) = match prepare_create(state, dto, caller).await {
        Ok(Prep::UserNotFound) => {
            return Ok(json!({ "success": false, "message": "user not found" }));
        }
        Ok(Prep::Ready {
            user_id,
            target_date,
        }) => (user_id, target_date),
        Err(message) => {
            tracing::error!(module = "tracking-create", error = %message, "create tracking failed");
            return Ok(json!({ "success": false, "message": message, "data": null }));
        }
    };

    match create_in_tx(&state.db, user_id, target_date, is_absent).await {
        Ok(row) => {
            Ok(json!({ "success": true, "message": "register tracking success", "data": row }))
        }
        Err(e) if is_unique_violation(&e) => {
            // P2002 (pengaman ganda): baca ulang dengan takenDate mentah dari klien.
            let raw = match &dto.taken_date {
                Some(Value::String(s)) => s.as_str(),
                _ => "",
            };
            let date = parse_js_date(raw)
                .map(|d| d.date())
                .ok_or_else(|| ApiError::internal(anyhow::anyhow!("Invalid Date")))?;
            let row = fetch_tracking(&state.db, user_id, date, false).await?;
            Ok(json!({ "success": true, "message": "register tracking success", "data": row }))
        }
        Err(e) => {
            tracing::error!(module = "tracking-create", error = %e, "create tracking failed");
            Ok(json!({ "success": false, "message": e.to_string(), "data": null }))
        }
    }
}

// ---------------------------------------------------------------------------
// server date time + fix
// ---------------------------------------------------------------------------

/// Node: src/trackings/trackings.service.ts:206 `getServerDateTime`.
/// Format tanggal `YYYY-DD-MM` memang begitu di Node.
pub fn server_date_time() -> Value {
    let now = Utc::now();
    json!({
        "success": true,
        "message": "get server date time",
        "data": {
            "date": now.format("%Y-%d-%m").to_string(),
            "time": now.format("%H:%M:%S").to_string(),
        }
    })
}

/// Node: src/trackings/trackings.service.ts:1015 `fix`. Hasilnya dibuang
/// controller; galat ditelan seperti `try/catch` Node.
pub async fn fix(state: &AppState, taken_date: Option<&str>, user_uuid: Option<&str>, caller: i32) {
    if let Err(e) = fix_inner(state, taken_date, user_uuid, caller).await {
        tracing::warn!(error = %e, "tracking fix failed");
    }
}

async fn fix_inner(
    state: &AppState,
    taken_date: Option<&str>,
    user_uuid: Option<&str>,
    caller: i32,
) -> Result<(), String> {
    let Some(raw) = taken_date.filter(|s| !s.is_empty()) else {
        return Ok(());
    };
    let taken = parse_js_date(raw).ok_or_else(|| "Invalid Date".to_owned())?;
    // `dayjs.utc(x).hour(0).minute(0).second(0)`: milidetik tetap dibawa.
    let ms = chrono::Timelike::nanosecond(&taken) / 1_000_000 % 1000;
    let day_ts = taken
        .date()
        .and_hms_milli_opt(0, 0, 0, ms)
        .ok_or_else(|| "bad date".to_owned())?;
    let day = taken.date();

    // Deviasi: `where: { uuid: undefined }` di Node memilih user pertama mana pun;
    // tanpa `userUuid` fungsi ini tidak melakukan apa-apa.
    let Some(raw_uuid) = user_uuid else {
        return Ok(());
    };
    let uuid = parse_uuid_or_msg(raw_uuid)?;
    let user: Option<i32> = sqlx::query_scalar("SELECT id FROM users WHERE uuid = $1 LIMIT 1")
        .bind(uuid)
        .fetch_optional(&state.db)
        .await
        .map_err(|e| e.to_string())?;
    let Some(user_id) = user else { return Ok(()) };
    if !can_access_user(&state.db, caller, user_id)
        .await
        .map_err(|e| e.to_string())?
    {
        return Ok(());
    }
    let target: Option<i32> = sqlx::query_scalar(
        r#"SELECT id FROM tracking WHERE "userId" = $1 AND "takenDate" = $2 AND status = 1 ORDER BY id LIMIT 1"#,
    )
    .bind(user_id)
    .bind(day)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| e.to_string())?;
    let Some(tracking_id) = target else {
        return Ok(());
    };
    // Setara loop Node: hanya timeline milik user itu yang dipindah (cek
    // `tmpTracking.userId === user.id`), tanpa memindai timeline semua company.
    sqlx::query(
        r#"UPDATE "trackingTimeline" tt SET "trackingId" = $1
           FROM tracking t
           WHERE tt."trackingId" = t.id AND t."userId" = $2 AND t."takenDate" < $3
             AND tt."startAt" >= $4 AND tt.status = 1"#,
    )
    .bind(tracking_id)
    .bind(user_id)
    .bind(day)
    .bind(day_ts)
    .execute(&state.db)
    .await
    .map_err(|e| e.to_string())?;
    Ok(())
}

// ---------------------------------------------------------------------------
// findAll
// ---------------------------------------------------------------------------

enum FindAllErr {
    /// Exception Nest yang dilempar ulang (403).
    Rethrow(ApiError),
    /// Ditangkap `catch` -> `{ success:false, error: err.message }`.
    Msg(String),
}

impl From<sqlx::Error> for FindAllErr {
    fn from(e: sqlx::Error) -> Self {
        Self::Msg(e.to_string())
    }
}

impl From<ApiError> for FindAllErr {
    fn from(e: ApiError) -> Self {
        if is_forbidden(&e) {
            Self::Rethrow(e)
        } else {
            Self::Msg(e.to_string())
        }
    }
}

impl From<String> for FindAllErr {
    fn from(m: String) -> Self {
        Self::Msg(m)
    }
}

/// `orderBy: { [orderCol]: orderDir }` — hanya kolom skalar `tracking` dan
/// `asc`/`desc` yang diterima Prisma; selain itu Prisma melempar.
pub fn order_clause(
    col: Option<&str>,
    dir: Option<&str>,
) -> Result<(&'static str, &'static str), String> {
    let col = col.filter(|s| !s.is_empty()).unwrap_or("createdAt");
    let dir = dir.filter(|s| !s.is_empty()).unwrap_or("desc");
    let column = match col {
        "id" => "id",
        "uuid" => "uuid",
        "takenDate" => "takenDate",
        "isAbsent" => "isAbsent",
        "userId" => "userId",
        "totalTakenInSecond" => "totalTakenInSecond",
        "createdAt" => "createdAt",
        "updatedAt" => "updatedAt",
        "status" => "status",
        other => {
            return Err(format!(
                "Unknown argument `{other}`. Available options are listed in green."
            ));
        }
    };
    let direction = match dir {
        "asc" => "ASC",
        "desc" => "DESC",
        other => {
            return Err(format!(
                "Invalid value for argument `{col}`: Expected SortOrder, got `{other}`"
            ));
        }
    };
    Ok((column, direction))
}

/// `/screenshots/{uuid}/image?variant=display` (`screenshotImagePath`).
pub fn screenshot_image_path(uuid: &str) -> String {
    let encoded: String = uuid
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
                char::from(b).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    format!("/screenshots/{encoded}/image?variant=display")
}

/// Node: src/trackings/trackings.service.ts:215 `findAll`
pub async fn find_all(
    state: &AppState,
    query: &TrackingPaginator,
    user_id: i32,
    opts: FindAllOptions,
) -> ApiResult<Value> {
    match find_all_inner(state, query, user_id, opts).await {
        Ok(v) => Ok(v),
        Err(FindAllErr::Rethrow(e)) => Err(e),
        Err(FindAllErr::Msg(m)) => {
            tracing::error!(user_id, error = %m, "findAll tracking failed");
            Ok(json!({ "success": false, "message": "cannot get findAll tracking", "error": m }))
        }
    }
}

async fn find_all_inner(
    state: &AppState,
    query: &TrackingPaginator,
    user_id: i32,
    opts: FindAllOptions,
) -> Result<Value, FindAllErr> {
    let db = &state.db;
    // 1. user + role terbaru + company.
    let own_uuid: Option<String> = sqlx::query_scalar("SELECT uuid::text FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_optional(db)
        .await?;
    let Some(own_uuid) = own_uuid else {
        return Ok(json!({ "success": false, "message": "User not found", "data": null }));
    };
    let role = latest_role_code_any(db, user_id).await?;
    let is_privileged = sees_whole_company(role.as_deref());
    let taken = query
        .taken_date
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(parse_js_date);

    let Some(company_id) = mz_core::company::find_company_id(db, user_id).await? else {
        return Ok(
            json!({ "success": false, "message": "No company found for user", "data": null }),
        );
    };

    // 2. Resolve target user.
    let user_uuid = query.user_uuid.as_deref().filter(|s| !s.is_empty());
    let target_user_id: Option<i32> = if !is_privileged {
        let mut target = Some(user_id);
        if let Some(uu) = user_uuid.filter(|uu| *uu != own_uuid) {
            let uuid = parse_uuid_or_msg(uu)?;
            let found: Option<i32> =
                sqlx::query_scalar("SELECT id FROM users WHERE uuid = $1 LIMIT 1")
                    .bind(uuid)
                    .fetch_optional(db)
                    .await?;
            if found != Some(user_id) {
                let scope = resolve_viewer_scope(db, user_id, company_id).await?;
                if matches!(scope, ViewerScope::Division { .. })
                    && leader_may_read_screenshots(state, user_id, company_id).await?
                {
                    match found {
                        Some(t) if can_view_user(&scope, user_id, t) => target = Some(t),
                        _ => return Err(FindAllErr::Rethrow(scope_forbidden())),
                    }
                }
            }
        }
        target
    } else if let Some(uu) = user_uuid {
        let uuid = parse_uuid_or_msg(uu)?;
        let found: Option<i32> = sqlx::query_scalar("SELECT id FROM users WHERE uuid = $1 LIMIT 1")
            .bind(uuid)
            .fetch_optional(db)
            .await?;
        Some(found.unwrap_or(0))
    } else {
        None
    };

    // 3. Query utama. Pagar company berdiri sendiri di WHERE (§1.12).
    let taken_date: Option<NaiveDate> = match taken {
        Some(Some(dt)) => Some(dt.date()),
        Some(None) => return Err("Invalid Date".to_owned().into()),
        None => None,
    };
    let (col, dir) = order_clause(query.order_column.as_deref(), query.order_by.as_deref())?;
    let mut qb = QueryBuilder::<Postgres>::new(format!(
        "SELECT {TRACKING_COLS} FROM tracking t WHERE t.status = 1"
    ));
    if let Some(d) = taken_date {
        qb.push(r#" AND t."takenDate" = "#).push_bind(d);
    }
    if let Some(u) = target_user_id {
        qb.push(r#" AND t."userId" = "#).push_bind(u);
    }
    qb.push(r#" AND EXISTS (SELECT 1 FROM "userCompany" uc WHERE uc."userId" = t."userId" AND uc."companyId" = "#)
        .push_bind(company_id)
        .push(" AND uc.status = 1)");
    qb.push(format!(r#" ORDER BY t."{col}" {dir}, t.id ASC LIMIT 1"#));
    let trackings: Vec<TrackingRow> = qb.build_query_as().fetch_all(db).await?;

    let tracking_ids: Vec<i32> = trackings.iter().map(|t| t.id).collect();
    let timelines: Vec<ListTimelineRow> = if tracking_ids.is_empty() {
        Vec::new()
    } else {
        sqlx::query_as(
            r#"SELECT tt.id, tt.uuid::text AS uuid, tt."trackingId", tt.note, tt."isOffline", tt."startAt", tt."endAt",
                      tt."startAtUtc", tt."endAtUtc", tt.timezone, tt."offsetMinutes", tt."createdAt", tt."updatedAt",
                      tt.status, tt."clickupTimeEntryId", tt."jiraWorklogId",
                      tt."manualTimeStatus"::text AS "manualTimeStatus",
                      tt."manualTimeApprovalSource"::text AS "manualTimeApprovalSource",
                      tt."taskId", p.id AS "projectId", p.uuid::text AS "projectUuid",
                      p.code AS "projectCode", p.name AS "projectName"
               FROM "trackingTimeline" tt
               JOIN project p ON p.id = tt."projectId"
               WHERE tt."trackingId" = ANY($1) AND tt.status = 1
               ORDER BY tt."startAt" ASC, tt.id ASC"#,
        )
        .bind(&tracking_ids)
        .fetch_all(db)
        .await?
    };
    let timeline_ids: Vec<i32> = timelines.iter().map(|t| t.id).collect();

    // 3b. Screenshot (status 3).
    let mut screenshots: HashMap<i32, Vec<ScreenshotOut>> = HashMap::new();
    if !timeline_ids.is_empty() && !opts.skip_screenshots {
        let rows: Vec<(i32, String, bool, JsDate)> = sqlx::query_as(
            r#"SELECT s."trackingTimelineId", s.uuid::text, s."isBlur", s."takenAt"
               FROM screenshot s
               WHERE s."trackingTimelineId" = ANY($1) AND s.status = 3
               ORDER BY s."trackingTimelineId" ASC, s."takenAt" ASC, s.id ASC"#,
        )
        .bind(&timeline_ids)
        .fetch_all(db)
        .await?;
        for (tl, uuid, is_blur, taken_at) in rows {
            screenshots.entry(tl).or_default().push(ScreenshotOut {
                screenshot_url: screenshot_image_path(&uuid),
                uuid,
                is_blur,
                taken_at,
            });
        }
    }

    // 3c. Integrasi (task) — dua tingkat.
    let integrations = if opts.skip_integration {
        HashMap::new()
    } else {
        load_integrations(db, &timelines).await?
    };

    // 4. Total detik dihitung di memori.
    let mut by_tracking: BTreeMap<i32, Vec<ListTimelineRow>> = BTreeMap::new();
    for t in timelines {
        by_tracking.entry(t.tracking_id).or_default().push(t);
    }

    let mut result: Vec<Value> = Vec::new();
    let mut first_total: i64 = 0;
    for (idx, tr) in trackings.iter().enumerate() {
        let mut tls = by_tracking.remove(&tr.id).unwrap_or_default();
        if tls
            .iter()
            .any(|t| t.timezone.as_deref().is_none_or(str::is_empty))
        {
            tls.sort_by_key(|t| calc::legacy_aware_utc_ms(t.start_at.0, t.timezone.as_deref()));
        }
        let ranges: Vec<(i64, i64)> = tls
            .iter()
            .map(|t| (naive_ms(t.start_at.0), naive_ms(t.end_at.0)))
            .collect();
        let total_second = calc::total_seconds(&ranges);
        if idx == 0 {
            first_total = total_second;
        }
        let out_tls: Vec<ListTimelineOut> = tls
            .into_iter()
            .map(|t| ListTimelineOut {
                screenshot: screenshots.remove(&t.id).unwrap_or_default(),
                integration: integrations.get(&t.id).cloned(),
                project: ProjectRef {
                    id: t.project_id,
                    uuid: t.project_uuid,
                    code: t.project_code,
                    name: t.project_name,
                },
                id: t.id,
                uuid: t.uuid,
                note: t.note,
                is_offline: t.is_offline,
                start_at: t.start_at,
                end_at: t.end_at,
                start_at_utc: t.start_at_utc,
                end_at_utc: t.end_at_utc,
                timezone: t.timezone,
                offset_minutes: t.offset_minutes,
                created_at: t.created_at,
                updated_at: t.updated_at,
                status: t.status,
                clickup_time_entry_id: t.clickup_time_entry_id,
                jira_worklog_id: t.jira_worklog_id,
                manual_time_status: t.manual_time_status,
                manual_time_approval_source: t.manual_time_approval_source,
            })
            .collect();

        let mut obj = to_object(tr)?;
        obj.insert("totalTakenInSecond".into(), Value::from(total_second));
        if !opts.skip_user {
            obj.insert(
                "user".into(),
                load_list_user(db, tr.user_id, is_privileged, company_id).await?,
            );
        }
        obj.insert("trackingTimeline".into(), to_value(&out_tls)?);
        result.push(Value::Object(obj));
    }

    let total = result.len();
    let work_limit = match target_user_id {
        Some(t) if t != 0 && !opts.skip_work_limit => {
            let day = taken_date.unwrap_or_else(|| Utc::now().date_naive());
            get_work_limit_data(db, t, company_id, day, first_total).await?
        }
        _ => Value::Null,
    };

    Ok(json!({
        "success": true,
        "message": "Success get all data tracking",
        "data": {
            "total": total,
            "itemsPerPage": total,
            "pageNumber": 1,
            "result": result,
            "workLimit": work_limit,
        }
    }))
}

fn to_value<T: serde::Serialize>(v: &T) -> Result<Value, FindAllErr> {
    serde_json::to_value(v).map_err(|e| FindAllErr::Msg(e.to_string()))
}

fn to_object<T: serde::Serialize>(v: &T) -> Result<Map<String, Value>, FindAllErr> {
    match to_value(v)? {
        Value::Object(m) => Ok(m),
        _ => Err(FindAllErr::Msg("not an object".into())),
    }
}

async fn load_list_user(
    db: &PgPool,
    user_id: i32,
    privileged: bool,
    company_id: i32,
) -> Result<Value, FindAllErr> {
    let user: Option<ListUserRow> = sqlx::query_as(
        r#"SELECT id, uuid::text AS uuid, username, name, "createdAt", "updatedAt", "sentAt", status
           FROM users WHERE id = $1"#,
    )
    .bind(user_id)
    .fetch_optional(db)
    .await?;
    let Some(user) = user else {
        return Ok(Value::Null);
    };
    let mut obj = to_object(&user)?;
    if privileged {
        let rows: Vec<UserCompanyRow> = sqlx::query_as(
            r#"SELECT id, uuid::text AS uuid, "userId", "companyId", "createdAt", "updatedAt", status,
                      "isScreenshotActive", "isAllowManualTime", "isBlur", "isAllowEditTime",
                      "screenshotPerHour", "idleTimeInSecond", "isDefault", "invitedById", "inviteMethod"
               FROM "userCompany" WHERE "userId" = $1 AND "companyId" = $2 ORDER BY id"#,
        )
        .bind(user_id)
        .bind(company_id)
        .fetch_all(db)
        .await?;
        obj.insert("userCompany".into(), to_value(&rows)?);
    }
    Ok(Value::Object(obj))
}

const TASK_COLS: &str =
    r#"id, "projectId", uuid::text AS uuid, source, name, "externalId", "externalUrl""#;

async fn load_integrations(
    db: &PgPool,
    timelines: &[ListTimelineRow],
) -> Result<HashMap<i32, IntegrationOut>, FindAllErr> {
    let mut out: HashMap<i32, IntegrationOut> = HashMap::new();
    if timelines.is_empty() {
        return Ok(out);
    }
    // Level 1: berbasis taskId.
    let task_ids: Vec<i32> = timelines
        .iter()
        .filter_map(|t| t.task_id)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    if !task_ids.is_empty() {
        let tasks: Vec<TaskRow> =
            sqlx::query_as(&format!("SELECT {TASK_COLS} FROM task WHERE id = ANY($1)"))
                .bind(&task_ids)
                .fetch_all(db)
                .await?;
        for t in timelines {
            let Some(tid) = t.task_id else { continue };
            if let Some(task) = tasks.iter().find(|k| k.id == tid) {
                out.insert(
                    t.id,
                    IntegrationOut {
                        platform: task.source.clone(),
                        task: Some(task.to_out()),
                    },
                );
            }
        }
    }
    // Level 2: fallback per project untuk clickup/jira.
    let mut metas: Vec<(i32, i32, &'static str)> = Vec::new();
    for t in timelines {
        if out.contains_key(&t.id) {
            continue;
        }
        if t.clickup_time_entry_id
            .as_deref()
            .is_some_and(|s| !s.is_empty())
        {
            metas.push((t.id, t.project_id, "clickup"));
        } else if t.jira_worklog_id.as_deref().is_some_and(|s| !s.is_empty()) {
            metas.push((t.id, t.project_id, "jira"));
        }
    }
    if !metas.is_empty() {
        let ids_for = |platform: &str| -> Vec<i32> {
            metas
                .iter()
                .filter(|m| m.2 == platform)
                .map(|m| m.1)
                .collect::<HashSet<_>>()
                .into_iter()
                .collect()
        };
        let tasks: Vec<TaskRow> = sqlx::query_as(&format!(
            r#"SELECT {TASK_COLS} FROM task
               WHERE ("projectId" = ANY($1) AND source = 'clickup') OR ("projectId" = ANY($2) AND source = 'jira')
               ORDER BY "lastSyncedAt" DESC, id DESC"#
        ))
        .bind(ids_for("clickup"))
        .bind(ids_for("jira"))
        .fetch_all(db)
        .await?;
        let mut by_key: HashMap<(i32, &str), &TaskRow> = HashMap::new();
        for task in &tasks {
            by_key
                .entry((task.project_id, task.source.as_str()))
                .or_insert(task);
        }
        for (timeline_id, project_id, platform) in metas {
            out.insert(
                timeline_id,
                IntegrationOut {
                    platform: platform.to_owned(),
                    task: by_key.get(&(project_id, platform)).map(|t| t.to_out()),
                },
            );
        }
    }
    Ok(out)
}

/// Node: src/trackings/trackings.service.ts:1135 `getWorkLimitData`
async fn get_work_limit_data(
    db: &PgPool,
    user_id: i32,
    company_id: i32,
    taken: NaiveDate,
    daily_worked: i64,
) -> Result<Value, FindAllErr> {
    let limit: Option<(Option<i32>, Option<i32>)> = sqlx::query_as(
        r#"SELECT "dailyLimitSec", "weeklyLimitSec" FROM "userWorkTimeLimit"
           WHERE "userId" = $1 AND "companyId" = $2 AND status = 1 ORDER BY id LIMIT 1"#,
    )
    .bind(user_id)
    .bind(company_id)
    .fetch_optional(db)
    .await?;
    let Some((daily_limit, weekly_limit)) = limit else {
        return Ok(Value::Null);
    };
    if daily_limit.is_none() && weekly_limit.is_none() {
        return Ok(Value::Null);
    }
    let week_start = calc::week_start_monday(taken);
    let rows: Vec<(JsDate, JsDate)> = sqlx::query_as(
        r#"SELECT tt."startAt", tt."endAt"
           FROM "trackingTimeline" tt JOIN tracking t ON t.id = tt."trackingId"
           WHERE tt.status = 1 AND t."userId" = $1 AND t."takenDate" >= $2 AND t."takenDate" < $3"#,
    )
    .bind(user_id)
    .bind(week_start)
    .bind(taken)
    .fetch_all(db)
    .await?;
    let ranges: Vec<(i64, i64)> = rows
        .iter()
        .map(|(s, e)| (naive_ms(s.0), naive_ms(e.0)))
        .collect();
    let other = calc::total_seconds(&ranges);
    Ok(
        match calc::work_limit_calc(daily_limit, weekly_limit, daily_worked, other) {
            None => Value::Null,
            Some(c) => json!({
                "dailyLimitSec": c.daily_limit_sec,
                "dailyWorkedSec": c.daily_worked_sec,
                "dailyOvertimeSec": c.daily_overtime_sec,
                "weeklyLimitSec": c.weekly_limit_sec,
                "weeklyWorkedSec": c.weekly_worked_sec,
                "weeklyOvertimeSec": c.weekly_overtime_sec,
            }),
        },
    )
}

// ---------------------------------------------------------------------------
// findOne
// ---------------------------------------------------------------------------

/// Node: src/trackings/trackings.service.ts:497 `findOne`
pub async fn find_one(
    state: &AppState,
    uuid_raw: &str,
    uuid: Uuid,
    viewer: i32,
) -> ApiResult<Value> {
    let db = &state.db;
    let row: Option<TrackingRow> = sqlx::query_as(&format!(
        "SELECT {TRACKING_COLS} FROM tracking t WHERE t.uuid = $1 AND t.status = 1 ORDER BY t.id LIMIT 1"
    ))
    .bind(uuid)
    .fetch_optional(db)
    .await?;
    let not_found = json!({ "success": false, "message": not_found_msg(uuid_raw), "data": null });
    let Some(tr) = row else { return Ok(not_found) };

    let user: Option<TrackingUserRow> = sqlx::query_as(
        "SELECT id, uuid::text AS uuid, username, name, status FROM users WHERE id = $1",
    )
    .bind(tr.user_id)
    .fetch_optional(db)
    .await?;
    let timelines: Vec<FullTimelineRow> = sqlx::query_as(&format!(
        r#"SELECT {FULL_TIMELINE_COLS} FROM "trackingTimeline" tt
           WHERE tt."trackingId" = $1 AND tt.status = 1 ORDER BY tt.id"#
    ))
    .bind(tr.id)
    .fetch_all(db)
    .await?;

    // FR-032: pemanggil harus boleh membaca data pemilik baris.
    match can_view_user_data(state, viewer, tr.user_id).await? {
        UserDataDecision::DenyLeader => return Err(scope_forbidden()),
        UserDataDecision::Deny => return Ok(not_found),
        UserDataDecision::Allow => {}
    }
    Ok(json!({
        "success": true,
        "message": format!("Success get data tracking by uuid {uuid_raw}"),
        "data": {
            "id": tr.id,
            "uuid": tr.uuid,
            "takenDate": tr.taken_date,
            "isAbsent": tr.is_absent,
            "totalTakenInSecond": tr.total_taken_in_second,
            "createdAt": tr.created_at,
            "updatedAt": tr.updated_at,
            "userId": tr.user_id,
            "user": user,
            "trackingTimeline": timelines,
        }
    }))
}

// ---------------------------------------------------------------------------
// update / remove
// ---------------------------------------------------------------------------

/// Node: src/trackings/trackings.service.ts:557 `update`
pub async fn update(
    state: &AppState,
    uuid: Uuid,
    dto: &UpdateTrackingDto,
    caller: i32,
) -> ApiResult<Value> {
    let failure = json!({ "success": false, "message": "cannot update tracking", "data": null });
    match update_inner(state, uuid, dto, caller).await {
        Ok(Some(row)) => {
            Ok(json!({ "success": true, "message": "update tracking success", "data": row }))
        }
        Ok(None) => Ok(failure),
        Err(m) => {
            tracing::error!(module = "tracking-update", error = %m, "update tracking failed");
            Ok(failure)
        }
    }
}

async fn update_inner(
    state: &AppState,
    uuid: Uuid,
    dto: &UpdateTrackingDto,
    caller: i32,
) -> Result<Option<TrackingRow>, String> {
    let existing: Option<(i32, i32)> =
        sqlx::query_as(r#"SELECT id, "userId" FROM tracking WHERE uuid = $1 ORDER BY id LIMIT 1"#)
            .bind(uuid)
            .fetch_optional(&state.db)
            .await
            .map_err(|e| e.to_string())?;
    let Some((id, owner)) = existing else {
        return Ok(None);
    };
    if !can_access_user(&state.db, caller, owner)
        .await
        .map_err(|e| e.to_string())?
    {
        return Ok(None);
    }

    // `moment.utc(takenDate)` (tanpa nilai = sekarang) lalu dikirim ke kolom
    // `date`: yang dipakai tanggal UTC instan itu.
    let taken_date: NaiveDate = match &dto.taken_date {
        None => Utc::now().date_naive(),
        Some(Value::String(s)) => parse_js_date(s)
            .ok_or_else(|| "Invalid date".to_owned())?
            .date(),
        Some(_) => return Err("Invalid date".into()),
    };

    let mut qb = QueryBuilder::<Postgres>::new(r#"UPDATE tracking SET "takenDate" = "#);
    qb.push_bind(taken_date);
    qb.push(", status = 1, \"updatedAt\" = ")
        .push_bind(JsDate(now_naive()));
    match &dto.is_absent {
        Some(Value::Bool(b)) => {
            qb.push(", \"isAbsent\" = ").push_bind(*b);
        }
        Some(Value::Null) => {
            qb.push(", \"isAbsent\" = NULL");
        }
        _ => {}
    }
    qb.push(" WHERE id = ").push_bind(id);
    qb.push(format!(" RETURNING {TRACKING_RETURNING}"));
    let row: TrackingRow = qb
        .build_query_as()
        .fetch_one(&state.db)
        .await
        .map_err(|e| e.to_string())?;
    Ok(Some(row))
}

/// Node: src/trackings/trackings.service.ts:631 `remove`
pub async fn remove(state: &AppState, uuid: Uuid, uuid_raw: &str, caller: i32) -> ApiResult<Value> {
    let existing: Option<(i32, i32)> =
        sqlx::query_as(r#"SELECT id, "userId" FROM tracking WHERE uuid = $1 ORDER BY id LIMIT 1"#)
            .bind(uuid)
            .fetch_optional(&state.db)
            .await?;
    let allowed = match existing {
        Some((_, owner)) => can_access_user(&state.db, caller, owner).await?,
        None => false,
    };
    let Some((id, _)) = existing.filter(|_| allowed) else {
        return Ok(json!({ "success": false, "message": not_found_msg(uuid_raw), "data": null }));
    };
    sqlx::query("UPDATE tracking SET status = 0 WHERE id = $1")
        .bind(id)
        .execute(&state.db)
        .await?;
    Ok(
        json!({ "success": true, "message": "delete tracking success", "data": { "isDeleted": true } }),
    )
}

// ---------------------------------------------------------------------------
// count
// ---------------------------------------------------------------------------

/// Node: src/trackings/trackings.service.ts:655 `countTotalTracking`
pub async fn count_total(
    state: &AppState,
    user_uuid: Uuid,
    query: &CountQuery,
    viewer: i32,
) -> ApiResult<Value> {
    match count_inner(state, user_uuid, query, viewer).await {
        Ok(v) => Ok(v),
        Err(e) if is_forbidden(&e) => Err(e),
        Err(e) => {
            tracing::error!(module = "tracking-count-tracking", error = %e, "countTotalTracking failed");
            Ok(failure_with_error_obj("cannot get countTotalTracking"))
        }
    }
}

async fn count_inner(
    state: &AppState,
    user_uuid: Uuid,
    query: &CountQuery,
    viewer: i32,
) -> ApiResult<Value> {
    let db = &state.db;
    let target: Option<i32> = sqlx::query_scalar("SELECT id FROM users WHERE uuid = $1 LIMIT 1")
        .bind(user_uuid)
        .fetch_optional(db)
        .await?;
    if let Some(t) = target {
        match can_view_user_data(state, viewer, t).await? {
            UserDataDecision::DenyLeader => return Err(scope_forbidden()),
            UserDataDecision::Deny => return Err(user_data_forbidden()),
            UserDataDecision::Allow => {}
        }
    }

    let now = Utc::now();
    let (first, last) = calc::month_range(
        now.date_naive(),
        query.month.as_deref(),
        query.year.as_deref(),
    )
    .ok_or_else(|| ApiError::internal(anyhow::anyhow!("Invalid Date")))?;
    let date_array = calc::dates_between(first, last);

    // `moment(query.date)` tanpa nilai = sekarang; tak terbaca = pekan kosong.
    let week_anchor: Option<NaiveDate> = match query.date.as_deref() {
        None => Some(now.date_naive()),
        Some(s) => parse_js_date(s).map(|d| d.date()),
    };
    let week = week_anchor.map(calc::week_dates).unwrap_or_default();
    // `new Date(query.date)`: tanpa nilai = Invalid Date, jadi hourCount kosong.
    let today: Option<NaiveDate> = query
        .date
        .as_deref()
        .and_then(parse_js_date)
        .map(|d| d.date());

    let rows: Vec<(i32, JsDate, Option<JsDate>, Option<JsDate>)> = sqlx::query_as(
        r#"SELECT t.id, t."takenDate"::timestamp, tt."startAt", tt."endAt"
           FROM tracking t
           JOIN users u ON u.id = t."userId"
           LEFT JOIN "trackingTimeline" tt ON tt."trackingId" = t.id AND tt.status = 1
           WHERE u.uuid = $1 AND t."takenDate" >= $2 AND t."takenDate" <= $3 AND t.status = 1
           ORDER BY t.id ASC, tt."startAt" ASC, tt.id ASC"#,
    )
    .bind(user_uuid)
    .bind(first)
    .bind(last)
    .fetch_all(db)
    .await?;

    // tanggal -> daftar (startAt, endAt) ms; entri kosong tetap dianggap "ada".
    let mut tracking_map: HashMap<NaiveDate, Vec<(i64, i64)>> = HashMap::new();
    let mut current: Option<(i32, NaiveDate)> = None;
    for (tid, taken, start, end) in rows {
        let date = taken.0.date();
        if current != Some((tid, date)) {
            tracking_map.insert(date, Vec::new());
            current = Some((tid, date));
        }
        if let (Some(s), Some(e)) = (start, end) {
            tracking_map
                .entry(date)
                .or_default()
                .push((naive_ms(s.0), naive_ms(e.0)));
        }
    }

    let mut total_month: i64 = 0;
    let month_count_date: Vec<Value> = date_array
        .iter()
        .map(|d| match tracking_map.get(d) {
            Some(tl) => {
                let secs = calc::total_seconds(tl);
                total_month += secs;
                json!({
                    "takenDate": d.format("%Y-%m-%d").to_string(),
                    "totalTakenInSecond": secs,
                    "percentage": calc::js_number(calc::day_percentage(secs)),
                })
            }
            None => json!({
                "takenDate": d.format("%Y-%m-%d").to_string(),
                "totalTakenInSecond": null,
                "percentage": 0,
            }),
        })
        .collect();

    let total_week: i64 = week
        .iter()
        .filter_map(|d| tracking_map.get(d))
        .map(|tl| calc::total_seconds(tl))
        .sum();

    let hour_count = match today.and_then(|d| tracking_map.get(&d).map(|tl| (d, tl))) {
        Some((d, tl)) if !tl.is_empty() => calc::hour_count(d, tl),
        _ => Vec::new(),
    };

    Ok(json!({
        "success": true,
        "message": "Success get total tracking",
        "data": {
            "totalTakenSecondOfWeek": total_week,
            "totalTakenSecondOfMonth": total_month,
            "monthCountDate": month_count_date,
            "hourCount": hour_count,
        }
    }))
}

// ---------------------------------------------------------------------------
// sync-local-db
// ---------------------------------------------------------------------------

/// Node: src/trackings/trackings.service.ts:1196 `createSyncLocal`.
///
/// Di Node `payload.data` selalu string (DTO `@IsString`), jadi loop
/// `timelineUpdates` mengiterasi karakter dan tidak pernah menghasilkan update;
/// yang berjalan hanya `syncDbLocal.create`.
pub async fn create_sync_local(
    state: &AppState,
    taken_date: Option<&str>,
    user_id: i32,
    data: &str,
) -> Value {
    let fail = || failure_with_error_obj("cannot sync local db");
    let date = match taken_date {
        None => Utc::now().date_naive(),
        Some(s) => match parse_js_date(s) {
            Some(d) => d.date(),
            None => {
                tracing::error!(function = "createSyncLocal", "invalid takenDate");
                return fail();
            }
        },
    };
    let res = sqlx::query(
        r#"INSERT INTO "syncDbLocal" ("userId", data, "takenDate") VALUES ($1, $2, $3)"#,
    )
    .bind(user_id)
    .bind(data)
    .bind(date)
    .execute(&state.db)
    .await;
    match res {
        Ok(_) => json!({ "success": true, "message": "sync local db has been created" }),
        Err(e) => {
            tracing::error!(function = "createSyncLocal", module = "TrackingService", error = %e, "sync local db failed");
            fail()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_clause_whitelist() {
        assert_eq!(order_clause(None, None), Ok(("createdAt", "DESC")));
        assert_eq!(order_clause(Some(""), Some("")), Ok(("createdAt", "DESC")));
        assert_eq!(
            order_clause(Some("takenDate"), Some("asc")),
            Ok(("takenDate", "ASC"))
        );
        assert!(order_clause(Some("user"), None).is_err());
        assert!(order_clause(Some("id; drop"), None).is_err());
        assert!(order_clause(None, Some("ASC")).is_err());
    }

    #[test]
    fn screenshot_path_matches_proxy() {
        assert_eq!(
            screenshot_image_path("9c9dd5b0-645e-499d-9c3e-d6cfc2f4ac55"),
            "/screenshots/9c9dd5b0-645e-499d-9c3e-d6cfc2f4ac55/image?variant=display"
        );
        assert_eq!(
            screenshot_image_path("a b"),
            "/screenshots/a%20b/image?variant=display"
        );
    }

    #[test]
    fn server_time_uses_year_day_month() {
        let v = server_date_time();
        let date = v
            .pointer("/data/date")
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert_eq!(date.len(), 10);
        assert_eq!(v.get("message"), Some(&json!("get server date time")));
    }
}
