//! Port `src/activity-logs/activity-logs.service.ts`.

use std::collections::HashMap;

use axum::http::StatusCode;
use mz_core::db::{advisory_xact_lock_timeout, is_lock_timeout};
use mz_core::enums::sees_whole_company;
use mz_core::time::{JsDate, parse_js_date};
use mz_core::{ApiError, ApiResult, AppState};
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::{FromRow, PgConnection};

use super::dto::{CreateActivityLogDto, FindActivityLogsByDateDto};
use super::window_title::{Session, continues, group_continuous_sessions, normalize_window_title};
use crate::division::{ViewerScope, can_view_user, resolve_viewer_scope, scope_forbidden};

const MAX_APP_NAME: usize = 100;
const MAX_TITLE: usize = 255;
const MAX_HOST: usize = 255;
const MAX_TIMEZONE: usize = 255;

/// Batas menunggu lock per timeline. Transaksi normal selesai dalam milidetik; menunggu
/// lebih lama hanya menahan koneksi pool dan ikut melambatkan endpoint lain.
const ACTIVITY_LOG_LOCK_TIMEOUT_MS: u32 = 1_000;

const NON_LEADER_USER_FILTER_MESSAGE: &str = "Only admin and manager can filter by user_uuid";

fn truncate(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}

/// Judul kosong diganti cadangan (nama proses / host).
fn title_or<'a>(title: &'a str, fallback: &'a str) -> &'a str {
    if title.trim().is_empty() {
        fallback
    } else {
        title
    }
}

/// `Math.round` JS (setengah ke atas).
fn js_round(x: f64) -> f64 {
    (x + 0.5).floor()
}

/// Angka JS: bilangan bulat tanpa `.0`.
#[allow(clippy::cast_possible_truncation)]
fn js_number(x: f64) -> Value {
    if x.fract() == 0.0 && x.abs() < 9e15 {
        Value::from(x as i64)
    } else {
        serde_json::Number::from_f64(x).map_or(Value::Null, Value::Number)
    }
}

/// Total detik gabungan rentang (`unionSeconds`).
fn union_seconds(spans: &[(i64, i64)]) -> i64 {
    let mut spans: Vec<(i64, i64)> = spans.iter().copied().filter(|(s, e)| e > s).collect();
    spans.sort_by_key(|s| s.0);
    let mut total_ms = 0i64;
    let mut cur: Option<(i64, i64)> = None;
    for (start, end) in spans {
        match cur {
            Some((cs, ce)) if start <= ce => cur = Some((cs, ce.max(end))),
            Some((cs, ce)) => {
                total_ms += ce - cs;
                cur = Some((start, end));
            }
            None => cur = Some((start, end)),
        }
    }
    if let Some((cs, ce)) = cur {
        total_ms += ce - cs;
    }
    #[allow(clippy::cast_precision_loss)]
    let secs = js_round(total_ms as f64 / 1000.0);
    #[allow(clippy::cast_possible_truncation)]
    {
        secs as i64
    }
}

/// Entri hasil `findByDate` (`ActivityLogWithDurationDto`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityEntry {
    pub id: i32,
    #[serde(rename = "type")]
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    pub start_at: JsDate,
    pub end_at: JsDate,
    pub timezone: String,
    pub duration_seconds: i64,
    #[serde(serialize_with = "ser_js_number")]
    pub activity_percent: f64,
}

fn ser_js_number<S: serde::Serializer>(v: &f64, s: S) -> Result<S::Ok, S::Error> {
    serde::Serialize::serialize(&js_number(*v), s)
}

#[derive(Debug, Clone)]
struct TopItem {
    name: String,
    duration_seconds: i64,
    count: i64,
}

