//! `GoogleCalendarService` (`google-calendar.service.ts`): koneksi Google Calendar level
//! company, sinkronisasi, kalender bulanan, event company, webhook, dan cron.

use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use chrono::{Datelike, NaiveDate, NaiveDateTime};
use mz_audit::{AuditHandle, Model};
use mz_core::AppState;
use mz_core::time::{JsDate, now_naive, to_js_iso};
use mz_int_common::envelope::Envelope;
use serde::Serialize;
use serde_json::{Map, Value, json};
use sqlx::types::Json;
use sqlx::{Postgres, QueryBuilder};

use crate::db::{
    self, Counts, EventUpsert, NewEvent, Set, event_from_item, expiry_from, opt_str, parse_or_fail,
    update_audited,
};
use crate::dto::{CalendarMonth, CalendarQuery, CreateEvent};
use crate::google::{self, COMPANY_TIMEOUT, Fail, GResult};
use crate::mail::{Invite, send_invite};
use crate::util::{JsNum, encode_uri_component, js_template, search_params, truthy};

pub const DEFAULT_CALENDAR_ID: &str = "primary";
const GOOGLE_AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const DEAD_CREDENTIAL_BACKOFF: Duration = Duration::from_secs(24 * 60 * 60);
const UPSERT_CHUNK_SIZE: usize = 100;
const COMPANY_CALENDAR_SCOPES: &str = "https://www.googleapis.com/auth/calendar.readonly https://www.googleapis.com/auth/calendar.events https://www.googleapis.com/auth/userinfo.email";

const T: Option<Duration> = Some(COMPANY_TIMEOUT);

// ── state in-process (instance tunggal Node) ────────────────────────────────

static SYNCING: LazyLock<Mutex<HashSet<i32>>> = LazyLock::new(Mutex::default);
static PENDING: LazyLock<Mutex<HashSet<i32>>> = LazyLock::new(Mutex::default);
static DEAD_UNTIL: LazyLock<Mutex<HashMap<i32, Instant>>> = LazyLock::new(Mutex::default);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn is_credential_dead(company_id: i32) -> bool {
    let mut dead = lock(&DEAD_UNTIL);
    match dead.get(&company_id) {
        None => false,
        Some(until) if *until <= Instant::now() => {
            dead.remove(&company_id);
            false
        }
        Some(_) => true,
    }
}

fn clear_dead(company_id: i32) {
    lock(&DEAD_UNTIL).remove(&company_id);
}

/// `client_id`/`client_secret` env (`undefined` → param dilewati axios).
fn client_params(state: &AppState) -> [(&'static str, Option<String>); 2] {
    [
        ("client_id", state.config.var("GOOGLE_CALENDAR_CLIENT_ID")),
        (
            "client_secret",
            state.config.var("GOOGLE_CALENDAR_CLIENT_SECRET"),
        ),
    ]
}

pub fn frontend_url(state: &AppState) -> String {
    state
        .config
        .var("FRONTEND_URL")
        .unwrap_or_else(|| "http://localhost:3001".into())
}

// ── token ───────────────────────────────────────────────────────────────────

#[derive(sqlx::FromRow)]
struct TokenRow {
    #[sqlx(rename = "googleCalendarAccessToken")]
    access: Option<String>,
    #[sqlx(rename = "googleCalendarRefreshToken")]
    refresh: Option<String>,
    #[sqlx(rename = "googleCalendarTokenExpiresAt")]
    expires_at: Option<NaiveDateTime>,
    #[sqlx(rename = "googleCalendarSyncToken")]
    sync_token: Option<String>,
}

async fn token_row(state: &AppState, company_id: i32) -> sqlx::Result<Option<TokenRow>> {
    sqlx::query_as(
        r#"SELECT "googleCalendarAccessToken", "googleCalendarRefreshToken",
                  "googleCalendarTokenExpiresAt", "googleCalendarSyncToken"
           FROM company WHERE id = $1"#,
    )
    .bind(company_id)
    .fetch_optional(&state.db)
    .await
}

fn needs_refresh(expires_at: Option<NaiveDateTime>) -> bool {
    expires_at.is_none_or(|e| e <= now_naive() + chrono::Duration::minutes(5))
}

/// `refreshAccessToken` — `None` = `access_token` tidak ada di respons (`undefined`).
async fn refresh_access_token(
    state: &AppState,
    audit: &AuditHandle,
    company_id: i32,
    refresh: &str,
) -> GResult<Option<String>> {
    let mut params = vec![("grant_type", Some("refresh_token".to_owned()))];
    params.extend(client_params(state));
    params.push(("refresh_token", Some(refresh.to_owned())));
    let data = match google::token(state, &params, T).await {
        Ok(d) => d,
        Err(err) => {
            if err.data_error() == Some("invalid_grant") {
                lock(&DEAD_UNTIL).insert(company_id, Instant::now() + DEAD_CREDENTIAL_BACKOFF);
                tracing::warn!(
                    company_id,
                    "Google Calendar refresh token rejected (invalid_grant); scheduled sync paused for 24h or until reconnect"
                );
            }
            return Err(err);
        }
    };
    clear_dead(company_id);
    let access = opt_str(data.get("access_token"));
    let expires = expiry_from(data.get("expires_in"))?;
    let mut sets = Vec::new();
    if let Some(a) = &access {
        sets.push(Set::Text("googleCalendarAccessToken", Some(a.clone())));
    }
    sets.push(Set::Ts("googleCalendarTokenExpiresAt", Some(expires)));
    sets.push(Set::Ts("updatedAt", Some(now_naive())));
    update_audited(state, audit, Model::Company, company_id, sets).await?;
    Ok(access)
}

/// `getValidToken(companyId)`.
pub async fn get_valid_token(
    state: &AppState,
    audit: &AuditHandle,
    company_id: i32,
) -> GResult<Option<String>> {
    let Some(row) = token_row(state, company_id).await? else {
        return Ok(None);
    };
    let Some(refresh) = row.refresh.filter(|r| !r.is_empty()) else {
        return Ok(None);
    };
    if needs_refresh(row.expires_at) {
        return refresh_access_token(state, audit, company_id, &refresh).await;
    }
    Ok(row.access)
}

// ── OAuth ───────────────────────────────────────────────────────────────────

/// `generateAuthUrl(userId)`.
pub fn generate_auth_url(state: &AppState, user_id: i32) -> String {
    auth_url(
        state,
        user_id,
        "company",
        "GOOGLE_CALENDAR_REDIRECT_URI",
        COMPANY_CALENDAR_SCOPES,
    )
}

/// `URLSearchParams` consent; env kosong → `"undefined"` seperti Node.
pub fn auth_url(
    state: &AppState,
    user_id: i32,
    kind: &str,
    redirect_env: &str,
    scopes: &str,
) -> String {
    let mut payload = Map::new();
    payload.insert("userId".into(), Value::from(user_id));
    payload.insert("type".into(), Value::from(kind));
    let token = match mz_int_common::nest_jwt::sign(state, payload, Some(600)) {
        Ok(t) => t,
        Err(e) => {
            tracing::error!(error = %e, "google-calendar: sign state gagal");
            String::new()
        }
    };
    let env = |k: &str| state.config.var(k).unwrap_or_else(|| "undefined".into());
    let qs = search_params(&[
        ("response_type", "code".into()),
        ("client_id", env("GOOGLE_CALENDAR_CLIENT_ID")),
        ("redirect_uri", env(redirect_env)),
        ("scope", scopes.into()),
        ("state", token),
        ("access_type", "offline".into()),
        ("prompt", "consent".into()),
    ]);
    format!("{GOOGLE_AUTH_URL}?{qs}")
}

/// Payload `state` OAuth → `userId` (`None` = `Invalid state`, `Some(Err)` = tipe salah).
pub fn verify_state(
    state: &AppState,
    token: Option<&str>,
    kind: &str,
) -> Result<Value, &'static str> {
    let Some(payload) = token.and_then(|t| mz_int_common::nest_jwt::verify(state, t)) else {
        return Err("Invalid state");
    };
    if payload.get("type").and_then(Value::as_str) != Some(kind) {
        return Err("Invalid state type");
    }
    Ok(payload.get("userId").cloned().unwrap_or(Value::Null))
}