fn top_items(
    entries: &[&ActivityEntry],
    key_of: impl Fn(&ActivityEntry) -> String,
) -> (usize, Vec<Value>) {
    let mut order: Vec<String> = Vec::new();
    let mut map: HashMap<String, TopItem> = HashMap::new();
    for e in entries {
        let key = key_of(e);
        let item = map.entry(key.clone()).or_insert_with(|| {
            order.push(key.clone());
            TopItem {
                name: key.clone(),
                duration_seconds: 0,
                count: 0,
            }
        });
        item.duration_seconds += e.duration_seconds;
        item.count += 1;
    }
    let size = map.len();
    let mut items: Vec<TopItem> = order.iter().filter_map(|k| map.remove(k)).collect();
    // `sort` JS stabil: seri mempertahankan urutan kemunculan.
    items.sort_by_key(|a| std::cmp::Reverse(a.duration_seconds));
    let top = items
        .into_iter()
        .take(5)
        .map(|i| json!({"name": i.name, "durationSeconds": i.duration_seconds, "count": i.count}))
        .collect();
    (size, top)
}

/// `buildSummary`.
pub fn build_summary(data: &[ActivityEntry]) -> Value {
    let apps: Vec<&ActivityEntry> = data.iter().filter(|d| d.kind == "app").collect();
    let urls: Vec<&ActivityEntry> = data.iter().filter(|d| d.kind == "url").collect();
    // Gabungan rentang, bukan jumlah: baris app browser dan baris url-nya menutupi waktu yang sama.
    let spans: Vec<(i64, i64)> = data
        .iter()
        .map(|d| (d.start_at.timestamp_millis(), d.end_at.timestamp_millis()))
        .collect();
    let (unique_apps, top_apps) =
        top_items(&apps, |e| e.name.clone().unwrap_or_else(|| e.title.clone()));
    let (unique_sites, top_hosts) = top_items(&urls, |e| {
        e.host
            .clone()
            .or_else(|| e.url.clone())
            .unwrap_or_else(|| e.title.clone())
    });
    json!({
        "totalTrackedSeconds": union_seconds(&spans),
        "totalUniqueApps": unique_apps,
        "totalUniqueSites": unique_sites,
        "topApps": top_apps,
        "topHosts": top_hosts,
    })
}

#[derive(FromRow)]
struct SessionRow {
    id: i32,
    #[sqlx(rename = "trackingTimelineId")]
    tracking_timeline_id: i32,
    uuid: String,
    name: Option<String>,
    title: String,
    url: Option<String>,
    host: Option<String>,
    #[sqlx(rename = "startAt")]
    start_at: JsDate,
    #[sqlx(rename = "endAt")]
    end_at: JsDate,
    timezone: String,
}

impl From<SessionRow> for Session {
    fn from(r: SessionRow) -> Self {
        Self {
            id: r.id,
            tracking_timeline_id: r.tracking_timeline_id,
            uuid: r.uuid,
            name: r.name,
            title: r.title,
            url: r.url,
            host: r.host,
            start_at: r.start_at,
            end_at: r.end_at,
            timezone: r.timezone,
        }
    }
}

/// Pola `ILIKE` untuk Prisma `contains` + `mode: 'insensitive'` (karakter khusus di-escape).
fn like_pattern(term: &str) -> String {
    let mut out = String::from("%");
    for c in term.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('%');
    out
}

/// Leader divisi boleh memfilter `user_uuid` anggota divisinya; selain itu 403.
async fn assert_may_filter_by_user(
    state: &AppState,
    user_uuid: &str,
    caller_id: i32,
    caller_uuid: &str,
) -> ApiResult<()> {
    let denied = || ApiError::forbidden(NON_LEADER_USER_FILTER_MESSAGE);
    if user_uuid == caller_uuid {
        return Err(denied());
    }
    let Some(company_id) = mz_core::company::find_company_id(&state.db, caller_id).await? else {
        return Err(denied());
    };
    let scope = resolve_viewer_scope(&state.db, caller_id, company_id).await?;
    if !matches!(scope, ViewerScope::Division { .. }) {
        return Err(denied());
    }
    let target: Option<i32> = sqlx::query_scalar("SELECT id FROM users WHERE uuid = $1::uuid")
        .bind(user_uuid)
        .fetch_optional(&state.db)
        .await?;
    match target {
        Some(t) if can_view_user(&scope, caller_id, t) => Ok(()),
        _ => Err(scope_forbidden()),
    }
}