/// `payload.userId` dipakai sebagai id Prisma (Int).
pub fn user_id_of(v: &Value) -> GResult<i32> {
    v.as_i64()
        .and_then(|n| i32::try_from(n).ok())
        .ok_or_else(|| Fail::Msg("Argument `userId`: Invalid value provided. Expected Int.".into()))
}

/// `handleCallback(code, state)` → `redirectUrl`.
pub async fn handle_callback(
    state: &AppState,
    audit: &AuditHandle,
    code: Option<String>,
    st: Option<String>,
) -> String {
    let fe = frontend_url(state);
    let error_url = |msg: &str| {
        format!(
            "{fe}/settings/integrations?error=google_calendar&message={}",
            encode_uri_component(msg)
        )
    };
    let user = match verify_state(state, st.as_deref(), "company") {
        Ok(u) => u,
        Err(msg) => return error_url(msg),
    };
    match callback_inner(state, audit, &user, code).await {
        Ok(None) => error_url("Company not found"),
        Ok(Some(())) => format!("{fe}/settings/integrations?connected=google_calendar"),
        Err(err) => {
            tracing::error!(error = %err.message(), "google-calendar-callback");
            error_url(&err.message())
        }
    }
}

async fn callback_inner(
    state: &AppState,
    audit: &AuditHandle,
    user: &Value,
    code: Option<String>,
) -> GResult<Option<()>> {
    let user_id = user_id_of(user)?;
    let Some(company_id) = mz_core::company::find_company_id(&state.db, user_id).await? else {
        return Ok(None);
    };
    let mut params = vec![("grant_type", Some("authorization_code".to_owned()))];
    params.extend(client_params(state));
    params.push(("code", code));
    params.push((
        "redirect_uri",
        state.config.var("GOOGLE_CALENDAR_REDIRECT_URI"),
    ));
    let data = google::token(state, &params, T).await?;
    let access = opt_str(data.get("access_token"));
    let refresh = opt_str(data.get("refresh_token"));
    let expires = expiry_from(data.get("expires_in"))?;
    let email = match google::userinfo(state, access.as_deref().unwrap_or("undefined"), T).await {
        Ok(d) => opt_str(d.get("email")),
        Err(_) => None,
    };
    let mut sets = Vec::new();
    if let Some(a) = &access {
        sets.push(Set::Text("googleCalendarAccessToken", Some(a.clone())));
    }
    if let Some(r) = &refresh {
        sets.push(Set::Text("googleCalendarRefreshToken", Some(r.clone())));
    }
    sets.push(Set::Ts("googleCalendarTokenExpiresAt", Some(expires)));
    sets.push(Set::Text("googleCalendarEmail", email));
    sets.push(Set::Text("googleCalendarSyncToken", None));
    sets.push(Set::Ts("updatedAt", Some(now_naive())));
    update_audited(state, audit, Model::Company, company_id, sets).await?;
    clear_dead(company_id);

    {
        let (state, audit) = (state.clone(), audit.clone());
        mz_core::bg::spawn(async move {
            if let Err(e) =
                sync_company_calendar(&state, &audit, company_id, DEFAULT_CALENDAR_ID).await
            {
                tracing::warn!(company_id, error = %e.message(), "Initial sync failed");
            }
        });
    }
    {
        let (state, audit) = (state.clone(), audit.clone());
        let token = access.unwrap_or_else(|| "undefined".into());
        mz_core::bg::spawn(async move {
            if let Err(e) = register_webhook_channel(&state, &audit, company_id, &token).await {
                tracing::warn!(company_id, error = %e.message(), "Webhook registration failed");
            }
        });
    }
    Ok(Some(()))
}

/// `{ success: true, message, data: null }`.
pub fn ok_null(message: &str) -> Envelope<()> {
    Envelope {
        success: true,
        message: message.into(),
        data: None,
    }
}

fn fail_env<T>(err: &Fail, module: &str) -> Envelope<T> {
    tracing::error!(module, error = %err.message(), "google-calendar");
    Envelope::fail(err.message())
}

/// `disconnect(userId)`.
pub async fn disconnect(state: &AppState, audit: &AuditHandle, user_id: i32) -> Envelope<()> {
    match disconnect_inner(state, audit, user_id).await {
        Ok(e) => e,
        Err(err) => fail_env(&err, "google-calendar-disconnect"),
    }
}

async fn disconnect_inner(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
) -> GResult<Envelope<()>> {
    let Some(company_id) = mz_core::company::find_company_id(&state.db, user_id).await? else {
        return Ok(Envelope::fail("Company not found"));
    };
    let row: Option<(Option<String>, Option<String>, Option<String>)> = sqlx::query_as(
        r#"SELECT "googleCalendarAccessToken", "googleCalendarWebhookChannelId",
                  "googleCalendarWebhookResourceId" FROM company WHERE id = $1"#,
    )
    .bind(company_id)
    .fetch_optional(&state.db)
    .await?;
    let (access, channel, resource) = row.unwrap_or_default();
    if let (Some(ch), Some(res)) = (
        channel.filter(|s| !s.is_empty()),
        resource.filter(|s| !s.is_empty()),
    ) {
        let stop = async {
            if let Some(token) = get_valid_token(state, audit, company_id).await?
                && !token.is_empty()
            {
                stop_webhook_channel(state, &ch, &res, &token).await?;
            }
            GResult::Ok(())
        };
        if let Err(e) = stop.await {
            tracing::warn!(company_id, error = %e.message(), "Webhook stop failed");
        }
    }
    if let Some(a) = access.filter(|a| !a.is_empty()) {
        let _ = google::revoke(state, &a, T).await;
    }
    let now = now_naive();
    update_audited(
        state,
        audit,
        Model::Company,
        company_id,
        vec![
            Set::Text("googleCalendarAccessToken", None),
            Set::Text("googleCalendarRefreshToken", None),
            Set::Ts("googleCalendarTokenExpiresAt", None),
            Set::Text("googleCalendarEmail", None),
            Set::Text("googleCalendarSyncToken", None),
            Set::Text("googleCalendarWebhookChannelId", None),
            Set::Text("googleCalendarWebhookResourceId", None),
            Set::Ts("googleCalendarWebhookExpiry", None),
            Set::Ts("updatedAt", Some(now)),
        ],
    )
    .await?;
    clear_dead(company_id);
    sqlx::query(
        r#"DELETE FROM "googleCalendarEvent"
           WHERE "companyId" = $1 AND "userId" IS NULL AND source = 'google'"#,
    )
    .bind(company_id)
    .execute(&state.db)
    .await?;
    Ok(ok_null("Google Calendar disconnected"))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    connected: bool,
    email: Option<String>,
    webhook_expiry: Option<JsDate>,
}

/// `getStatus(userId)`.
pub async fn get_status(state: &AppState, user_id: i32) -> Envelope<Status> {
    let run = async {
        let Some(company_id) = mz_core::company::find_company_id(&state.db, user_id).await? else {
            return GResult::Ok(Envelope::fail("Company not found"));
        };
        let row: Option<(Option<String>, Option<String>, Option<JsDate>)> = sqlx::query_as(
            r#"SELECT "googleCalendarEmail", "googleCalendarRefreshToken", "googleCalendarWebhookExpiry"
               FROM company WHERE id = $1"#,
        )
        .bind(company_id)
        .fetch_optional(&state.db)
        .await?;
        let (email, refresh, expiry) = row.unwrap_or_default();
        Ok(Envelope::ok(
            "Success",
            Status {
                connected: refresh.is_some_and(|r| !r.is_empty()),
                email,
                webhook_expiry: expiry,
            },
        ))
    };
    run.await
        .unwrap_or_else(|e| fail_env(&e, "google-calendar-status"))
}

/// `triggerSync(userId)`.
pub async fn trigger_sync(state: &AppState, audit: &AuditHandle, user_id: i32) -> Envelope<Counts> {
    let run = async {
        let Some(company_id) = mz_core::company::find_company_id(&state.db, user_id).await? else {
            return GResult::Ok(Envelope::fail("Company not found"));
        };
        match get_valid_token(state, audit, company_id).await? {
            Some(t) if !t.is_empty() => {}
            _ => return Ok(Envelope::fail("Google Calendar not connected")),
        }
        let r = sync_company_calendar(state, audit, company_id, DEFAULT_CALENDAR_ID).await?;
        Ok(Envelope::ok(
            format!(
                "Sync complete: {} events synced, {} cancelled",
                r.upserted, r.cancelled
            ),
            r,
        ))
    };
    run.await
        .unwrap_or_else(|e| fail_env(&e, "google-calendar-manual-sync"))
}

// ── sinkronisasi ────────────────────────────────────────────────────────────

/// Melepas kunci sync per company (blok `finally`).
struct SyncGuard(i32);

impl Drop for SyncGuard {
    fn drop(&mut self) {
        lock(&PENDING).remove(&self.0);
        lock(&SYNCING).remove(&self.0);
    }
}

/// `syncCompanyCalendar` — serial per company (in-process), permintaan saat sync berjalan
/// digabung jadi maksimal 3 putaran susulan.
pub async fn sync_company_calendar(
    state: &AppState,
    audit: &AuditHandle,
    company_id: i32,
    calendar_id: &str,
) -> GResult<Counts> {
    {
        let mut syncing = lock(&SYNCING);
        if syncing.contains(&company_id) {
            lock(&PENDING).insert(company_id);
            tracing::debug!(
                company_id,
                "Google Calendar sync already running; queued one follow-up pass"
            );
            return Ok(Counts::default());
        }
        syncing.insert(company_id);
    }
    let _guard = SyncGuard(company_id);
    let mut result = run_company_sync(state, audit, company_id, calendar_id).await?;
    for _ in 0..3 {
        if !lock(&PENDING).remove(&company_id) {
            break;
        }
        result += run_company_sync(state, audit, company_id, calendar_id).await?;
    }
    Ok(result)
}

async fn run_company_sync(
    state: &AppState,
    audit: &AuditHandle,
    company_id: i32,
    calendar_id: &str,
) -> GResult<Counts> {
    let Some(row) = token_row(state, company_id).await? else {
        return Ok(Counts::default());
    };
    let Some(refresh) = row.refresh.clone().filter(|r| !r.is_empty()) else {
        return Ok(Counts::default());
    };
    let access = if needs_refresh(row.expires_at) {
        js_opt(refresh_access_token(state, audit, company_id, &refresh).await?)
    } else {
        row.access.clone().unwrap_or_else(|| "null".into())
    };
    if let Some(sync_token) = row.sync_token.filter(|s| !s.is_empty()) {
        match page_sync(
            state,
            audit,
            company_id,
            calendar_id,
            &access,
            Some(&sync_token),
        )
        .await
        {
            Err(e) if e.http_status() == Some(410) => {
                update_audited(
                    state,
                    audit,
                    Model::Company,
                    company_id,
                    vec![
                        Set::Text("googleCalendarSyncToken", None),
                        Set::Ts("updatedAt", Some(now_naive())),
                    ],
                )
                .await?;
                return page_sync(state, audit, company_id, calendar_id, &access, None).await;
            }
            other => return other,
        }
    }
    page_sync(state, audit, company_id, calendar_id, &access, None).await
}

/// `undefined` di template literal.
pub fn js_opt(v: Option<String>) -> String {
    v.unwrap_or_else(|| "undefined".into())
}

/// `fullSync` (tanpa `sync_token`) / `incrementalSync`.
async fn page_sync(
    state: &AppState,
    audit: &AuditHandle,
    company_id: i32,
    calendar_id: &str,
    access: &str,
    sync_token: Option<&str>,
) -> GResult<Counts> {
    let now = chrono::Utc::now();
    let time_min = to_js_iso(&(now - chrono::Duration::days(30)));
    let time_max = to_js_iso(&(now + chrono::Duration::days(90)));
    let mut page_token: Option<String> = None;
    let mut next_sync: Option<String>;
    let mut total = Counts::default();
    loop {
        let params: Vec<(&str, Option<String>)> = match sync_token {
            None => vec![
                ("singleEvents", Some("true".into())),
                ("maxResults", Some("250".into())),
                ("timeMin", Some(time_min.clone())),
                ("timeMax", Some(time_max.clone())),
                ("pageToken", page_token.clone()),
            ],
            Some(st) => vec![
                ("syncToken", Some(st.to_owned())),
                ("showDeleted", Some("true".into())),
                ("singleEvents", Some("true".into())),
                ("maxResults", Some("250".into())),
                ("pageToken", page_token.clone()),
            ],
        };
        let data = google::list_events(state, access, calendar_id, &params, T).await?;
        let items = items_of(&data);
        page_token = truthy_str(data.get("nextPageToken"));
        next_sync = truthy_str(data.get("nextSyncToken"));
        total += upsert_events(state, company_id, None, calendar_id, &items).await?;
        if page_token.is_none() {
            break;
        }
    }
    if let Some(st) = next_sync {
        update_audited(
            state,
            audit,
            Model::Company,
            company_id,
            vec![
                Set::Text("googleCalendarSyncToken", Some(st)),
                Set::Ts("updatedAt", Some(now_naive())),
            ],
        )
        .await?;
    }
    Ok(total)
}