/// Hasil `findByDate`.
pub struct FindByDate {
    pub taken_date: String,
    pub data: Vec<ActivityEntry>,
    pub body: Value,
}

/// Node: `findByDate`.
pub async fn find_by_date(
    state: &AppState,
    dto: &FindActivityLogsByDateDto,
    current_user_id: i32,
) -> ApiResult<FindByDate> {
    let current: Option<(String,)> = sqlx::query_as("SELECT uuid::text FROM users WHERE id = $1")
        .bind(current_user_id)
        .fetch_optional(&state.db)
        .await?;
    let Some((current_uuid,)) = current else {
        return Err(ApiError::not_found("Current user not found"));
    };
    let requester = mz_files::load_file_requester(&state.db, current_user_id).await?;
    let is_privileged = sees_whole_company(requester.role_code.as_deref());

    let user_uuid_filter = dto.user_uuid.as_deref();
    if let Some(u) = user_uuid_filter.filter(|u| !u.is_empty())
        && !is_privileged
    {
        assert_may_filter_by_user(state, u, current_user_id, &current_uuid).await?;
    }
    let resolved_user_uuid = user_uuid_filter.map_or(current_uuid.as_str(), |u| u);

    let timeline_id: Option<i32> = match dto
        .tracking_timeline_uuid
        .as_deref()
        .filter(|u| !u.is_empty())
    {
        Some(tl) => {
            let id: Option<i32> =
                sqlx::query_scalar(r#"SELECT id FROM "trackingTimeline" WHERE uuid = $1::uuid"#)
                    .bind(tl)
                    .fetch_optional(&state.db)
                    .await?;
            Some(id.ok_or_else(|| {
                ApiError::not_found(format!("TrackingTimeline with uuid {tl} not found"))
            })?)
        }
        None => None,
    };

    let day_start = parse_js_date(&format!("{}T00:00:00.000Z", dto.taken_date()))
        .ok_or_else(|| ApiError::internal(anyhow::anyhow!("Invalid time value")))?;
    let day_end = parse_js_date(&format!("{}T23:59:59.999Z", dto.taken_date()))
        .ok_or_else(|| ApiError::internal(anyhow::anyhow!("Invalid time value")))?;

    let mut qb = sqlx::QueryBuilder::<sqlx::Postgres>::new(
        r#"SELECT tt.id, tt."startAt", tt."endAt" FROM "trackingTimeline" tt
           JOIN tracking tr ON tr.id = tt."trackingId"
           JOIN users u ON u.id = tr."userId"
           JOIN project p ON p.id = tt."projectId"
           WHERE u.uuid = "#,
    );
    qb.push_bind(resolved_user_uuid).push("::uuid");
    if let Some(p) = dto.project_uuid.as_deref().filter(|p| !p.is_empty()) {
        qb.push(" AND p.uuid = ").push_bind(p).push("::uuid");
    }
    if let Some(id) = timeline_id {
        qb.push(" AND tt.id = ").push_bind(id);
    }
    qb.push(r#" AND tt."startAt" <= "#).push_bind(day_end);
    qb.push(r#" AND tt."endAt" >= "#).push_bind(day_start);
    let timelines: Vec<(i32, JsDate, JsDate)> = qb.build_query_as().fetch_all(&state.db).await?;

    if timelines.is_empty() {
        let body = json!({
            "success": true,
            "takenDate": dto.taken_date(),
            "total": 0,
            "summary": {
                "totalTrackedSeconds": 0,
                "totalUniqueApps": 0,
                "totalUniqueSites": 0,
                "topApps": [],
                "topHosts": [],
            },
            "data": [],
        });
        return Ok(FindByDate {
            taken_date: dto.taken_date().to_owned(),
            data: Vec::new(),
            body,
        });
    }

    let timeline_ids: Vec<i32> = timelines.iter().map(|t| t.0).collect();
    let duration_of: HashMap<i32, i64> = timelines
        .iter()
        .map(|(id, s, e)| {
            #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
            let secs =
                js_round((e.timestamp_millis() - s.timestamp_millis()) as f64 / 1000.0) as i64;
            (*id, secs.max(1))
        })
        .collect();

    let search = dto
        .search
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(like_pattern);

    let mut app_qb = sqlx::QueryBuilder::<sqlx::Postgres>::new(
        r#"SELECT id, "trackingTimelineId", uuid::text AS uuid, name, title, NULL::text AS url, NULL::text AS host,
                  "startAt", "endAt", timezone
           FROM app WHERE "trackingTimelineId" = ANY("#,
    );
    app_qb
        .push_bind(&timeline_ids)
        .push(r#") AND "startAt" >= "#)
        .push_bind(day_start);
    app_qb.push(r#" AND "startAt" <= "#).push_bind(day_end);
    if let Some(p) = &search {
        app_qb
            .push(" AND (name ILIKE ")
            .push_bind(p.clone())
            .push(" OR title ILIKE ")
            .push_bind(p.clone())
            .push(")");
    }
    app_qb.push(r#" ORDER BY "startAt" DESC"#);

    let mut url_qb = sqlx::QueryBuilder::<sqlx::Postgres>::new(
        r#"SELECT id, "trackingTimelineId", uuid::text AS uuid, NULL::text AS name, title, url, host,
                  "startAt", "endAt", timezone
           FROM url WHERE "trackingTimelineId" = ANY("#,
    );
    url_qb
        .push_bind(&timeline_ids)
        .push(r#") AND "startAt" >= "#)
        .push_bind(day_start);
    url_qb.push(r#" AND "startAt" <= "#).push_bind(day_end);
    if let Some(p) = &search {
        url_qb
            .push(" AND (title ILIKE ")
            .push_bind(p.clone())
            .push(" OR url ILIKE ")
            .push_bind(p.clone())
            .push(")");
    }
    url_qb.push(r#" ORDER BY "startAt" DESC"#);

    let (apps, urls) = tokio::try_join!(
        app_qb.build_query_as::<SessionRow>().fetch_all(&state.db),
        url_qb.build_query_as::<SessionRow>().fetch_all(&state.db),
    )?;
    let apps: Vec<Session> = apps.into_iter().map(Session::from).collect();
    let urls: Vec<Session> = urls.into_iter().map(Session::from).collect();

    let starts = |rows: &[Session]| -> Vec<(i32, i64)> {
        rows.iter()
            .map(|r| (r.tracking_timeline_id, r.start_at.timestamp_millis()))
            .collect()
    };
    // Baris tabel lain yang mulai di jeda antara dua sesi tetap memutus grup.
    let grouped_apps = group_continuous_sessions(
        &apps,
        |r| r.name.clone().unwrap_or_default(),
        &starts(&urls),
    );
    let grouped_urls = group_continuous_sessions(
        &urls,
        |r| json!([r.url, normalize_window_title(&r.title)]).to_string(),
        &starts(&apps),
    );

    let to_entry = |row: &Session, kind: &'static str| -> ActivityEntry {
        #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
        let duration = js_round(
            (row.end_at.timestamp_millis() - row.start_at.timestamp_millis()) as f64 / 1000.0,
        ) as i64;
        let timeline_duration = duration_of
            .get(&row.tracking_timeline_id)
            .copied()
            .unwrap_or(1);
        #[allow(clippy::cast_precision_loss)]
        let percent = js_round(duration as f64 / timeline_duration as f64 * 10000.0) / 100.0;
        ActivityEntry {
            id: row.id,
            kind,
            name: row.name.clone(),
            title: row.title.clone(),
            url: row.url.clone(),
            host: row.host.clone(),
            start_at: row.start_at,
            end_at: row.end_at,
            timezone: row.timezone.clone(),
            duration_seconds: duration,
            activity_percent: percent,
        }
    };

    let mut data: Vec<ActivityEntry> = grouped_apps
        .iter()
        .map(|r| to_entry(r, "app"))
        .chain(grouped_urls.iter().map(|r| to_entry(r, "url")))
        .collect();
    data.sort_by(|a, b| {
        b.start_at
            .timestamp_millis()
            .cmp(&a.start_at.timestamp_millis())
            .then(
                b.end_at
                    .timestamp_millis()
                    .cmp(&a.end_at.timestamp_millis()),
            )
    });

    let summary = build_summary(&data);
    let body = json!({
        "success": true,
        "takenDate": dto.taken_date(),
        "total": data.len(),
        "summary": summary,
        "data": data,
    });
    Ok(FindByDate {
        taken_date: dto.taken_date().to_owned(),
        data,
        body,
    })
}

/// `new URL(full_url).hostname`, atau `full_url` bila tidak valid.
fn host_of(full_url: &str) -> String {
    url::Url::parse(full_url).map_or_else(
        |_| full_url.to_owned(),
        |u| u.host_str().unwrap_or("").to_owned(),
    )
}

fn later_of(a: JsDate, b: JsDate) -> JsDate {
    if b.timestamp_millis() > a.timestamp_millis() {
        b
    } else {
        a
    }
}

const APP_COLS: &str = r#"id, "trackingTimelineId", uuid::text AS uuid, name, title, NULL::text AS url, NULL::text AS host, "startAt", "endAt", timezone"#;
const URL_COLS: &str = r#"id, "trackingTimelineId", uuid::text AS uuid, NULL::text AS name, title, url, host, "startAt", "endAt", timezone"#;

fn app_json(r: &SessionRow) -> Value {
    json!({
        "id": r.id, "trackingTimelineId": r.tracking_timeline_id, "uuid": r.uuid, "name": r.name,
        "title": r.title, "startAt": r.start_at, "endAt": r.end_at, "timezone": r.timezone,
    })
}

fn url_json(r: &SessionRow) -> Value {
    json!({
        "id": r.id, "trackingTimelineId": r.tracking_timeline_id, "uuid": r.uuid, "host": r.host, "url": r.url,
        "title": r.title, "startAt": r.start_at, "endAt": r.end_at, "timezone": r.timezone,
    })
}

#[allow(clippy::too_many_arguments)]
async fn upsert_app(
    tx: &mut PgConnection,
    dto: &CreateActivityLogDto,
    timeline_id: i32,
    start_at: JsDate,
    end_at: JsDate,
) -> sqlx::Result<Value> {
    let name = truncate(dto.process_name(), MAX_APP_NAME);
    let title = truncate(
        &normalize_window_title(title_or(dto.window_title(), dto.process_name())),
        MAX_TITLE,
    );
    let timezone = truncate(dto.timezone(), MAX_TIMEZONE);

    // Cari-lalu-tulis harus berurutan per timeline (TOCTOU retry klien).
    advisory_xact_lock_timeout(
        tx,
        &format!("activity-log:app:{timeline_id}"),
        ACTIVITY_LOG_LOCK_TIMEOUT_MS,
    )
    .await?;

    let same_session: Option<SessionRow> = sqlx::query_as(&format!(
        r#"SELECT {APP_COLS} FROM app
           WHERE "trackingTimelineId" = $1 AND name = $2 AND title = $3 AND "startAt" = $4
           ORDER BY id DESC LIMIT 1"#
    ))
    .bind(timeline_id)
    .bind(&name)
    .bind(&title)
    .bind(start_at)
    .fetch_optional(&mut *tx)
    .await?;

    let target = match same_session {
        Some(t) => Some(t),
        None => {
            let last: Option<SessionRow> = sqlx::query_as(&format!(
                r#"SELECT {APP_COLS} FROM app WHERE "trackingTimelineId" = $1 AND "startAt" <= $2
                   ORDER BY "startAt" DESC, id DESC LIMIT 1"#
            ))
            .bind(timeline_id)
            .bind(start_at)
            .fetch_optional(&mut *tx)
            .await?;
            last.filter(|l| {
                l.name.as_deref() == Some(name.as_str())
                    && l.title == title
                    && continues(&l.end_at, &start_at)
            })
        }
    };

    if let Some(t) = target {
        let updated: SessionRow = sqlx::query_as(&format!(
            r#"UPDATE app SET "endAt" = $2, timezone = $3 WHERE id = $1 RETURNING {APP_COLS}"#
        ))
        .bind(t.id)
        .bind(later_of(t.end_at, end_at))
        .bind(&timezone)
        .fetch_one(&mut *tx)
        .await?;
        return Ok(
            json!({"success": true, "message": "activity log updated successfully", "data": app_json(&updated)}),
        );
    }

    let created: SessionRow = sqlx::query_as(&format!(
        r#"INSERT INTO app (name, title, "startAt", "endAt", timezone, "trackingTimelineId")
           VALUES ($1, $2, $3, $4, $5, $6) RETURNING {APP_COLS}"#
    ))
    .bind(&name)
    .bind(&title)
    .bind(start_at)
    .bind(end_at)
    .bind(&timezone)
    .bind(timeline_id)
    .fetch_one(&mut *tx)
    .await?;
    Ok(
        json!({"success": true, "message": "activity log created successfully", "data": app_json(&created)}),
    )
}

async fn upsert_url(
    tx: &mut PgConnection,
    dto: &CreateActivityLogDto,
    timeline_id: i32,
    start_at: JsDate,
    end_at: JsDate,
) -> sqlx::Result<Value> {
    let full_url = dto.full_url();
    let host = truncate(&host_of(full_url), MAX_HOST);
    let title = truncate(
        &normalize_window_title(title_or(dto.window_title(), &host)),
        MAX_TITLE,
    );
    let timezone = truncate(dto.timezone(), MAX_TIMEZONE);

    advisory_xact_lock_timeout(
        tx,
        &format!("activity-log:url:{timeline_id}"),
        ACTIVITY_LOG_LOCK_TIMEOUT_MS,
    )
    .await?;

    let same_session: Option<SessionRow> = sqlx::query_as(&format!(
        r#"SELECT {URL_COLS} FROM url
           WHERE "trackingTimelineId" = $1 AND title = $2 AND url = $3 AND "startAt" = $4
           ORDER BY id DESC LIMIT 1"#
    ))
    .bind(timeline_id)
    .bind(&title)
    .bind(full_url)
    .bind(start_at)
    .fetch_optional(&mut *tx)
    .await?;

    let target = match same_session {
        Some(t) => Some(t),
        None => {
            let last: Option<SessionRow> = sqlx::query_as(&format!(
                r#"SELECT {URL_COLS} FROM url WHERE "trackingTimelineId" = $1 AND "startAt" <= $2
                   ORDER BY "startAt" DESC, id DESC LIMIT 1"#
            ))
            .bind(timeline_id)
            .bind(start_at)
            .fetch_optional(&mut *tx)
            .await?;
            last.filter(|l| {
                l.url.as_deref() == Some(full_url)
                    && l.title == title
                    && continues(&l.end_at, &start_at)
            })
        }
    };

    if let Some(t) = target {
        let updated: SessionRow = sqlx::query_as(&format!(
            r#"UPDATE url SET "endAt" = $2, timezone = $3, host = $4 WHERE id = $1 RETURNING {URL_COLS}"#
        ))
        .bind(t.id)
        .bind(later_of(t.end_at, end_at))
        .bind(&timezone)
        .bind(&host)
        .fetch_one(&mut *tx)
        .await?;
        return Ok(
            json!({"success": true, "message": "activity log updated successfully", "data": url_json(&updated)}),
        );
    }

    let created: SessionRow = sqlx::query_as(&format!(
        r#"INSERT INTO url (host, url, title, "startAt", "endAt", timezone, "trackingTimelineId")
           VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING {URL_COLS}"#
    ))
    .bind(&host)
    .bind(full_url)
    .bind(&title)
    .bind(start_at)
    .bind(end_at)
    .bind(&timezone)
    .bind(timeline_id)
    .fetch_one(&mut *tx)
    .await?;
    Ok(
        json!({"success": true, "message": "activity log created successfully", "data": url_json(&created)}),
    )
}

/// Node: `logActivity`.
pub async fn log_activity(state: &AppState, dto: &CreateActivityLogDto) -> ApiResult<Value> {
    let start_at = JsDate::from_millis(dto.start_at_ms())
        .ok_or_else(|| ApiError::internal(anyhow::anyhow!("Invalid time value")))?;
    let end_at = JsDate::from_millis(dto.end_at_ms())
        .ok_or_else(|| ApiError::internal(anyhow::anyhow!("Invalid time value")))?;

    let timeline_uuid = dto.tracking_timeline_uuid().trim();
    if timeline_uuid.is_empty() {
        return Err(ApiError::bad_request("tracking_timeline_uuid is required"));
    }

    // `trackingTimeline` + apakah ada company project-nya yang menyalakan activity track.
    let timeline: Option<(i32, bool)> = sqlx::query_as(
        r#"SELECT tt.id,
                  EXISTS (SELECT 1 FROM "projectCompany" pc
                          JOIN company c ON c.id = pc."companyId"
                          WHERE pc."projectId" = tt."projectId" AND c."useActivityTrack" = TRUE)
           FROM "trackingTimeline" tt WHERE tt.uuid = $1::uuid"#,
    )
    .bind(dto.tracking_timeline_uuid())
    .fetch_optional(&state.db)
    .await?;
    let Some((timeline_id, activity_track_on)) = timeline else {
        return Err(ApiError::not_found(format!(
            "TrackingTimeline with uuid {} not found",
            dto.tracking_timeline_uuid()
        )));
    };

    // Company tanpa activity track: tetap 200 supaya klien desktop tidak me-retry.
    if !activity_track_on {
        return Ok(json!({
            "success": true,
            "message": "activity tracking is disabled for this company",
            "data": Value::Null,
        }));
    }

    let has_url = !dto.full_url().trim().is_empty();
    // Satu transaksi; lock app selalu diambil sebelum lock url (tanpa urutan terbalik).
    // Lock yang tak kebagian dalam ACTIVITY_LOG_LOCK_TIMEOUT_MS dijawab 503; kiriman
    // berikutnya dari sesi yang sama (startAt sama, endAt lebih baru) menutup rentangnya.
    let mut tx = state.db.begin().await?;
    let written = async {
        let mut result = upsert_app(&mut tx, dto, timeline_id, start_at, end_at).await?;
        if has_url {
            result = upsert_url(&mut tx, dto, timeline_id, start_at, end_at).await?;
        }
        Ok::<_, sqlx::Error>(result)
    }
    .await;
    let result = match written {
        Ok(r) => r,
        Err(e) if is_lock_timeout(&e) => {
            return Err(ApiError::message(
                StatusCode::SERVICE_UNAVAILABLE,
                "Activity log for this tracking timeline is busy, please retry",
            ));
        }
        Err(e) => return Err(e.into()),
    };
    tx.commit().await?;
    Ok(result)
}

/// CSV `export/csv` (`escape` + join `\n`).
pub fn to_csv(data: &[ActivityEntry]) -> String {
    let escape = |val: &str| {
        if val.contains(',') || val.contains('"') || val.contains('\n') {
            format!("\"{}\"", val.replace('"', "\"\""))
        } else {
            val.to_owned()
        }
    };
    let header = [
        "id",
        "type",
        "name",
        "title",
        "url",
        "host",
        "startAt",
        "endAt",
        "timezone",
        "durationSeconds",
    ];
    let mut lines = vec![header.join(",")];
    for r in data {
        let cells = [
            r.id.to_string(),
            r.kind.to_owned(),
            r.name.clone().unwrap_or_default(),
            r.title.clone(),
            r.url.clone().unwrap_or_default(),
            r.host.clone().unwrap_or_default(),
            r.start_at.to_js_string(),
            r.end_at.to_js_string(),
            r.timezone.clone(),
            r.duration_seconds.to_string(),
        ];
        lines.push(
            cells
                .iter()
                .map(|c| escape(c))
                .collect::<Vec<_>>()
                .join(","),
        );
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(
        kind: &'static str,
        name: Option<&str>,
        host: Option<&str>,
        start: i64,
        end: i64,
    ) -> ActivityEntry {
        ActivityEntry {
            id: 1,
            kind,
            name: name.map(str::to_owned),
            title: "t".into(),
            url: host.map(|h| format!("https://{h}/")),
            host: host.map(str::to_owned),
            start_at: JsDate::from_millis(start).unwrap_or_else(JsDate::now),
            end_at: JsDate::from_millis(end).unwrap_or_else(JsDate::now),
            timezone: "UTC".into(),
            duration_seconds: (end - start) / 1000,
            activity_percent: 12.5,
        }
    }

    #[test]
    fn union_merges_overlaps() {
        assert_eq!(
            union_seconds(&[(0, 10_000), (5_000, 20_000), (30_000, 40_000)]),
            30
        );
        assert_eq!(union_seconds(&[(0, 0), (5, 5)]), 0);
        assert_eq!(union_seconds(&[]), 0);
        assert_eq!(union_seconds(&[(0, 1_500)]), 2);
    }

    #[test]
    fn summary_counts_and_top() {
        let data = vec![
            entry("app", Some("Chrome"), None, 0, 60_000),
            entry("url", None, Some("a.com"), 0, 30_000),
            entry("url", None, Some("a.com"), 40_000, 50_000),
            entry("app", Some("Code"), None, 100_000, 110_000),
        ];
        let s = build_summary(&data);
        assert_eq!(s["totalTrackedSeconds"], 70);
        assert_eq!(s["totalUniqueApps"], 2);
        assert_eq!(s["totalUniqueSites"], 1);
        assert_eq!(s["topApps"][0]["name"], "Chrome");
        assert_eq!(
            s["topHosts"][0],
            json!({"name": "a.com", "durationSeconds": 40, "count": 2})
        );
    }

    #[test]
    fn entry_json_omits_undefined_and_trims_integers() {
        let mut e = entry("app", Some("Chrome"), None, 0, 1000);
        e.activity_percent = 100.0;
        let v = serde_json::to_value(&e).unwrap_or_default();
        assert!(v.get("url").is_none() && v.get("host").is_none());
        assert_eq!(v["activityPercent"].to_string(), "100");
        assert_eq!(v["type"], "app");
        assert_eq!(v["startAt"], "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn csv_escaping() {
        let mut e = entry("app", Some("a,b"), None, 0, 1000);
        e.title = "say \"hi\"".into();
        let csv = to_csv(&[e]);
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(
            lines[0],
            "id,type,name,title,url,host,startAt,endAt,timezone,durationSeconds"
        );
        assert_eq!(
            lines[1],
            "1,app,\"a,b\",\"say \"\"hi\"\"\",,,1970-01-01T00:00:00.000Z,1970-01-01T00:00:01.000Z,UTC,1"
        );
    }

    #[test]
    fn helpers() {
        assert_eq!(host_of("https://Example.com:8080/x"), "example.com");
        assert_eq!(host_of("not a url"), "not a url");
        assert_eq!(like_pattern("50%_a"), "%50\\%\\_a%");
        assert_eq!(js_round(-0.5), 0.0);
        assert_eq!(js_round(2.5), 3.0);
        assert_eq!(truncate("héllo", 2), "hé");
        assert_eq!(title_or("  ", "fb"), "fb");
    }
}