/// `res.data.items ?? []`.
pub fn items_of(data: &Value) -> Vec<Value> {
    data.get("items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// Nilai string yang truthy (`?? undefined` lalu dicek truthy oleh loop).
pub fn truthy_str(v: Option<&Value>) -> Option<String> {
    truthy(v).then(|| js_template(v))
}

/// `upsertEvents(companyId, userId, calendarId, items)` — batch per 100 dalam transaksi.
pub async fn upsert_events(
    state: &AppState,
    company_id: i32,
    user_id: Option<i32>,
    calendar_id: &str,
    items: &[Value],
) -> GResult<Counts> {
    let mut counts = Counts::default();
    let mut ops: Vec<GResult<EventUpsert>> = Vec::new();
    for item in items {
        match event_from_item(item, user_id) {
            Ok(None) => continue,
            Ok(Some(e)) => {
                if e.cancelled {
                    counts.cancelled += 1;
                } else {
                    counts.upserted += 1;
                }
                ops.push(Ok(e));
            }
            Err(e) => {
                // Node tetap menghitungnya (status dibaca sebelum Prisma menolak tanggal).
                if item.get("status").and_then(Value::as_str) == Some("cancelled") {
                    counts.cancelled += 1;
                } else {
                    counts.upserted += 1;
                }
                ops.push(Err(e));
            }
        }
    }
    for chunk in ops.chunks(UPSERT_CHUNK_SIZE) {
        if let Some(Err(e)) = chunk.iter().find(|o| o.is_err()) {
            return Err(e.clone());
        }
        let mut tx = state.db.begin().await?;
        for e in chunk.iter().flatten() {
            db::upsert_synced(&mut *tx, company_id, user_id, calendar_id, e).await?;
        }
        tx.commit().await?;
    }
    Ok(counts)
}

// ── kalender bulanan & daftar event ─────────────────────────────────────────

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Attendee {
    email: Value,
    name: Value,
    response_status: Value,
}

/// `extractAttendees(rawMeta)`.
fn extract_attendees(raw: Option<&Value>) -> Vec<Attendee> {
    let Some(list) = raw
        .and_then(|r| r.get("attendees"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    let f = |a: &Value, k: &str| a.get(k).cloned().unwrap_or(Value::Null);
    list.iter()
        .map(|a| Attendee {
            email: f(a, "email"),
            name: f(a, "displayName"),
            response_status: f(a, "responseStatus"),
        })
        .collect()
}

#[derive(sqlx::FromRow)]
struct EventRow {
    uuid: String,
    #[sqlx(rename = "calendarId")]
    calendar_id: String,
    title: String,
    description: Option<String>,
    location: Option<String>,
    #[sqlx(rename = "startAt")]
    start_at: JsDate,
    #[sqlx(rename = "endAt")]
    end_at: JsDate,
    #[sqlx(rename = "isAllDay")]
    is_all_day: bool,
    status: String,
    #[sqlx(rename = "htmlLink")]
    html_link: Option<String>,
    #[sqlx(rename = "userId")]
    user_id: Option<i32>,
    #[sqlx(rename = "syncedAt")]
    synced_at: JsDate,
    #[sqlx(rename = "rawMeta")]
    raw_meta: Option<Json<Value>>,
    source: String,
}

const EVENT_COLS: &str = r#"uuid::text AS uuid, "calendarId", title, description, location, "startAt",
    "endAt", "isAllDay", status, "htmlLink", "userId", "syncedAt", "rawMeta", source"#;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EventItem {
    id: String,
    source: &'static str,
    origin: String,
    calendar_id: String,
    title: String,
    description: Option<String>,
    location: Option<String>,
    start_at: JsDate,
    end_at: JsDate,
    is_all_day: bool,
    status: String,
    html_link: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    synced_at: Option<JsDate>,
    attendees: Vec<Attendee>,
}

impl EventItem {
    fn from_row(e: &EventRow, with_synced: bool) -> Self {
        Self {
            id: e.uuid.clone(),
            source: if e.user_id.is_some() {
                "personal"
            } else {
                "company"
            },
            origin: e.source.clone(),
            calendar_id: e.calendar_id.clone(),
            title: e.title.clone(),
            description: e.description.clone(),
            location: e.location.clone(),
            start_at: e.start_at,
            end_at: e.end_at,
            is_all_day: e.is_all_day,
            status: e.status.clone(),
            html_link: e.html_link.clone(),
            synced_at: with_synced.then_some(e.synced_at),
            attendees: extract_attendees(e.raw_meta.as_ref().map(|j| &j.0)),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OnLeave {
    user_id: i32,
    name: String,
    username: String,
    leave_type: Option<String>,
    leave_type_name: Option<String>,
    start_date: JsDate,
    end_date: JsDate,
    total_days: JsNum,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Day {
    date: String,
    events: Vec<EventItem>,
    on_leave: Vec<OnLeave>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CalendarView {
    year: JsNum,
    month: JsNum,
    google_connected: bool,
    days: Vec<Day>,
}

#[derive(sqlx::FromRow)]
struct LeaveRow {
    #[sqlx(rename = "startDate")]
    start_date: NaiveDate,
    #[sqlx(rename = "endDate")]
    end_date: NaiveDate,
    #[sqlx(rename = "totalDays")]
    total_days: f64,
    #[sqlx(rename = "leaveType")]
    leave_type: Option<String>,
    lt_name: Option<String>,
    lt_code: Option<String>,
    uid: i32,
    uname: String,
    username: String,
}

fn midnight(d: NaiveDate) -> JsDate {
    JsDate(d.and_time(chrono::NaiveTime::MIN))
}

/// `getCalendar(userId, dto)`.
pub async fn get_calendar(
    state: &AppState,
    user_id: i32,
    dto: CalendarMonth,
) -> Envelope<CalendarView> {
    get_calendar_inner(state, user_id, dto)
        .await
        .unwrap_or_else(|e| fail_env(&e, "google-calendar-get-calendar"))
}

async fn get_calendar_inner(
    state: &AppState,
    user_id: i32,
    dto: CalendarMonth,
) -> GResult<Envelope<CalendarView>> {
    let Some(company_id) = mz_core::company::find_company_id(&state.db, user_id).await? else {
        return Ok(Envelope::fail("Company not found"));
    };
    #[allow(clippy::cast_possible_truncation)]
    let (year, month) = (dto.year as i32, dto.month as u32);
    let som =
        NaiveDate::from_ymd_opt(year, month, 1).ok_or_else(|| Fail::Msg("Invalid Date".into()))?;
    let eom = som
        .checked_add_months(chrono::Months::new(1))
        .ok_or_else(|| Fail::Msg("Invalid Date".into()))?;
    let policy_approver: bool = sqlx::query_scalar(
        r#"SELECT EXISTS (SELECT 1 FROM "leavePolicyApprover" WHERE "userId" = $1 AND "companyId" = $2)"#,
    )
    .bind(user_id)
    .bind(company_id)
    .fetch_one(&state.db)
    .await?;

    let refresh: Option<Option<String>> =
        sqlx::query_scalar(r#"SELECT "googleCalendarRefreshToken" FROM company WHERE id = $1"#)
            .bind(company_id)
            .fetch_optional(&state.db)
            .await?;
    let som_ts = som.and_time(chrono::NaiveTime::MIN);
    let eom_ts = eom.and_time(chrono::NaiveTime::MIN);
    let events: Vec<EventRow> = sqlx::query_as(&format!(
        r#"SELECT {EVENT_COLS} FROM "googleCalendarEvent"
           WHERE "companyId" = $1 AND "startAt" < $2 AND "endAt" > $3
             AND ("userId" IS NULL OR "userId" = $4)
             AND "leaveRequestId" IS NULL AND status <> 'cancelled'
           ORDER BY "startAt" ASC"#
    ))
    .bind(company_id)
    .bind(eom_ts)
    .bind(som_ts)
    .bind(user_id)
    .fetch_all(&state.db)
    .await?;
    let approver = if policy_approver {
        r#"(lr."approverId" = $2 OR lr."approverId" IS NULL)"#
    } else {
        r#"lr."approverId" = $2"#
    };
    let leaves: Vec<LeaveRow> = sqlx::query_as(&format!(
        r#"SELECT lr."startDate", lr."endDate", lr."totalDays", lr."leaveType"::text AS "leaveType",
                  clt.name AS lt_name, clt.code AS lt_code,
                  u.id AS uid, u.name AS uname, u.username
           FROM "leaveRequest" lr
           JOIN users u ON u.id = lr."userId"
           LEFT JOIN "companyLeaveType" clt ON clt.id = lr."leaveTypeId"
           WHERE lr."companyId" = $1 AND lr.status = 'APPROVED'
             AND (lr."userId" = $2 OR {approver})
             AND ((lr."startDate" >= $3 AND lr."startDate" < $4)
               OR (lr."endDate" >= $3 AND lr."endDate" < $4)
               OR (lr."startDate" < $3 AND lr."endDate" >= $3))
           ORDER BY lr."startDate" ASC"#
    ))
    .bind(company_id)
    .bind(user_id)
    .bind(som)
    .bind(eom)
    .fetch_all(&state.db)
    .await?;

    let mut days = Vec::new();
    let mut date = som;
    while date < eom {
        let day_start = date.and_time(chrono::NaiveTime::MIN);
        let day_end = day_start + chrono::Duration::days(1);
        let day_events = events
            .iter()
            .filter(|e| e.start_at.0 < day_end && e.end_at.0 > day_start)
            .map(|e| EventItem::from_row(e, false))
            .collect();
        let on_leave = leaves
            .iter()
            .filter(|l| date >= l.start_date && date <= l.end_date)
            .map(|l| OnLeave {
                user_id: l.uid,
                name: l.uname.clone(),
                username: l.username.clone(),
                leave_type: l.lt_code.clone().or_else(|| l.leave_type.clone()),
                leave_type_name: l.lt_name.clone().or_else(|| l.leave_type.clone()),
                start_date: midnight(l.start_date),
                end_date: midnight(l.end_date),
                total_days: JsNum(l.total_days),
            })
            .collect();
        days.push(Day {
            date: format!("{:04}-{:02}-{:02}", date.year(), date.month(), date.day()),
            events: day_events,
            on_leave,
        });
        date = date
            .succ_opt()
            .ok_or_else(|| Fail::Msg("Invalid Date".into()))?;
    }
    Ok(Envelope::ok(
        "Success",
        CalendarView {
            year: JsNum(dto.year),
            month: JsNum(dto.month),
            google_connected: refresh.flatten().is_some_and(|r| !r.is_empty()),
            days,
        },
    ))
}

#[derive(Serialize)]
pub struct Meta {
    pub total: i64,
    pub skip: JsNum,
    pub take: JsNum,
}

/// `{ success, message, data, meta }` atau `{ success: false, message, data: null }`.
#[derive(Serialize)]
pub struct Paged<T> {
    pub success: bool,
    pub message: String,
    pub data: Option<Vec<T>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

impl<T> Paged<T> {
    pub fn fail(message: impl Into<String>) -> Self {
        Self {
            success: false,
            message: message.into(),
            data: None,
            meta: None,
        }
    }
}

/// `skip`/`take` Prisma harus Int (bukan pecahan).
pub fn skip_take(dto: &CalendarQuery) -> GResult<(f64, f64, i64, i64)> {
    let skip = dto.skip.unwrap_or(0.0);
    let take = dto.take.unwrap_or(20.0);
    let as_int = |n: f64, k: &str| {
        #[allow(clippy::cast_possible_truncation)]
        (n.fract() == 0.0).then_some(n as i64).ok_or_else(|| {
            Fail::Msg(format!(
                "Argument `{k}`: Invalid value provided. Expected Int, provided Float."
            ))
        })
    };
    Ok((skip, take, as_int(skip, "skip")?, as_int(take, "take")?))
}

/// `getEvents(userId, dto)` (company).
pub async fn get_events(state: &AppState, user_id: i32, dto: CalendarQuery) -> Paged<EventItem> {
    let run = async {
        let Some(company_id) = mz_core::company::find_company_id(&state.db, user_id).await? else {
            return GResult::Ok(Paged::fail("Company not found"));
        };
        let (skip, take, skip_i, take_i) = skip_take(&dto)?;
        let start = parse_or_fail(&dto.start_date, "startAt")?;
        let end = parse_or_fail(&dto.end_date, "endAt")?;
        let calendar = dto.calendar_id.clone().filter(|c| !c.is_empty());
        let source = dto.source.clone();
        let filter = |qb: &mut QueryBuilder<'_, Postgres>| {
            qb.push(r#" WHERE "companyId" = "#).push_bind(company_id);
            qb.push(r#" AND "startAt" >= "#).push_bind(start);
            qb.push(r#" AND "endAt" <= "#).push_bind(end);
            qb.push(r#" AND "leaveRequestId" IS NULL"#);
            if let Some(c) = &calendar {
                qb.push(r#" AND "calendarId" = "#).push_bind(c.clone());
            }
            match source.as_deref() {
                Some("google") => {
                    qb.push(r#" AND "userId" IS NULL"#);
                }
                Some("internal") => {
                    qb.push(r#" AND "userId" = "#).push_bind(user_id);
                }
                _ => {
                    qb.push(r#" AND ("userId" IS NULL OR "userId" = "#)
                        .push_bind(user_id)
                        .push(")");
                }
            }
        };
        let mut cq = QueryBuilder::new(r#"SELECT COUNT(*) FROM "googleCalendarEvent""#);
        filter(&mut cq);
        let total: i64 = cq.build_query_scalar().fetch_one(&state.db).await?;
        let mut lq =
            QueryBuilder::new(format!(r#"SELECT {EVENT_COLS} FROM "googleCalendarEvent""#));
        filter(&mut lq);
        lq.push(r#" ORDER BY "startAt" ASC OFFSET "#)
            .push_bind(skip_i)
            .push(" LIMIT ")
            .push_bind(take_i);
        let rows: Vec<EventRow> = lq.build_query_as().fetch_all(&state.db).await?;
        Ok(Paged {
            success: true,
            message: "Success".into(),
            data: Some(rows.iter().map(|e| EventItem::from_row(e, true)).collect()),
            meta: Some(Meta {
                total,
                skip: JsNum(skip),
                take: JsNum(take),
            }),
        })
    };
    run.await.unwrap_or_else(|e| {
        tracing::error!(error = %e.message(), "google-calendar-get-events");
        Paged::fail(e.message())
    })
}

/// Satu kalender `listCalendars` (`undefined` = kunci tidak ditulis).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CalendarEntry {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<Value>,
    description: Value,
    primary: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    access_role: Option<Value>,
    background_color: Value,
}

/// `listCalendars(userId)`.
pub async fn list_calendars(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
) -> Envelope<Vec<CalendarEntry>> {
    let run = async {
        let Some(company_id) = mz_core::company::find_company_id(&state.db, user_id).await? else {
            return GResult::Ok(Envelope::fail("Company not found"));
        };
        let token = match get_valid_token(state, audit, company_id).await? {
            Some(t) if !t.is_empty() => t,
            _ => return Ok(Envelope::fail("Google Calendar not connected")),
        };
        let data = google::calendar_list(state, &token).await?;
        let nn = |c: &Value, k: &str, d: Value| match c.get(k) {
            None | Some(Value::Null) => d,
            Some(v) => v.clone(),
        };
        let list = items_of(&data)
            .iter()
            .map(|c| CalendarEntry {
                id: c.get("id").cloned(),
                summary: c.get("summary").cloned(),
                description: nn(c, "description", Value::Null),
                primary: nn(c, "primary", Value::Bool(false)),
                access_role: c.get("accessRole").cloned(),
                background_color: nn(c, "backgroundColor", Value::Null),
            })
            .collect();
        Ok(Envelope::ok("Success", list))
    };
    run.await
        .unwrap_or_else(|e| fail_env(&e, "google-calendar-list-calendars"))
}

// ── event company ───────────────────────────────────────────────────────────

#[derive(Serialize, Clone)]
pub struct Creator {
    id: i32,
    name: String,
    username: String,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct AttendeeOut {
    user_id: i32,
    name: String,
    email: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreatedEvent {
    id: String,
    origin: String,
    google_event_id: Option<String>,
    title: String,
    description: Option<String>,
    location: Option<String>,
    start_at: JsDate,
    end_at: JsDate,
    is_all_day: bool,
    status: String,
    html_link: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    created_by: Option<Option<Creator>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    attendees: Option<Vec<AttendeeOut>>,
}

impl CreatedEvent {
    pub fn personal(s: db::SavedEvent) -> Self {
        Self::base(s, None, None)
    }

    fn base(
        s: db::SavedEvent,
        created_by: Option<Option<Creator>>,
        attendees: Option<Vec<AttendeeOut>>,
    ) -> Self {
        Self {
            id: s.uuid,
            origin: s.source,
            google_event_id: s.google_event_id,
            title: s.title,
            description: s.description,
            location: s.location,
            start_at: s.start_at,
            end_at: s.end_at,
            is_all_day: s.is_all_day,
            status: s.status,
            html_link: s.html_link,
            created_by,
            attendees,
        }
    }
}

/// Payload `events.insert` (`start`/`end` `date` untuk event seharian).
pub fn insert_payload(dto: &CreateEvent, attendees: &[AttendeeOut]) -> Value {
    let when = |s: &str| {
        if dto.is_all_day == Some(true) {
            let date = String::from_utf16_lossy(&s.encode_utf16().take(10).collect::<Vec<_>>());
            json!({ "date": date })
        } else {
            json!({ "dateTime": s })
        }
    };
    let mut p = Map::new();
    p.insert("summary".into(), Value::from(dto.title.clone()));
    p.insert("start".into(), when(&dto.start_at));
    p.insert("end".into(), when(&dto.end_at));
    if let Some(d) = dto.description.as_deref().filter(|d| !d.is_empty()) {
        p.insert("description".into(), Value::from(d));
    }
    if let Some(l) = dto.location.as_deref().filter(|l| !l.is_empty()) {
        p.insert("location".into(), Value::from(l));
    }
    if !attendees.is_empty() {
        p.insert(
            "attendees".into(),
            Value::Array(
                attendees
                    .iter()
                    .map(|a| json!({ "email": a.email }))
                    .collect(),
            ),
        );
    }
    Value::Object(p)
}

/// `createCompanyEvent(userId, dto)`.
pub async fn create_company_event(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    dto: CreateEvent,
) -> Envelope<CreatedEvent> {
    create_company_event_inner(state, audit, user_id, dto)
        .await
        .unwrap_or_else(|e| fail_env(&e, "google-calendar-create-company-event"))
}

async fn create_company_event_inner(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    dto: CreateEvent,
) -> GResult<Envelope<CreatedEvent>> {
    let Some(company_id) = mz_core::company::find_company_id(&state.db, user_id).await? else {
        return Ok(Envelope::fail("Company not found"));
    };
    let start = parse_or_fail(&dto.start_at, "startAt")?;
    let end = parse_or_fail(&dto.end_at, "endAt")?;
    if end <= start {
        return Ok(Envelope::fail("endAt must be after startAt"));
    }
    let creator: Option<Creator> = sqlx::query_as::<_, (i32, String, String)>(
        "SELECT id, name, username FROM users WHERE id = $1",
    )
    .bind(user_id)
    .fetch_optional(&state.db)
    .await?
    .map(|(id, name, username)| Creator { id, name, username });

    let mut attendees: Vec<AttendeeOut> = Vec::new();
    if let Some(ids) = dto.attendee_user_ids.as_ref().filter(|v| !v.is_empty()) {
        let mut unique: Vec<i32> = Vec::new();
        for n in ids {
            #[allow(clippy::cast_possible_truncation)]
            let id = *n as i32;
            if !unique.contains(&id) {
                unique.push(id);
            }
        }
        let valid: Vec<(i32, String, String, Option<String>)> = sqlx::query_as(
            r#"SELECT u.id, u.name, u.username, u."googleCalendarEmail" FROM users u
               WHERE u.id = ANY($1) AND EXISTS (
                 SELECT 1 FROM "userCompany" uc
                 WHERE uc."userId" = u.id AND uc."companyId" = $2 AND uc.status = 1)"#,
        )
        .bind(&unique)
        .bind(company_id)
        .fetch_all(&state.db)
        .await?;
        if valid.len() != unique.len() {
            return Ok(Envelope::fail(
                "Invalid attendee: one or more users not found in this company",
            ));
        }
        attendees = valid
            .into_iter()
            .map(|(id, name, username, gmail)| AttendeeOut {
                user_id: id,
                name,
                email: gmail.unwrap_or(username),
            })
            .collect();
    }

    let access = get_valid_token(state, audit, company_id).await?;
    let mut google_event_id: Option<String> = None;
    let mut html_link: Option<String> = None;
    let mut raw: Option<Value> = (!attendees.is_empty()).then(|| {
        json!({ "attendees": attendees.iter().map(|a| json!({ "email": a.email, "displayName": a.name })).collect::<Vec<_>>() })
    });
    let mut source = "local";
    if let Some(token) = access.filter(|t| !t.is_empty()) {
        let payload = insert_payload(&dto, &attendees);
        match google::insert_event(state, &token, &payload, T).await {
            Ok(data) => {
                google_event_id = data
                    .get("id")
                    .and_then(|v| truthy(Some(v)).then(|| js_template(Some(v))));
                html_link = opt_str(data.get("htmlLink"));
                raw = Some(data);
                source = "google";
            }
            Err(e) if matches!(e.http_status(), Some(401 | 403)) => {}
            Err(e) => return Err(e),
        }
    }
    let saved = db::save_new_event(
        state,
        &NewEvent {
            company_id,
            user_id: None,
            created_by: user_id,
            calendar_id: DEFAULT_CALENDAR_ID.into(),
            google_event_id,
            title: dto.title.clone(),
            description: dto.description.clone(),
            location: dto.location.clone(),
            start_at: start,
            end_at: end,
            is_all_day: dto.is_all_day.unwrap_or(false),
            html_link,
            raw,
            source,
        },
    )
    .await?;
    if source == "local" && !attendees.is_empty() {
        send_local_event_invites(
            state,
            company_id,
            creator.clone(),
            &saved.uuid,
            &dto,
            start,
            end,
            &attendees,
        );
    }
    let message = if source == "google" {
        "Event created and synced to Google Calendar"
    } else {
        "Event created locally (Google Calendar not connected for this company)"
    };
    Ok(Envelope::ok(
        message,
        CreatedEvent::base(saved, Some(creator), Some(attendees)),
    ))
}

/// `sendLocalEventInvites` — fire-and-forget.
#[allow(clippy::too_many_arguments)]
fn send_local_event_invites(
    state: &AppState,
    company_id: i32,
    creator: Option<Creator>,
    uid: &str,
    dto: &CreateEvent,
    start: NaiveDateTime,
    end: NaiveDateTime,
    attendees: &[AttendeeOut],
) {
    let state = state.clone();
    let (uid, dto, attendees) = (uid.to_owned(), dto.clone(), attendees.to_vec());
    mz_core::bg::spawn(async move {
        let company: Option<String> =
            match sqlx::query_scalar("SELECT name FROM company WHERE id = $1")
                .bind(company_id)
                .fetch_optional(&state.db)
                .await
            {
                Ok(n) => n,
                Err(e) => {
                    tracing::error!(error = %e, "google-calendar-invite-email");
                    return;
                }
            };
        let sends = attendees.iter().map(|a| {
            let invite = Invite {
                to: a.email.clone(),
                attendee_name: a.name.clone(),
                organizer_name: creator
                    .as_ref()
                    .map_or_else(|| "A teammate".into(), |c| c.name.clone()),
                organizer_email: creator
                    .as_ref()
                    .map(|c| c.username.clone())
                    .unwrap_or_default(),
                company_name: company.clone().unwrap_or_else(|| "your company".into()),
                uid: uid.clone(),
                title: dto.title.clone(),
                description: dto.description.clone(),
                location: dto.location.clone(),
                start_at: start,
                end_at: end,
                is_all_day: dto.is_all_day.unwrap_or(false),
            };
            let state = state.clone();
            async move { (invite.to.clone(), send_invite(&state, &invite).await) }
        });
        for (email, result) in futures::future::join_all(sends).await {
            if !result.success {
                tracing::warn!(email = %email, message = %result.message, "Failed to send calendar invite");
            }
        }
    });
}

/// `deleteCompanyEvent(userId, uuid)`.
pub async fn delete_company_event(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    uuid: &str,
) -> Envelope<()> {
    let run = async {
        let Some(company_id) = mz_core::company::find_company_id(&state.db, user_id).await? else {
            return GResult::Ok(Envelope::fail("Company not found"));
        };
        let event: Option<(i32, Option<String>, String)> = sqlx::query_as(
            r#"SELECT id, "googleEventId", "calendarId" FROM "googleCalendarEvent"
               WHERE uuid::text = $1 AND "companyId" = $2 LIMIT 1"#,
        )
        .bind(uuid)
        .bind(company_id)
        .fetch_optional(&state.db)
        .await?;
        let Some((id, google_id, calendar_id)) = event else {
            return Ok(Envelope::fail("Event not found"));
        };
        if let Some(gid) = google_id.filter(|g| !g.is_empty()) {
            let token = match get_valid_token(state, audit, company_id).await? {
                Some(t) if !t.is_empty() => t,
                _ => {
                    return Ok(Envelope::fail(
                        "Google Calendar not connected for this company",
                    ));
                }
            };
            match google::delete_event(state, &token, &calendar_id, &gid, T).await {
                Ok(_) => {}
                Err(e) if e.http_status() == Some(404) => {}
                Err(e) => return Err(e),
            }
        }
        sqlx::query(r#"DELETE FROM "googleCalendarEvent" WHERE id = $1"#)
            .bind(id)
            .execute(&state.db)
            .await?;
        Ok(ok_null("Event deleted"))
    };
    run.await
        .unwrap_or_else(|e| fail_env(&e, "google-calendar-delete-company-event"))
}

// ── webhook ─────────────────────────────────────────────────────────────────

/// `registerWebhookChannel(companyId, accessToken)`.
pub async fn register_webhook_channel(
    state: &AppState,
    audit: &AuditHandle,
    company_id: i32,
    access: &str,
) -> GResult<()> {
    let Some(address) = state.config.var("GOOGLE_CALENDAR_WEBHOOK_URL") else {
        tracing::warn!("GOOGLE_CALENDAR_WEBHOOK_URL not set, skipping webhook registration");
        return Ok(());
    };
    let channel_id = uuid::Uuid::new_v4().to_string();
    let expiration = chrono::Utc::now().timestamp_millis() + 7 * 24 * 60 * 60 * 1000;
    let data = google::watch(
        state,
        access,
        &json!({
            "id": channel_id,
            "type": "web_hook",
            "address": address,
            "expiration": expiration.to_string(),
        }),
    )
    .await?;
    // `new Date(parseInt(res.data.expiration, 10))`.
    let exp_ms = parse_int(data.get("expiration"));
    let expiry = exp_ms
        .and_then(JsDate::from_millis)
        .ok_or_else(|| Fail::Msg(db::invalid_date("googleCalendarWebhookExpiry")))?;
    let channel = opt_str(data.get("id"));
    update_audited(
        state,
        audit,
        Model::Company,
        company_id,
        vec![
            Set::Text("googleCalendarWebhookChannelId", channel.clone()),
            Set::Text(
                "googleCalendarWebhookResourceId",
                opt_str(data.get("resourceId")),
            ),
            Set::Ts("googleCalendarWebhookExpiry", Some(expiry.0)),
            Set::Ts("updatedAt", Some(now_naive())),
        ],
    )
    .await?;
    tracing::info!(
        company_id,
        channel_id = %js_opt(channel),
        "Google Calendar webhook registered"
    );
    Ok(())
}

/// `parseInt(x, 10)` (awalan digit; angka JSON dibaca lewat `String(x)`).
fn parse_int(v: Option<&Value>) -> Option<i64> {
    let s = crate::util::js_string(v?);
    let t = s.trim_start();
    let (neg, digits) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let end = digits
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(digits.len());
    let n: i64 = digits.get(..end)?.parse().ok()?;
    Some(if neg { -n } else { n })
}

async fn stop_webhook_channel(
    state: &AppState,
    channel_id: &str,
    resource_id: &str,
    access: &str,
) -> GResult<()> {
    google::stop_channel(
        state,
        access,
        &json!({ "id": channel_id, "resourceId": resource_id }),
    )
    .await
    .map(|_| ())
}

/// `handleWebhookNotification(channelId, resourceState)`.
pub async fn handle_webhook_notification(
    state: &AppState,
    audit: &AuditHandle,
    channel_id: Option<&str>,
    resource_state: Option<&str>,
) -> GResult<()> {
    let Some(channel_id) = channel_id.filter(|c| !c.is_empty()) else {
        return Ok(());
    };
    if resource_state == Some("sync") {
        return Ok(());
    }
    let company: Option<i32> = sqlx::query_scalar(
        r#"SELECT id FROM company WHERE "googleCalendarWebhookChannelId" = $1 LIMIT 1"#,
    )
    .bind(channel_id)
    .fetch_optional(&state.db)
    .await?;
    let Some(company_id) = company else {
        return Ok(());
    };
    if let Err(e) = sync_company_calendar(state, audit, company_id, DEFAULT_CALENDAR_ID).await {
        tracing::error!(company_id, error = %e.message(), "Webhook-triggered sync failed");
    }
    Ok(())
}

// ── cron ────────────────────────────────────────────────────────────────────

/// `scheduledSyncAllCompanies` (`@Cron('0 35 * * * *')`).
pub async fn scheduled_sync_all_companies(state: &AppState) -> anyhow::Result<()> {
    tracing::info!("Starting scheduled Google Calendar sync...");
    let audit = AuditHandle::detached(state.db.clone());
    let ids: Vec<i32> = sqlx::query_scalar(
        r#"SELECT id FROM company WHERE "googleCalendarRefreshToken" IS NOT NULL"#,
    )
    .fetch_all(&state.db)
    .await?;
    let (mut synced, mut skipped) = (0, 0);
    for &id in &ids {
        if is_credential_dead(id) {
            skipped += 1;
            continue;
        }
        match sync_company_calendar(state, &audit, id, DEFAULT_CALENDAR_ID).await {
            Ok(_) => synced += 1,
            Err(e) => {
                tracing::error!(company_id = id, error = %e.message(), "google-calendar-scheduled-sync")
            }
        }
    }
    let suffix = if skipped > 0 {
        format!(", {skipped} skipped (invalid_grant backoff)")
    } else {
        String::new()
    };
    tracing::info!(
        "Scheduled Google Calendar sync complete: {synced}/{} companies synced{suffix}",
        ids.len()
    );
    Ok(())
}

/// `scheduledRenewWebhookChannels` (`@Cron('0 40 1,22 * * *', { timeZone: CRON_TIMEZONE })`).
pub async fn scheduled_renew_webhook_channels(state: &AppState) -> anyhow::Result<()> {
    tracing::info!("Checking Google Calendar webhook channels for renewal...");
    let audit = AuditHandle::detached(state.db.clone());
    let threshold = now_naive() + chrono::Duration::hours(24);
    let rows: Vec<(i32, Option<String>, Option<String>)> = sqlx::query_as(
        r#"SELECT id, "googleCalendarWebhookChannelId", "googleCalendarWebhookResourceId"
           FROM company
           WHERE "googleCalendarWebhookChannelId" IS NOT NULL
             AND "googleCalendarWebhookExpiry" <= $1
             AND "googleCalendarRefreshToken" IS NOT NULL"#,
    )
    .bind(threshold)
    .fetch_all(&state.db)
    .await?;
    let mut renewed = 0;
    for (id, channel, resource) in &rows {
        if is_credential_dead(*id) {
            continue;
        }
        let run = async {
            let Some(token) = get_valid_token(state, &audit, *id)
                .await?
                .filter(|t| !t.is_empty())
            else {
                return GResult::Ok(false);
            };
            if let (Some(ch), Some(res)) = (
                channel.as_deref().filter(|s| !s.is_empty()),
                resource.as_deref().filter(|s| !s.is_empty()),
            ) {
                let _ = stop_webhook_channel(state, ch, res, &token).await;
            }
            register_webhook_channel(state, &audit, *id, &token).await?;
            Ok(true)
        };
        match run.await {
            Ok(true) => renewed += 1,
            Ok(false) => {}
            Err(e) => {
                tracing::error!(company_id = id, error = %e.message(), "google-calendar-webhook-renew")
            }
        }
    }
    tracing::info!("Webhook renewal complete: {renewed}/{} renewed", rows.len());
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn parse_int_like_js() {
        assert_eq!(
            parse_int(Some(&json!("1700000000000"))),
            Some(1_700_000_000_000)
        );
        assert_eq!(parse_int(Some(&json!(12))), Some(12));
        assert_eq!(parse_int(Some(&json!("12abc"))), Some(12));
        assert_eq!(parse_int(Some(&json!("abc"))), None);
        assert_eq!(parse_int(None), None);
    }

    #[test]
    fn payload_all_day() {
        let dto = CreateEvent {
            title: "T".into(),
            description: Some(String::new()),
            location: Some("Zoom".into()),
            start_at: "2026-05-20T09:00:00+07:00".into(),
            end_at: "2026-05-21T09:00:00+07:00".into(),
            is_all_day: Some(true),
            attendee_user_ids: None,
        };
        let p = insert_payload(&dto, &[]);
        assert_eq!(
            p,
            json!({"summary": "T", "start": {"date": "2026-05-20"}, "end": {"date": "2026-05-21"}, "location": "Zoom"})
        );
    }

    #[test]
    fn attendees_from_raw() {
        let raw = json!({"attendees": [{"email": "a@b.c", "displayName": "A"}]});
        let a = extract_attendees(Some(&raw));
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            r#"[{"email":"a@b.c","name":"A","responseStatus":null}]"#
        );
        assert!(extract_attendees(None).is_empty());
    }
}
