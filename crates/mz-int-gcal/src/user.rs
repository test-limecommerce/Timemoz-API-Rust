//! `GoogleCalendarUserService` (`google-calendar-user.service.ts`): kalender pribadi per user.
//! Beda dengan service company: axios TANPA `timeout`, tidak ada backoff `invalid_grant`,
//! tidak ada kunci sync, sync penuh memakai `orderBy=startTime`, dan upsert per event.

use chrono::NaiveDateTime;
use mz_audit::{AuditHandle, Model};
use mz_core::AppState;
use mz_core::time::{JsDate, now_naive, to_js_iso};
use mz_int_common::envelope::Envelope;
use serde::Serialize;
use serde_json::Value;
use sqlx::{Postgres, QueryBuilder};

use crate::company::{
    CreatedEvent, Meta, Paged, auth_url, frontend_url, insert_payload, items_of, js_opt, ok_null,
    skip_take, truthy_str, user_id_of, verify_state,
};
use crate::db::{
    self, Counts, NewEvent, Set, event_from_item, expiry_from, opt_str, parse_or_fail,
    update_audited,
};
use crate::dto::{CalendarQuery, CreateEvent};
use crate::google::{self, Fail, GResult};
use crate::util::{encode_uri_component, js_template, truthy};

const USER_CALENDAR_SCOPES: &str = "https://www.googleapis.com/auth/calendar.events https://www.googleapis.com/auth/userinfo.email";

fn personal_calendar(user_id: i32) -> String {
    format!("personal:{user_id}")
}

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

async fn token_row(state: &AppState, user_id: i32) -> sqlx::Result<Option<TokenRow>> {
    sqlx::query_as(
        r#"SELECT "googleCalendarAccessToken", "googleCalendarRefreshToken",
                  "googleCalendarTokenExpiresAt", "googleCalendarSyncToken"
           FROM users WHERE id = $1"#,
    )
    .bind(user_id)
    .fetch_optional(&state.db)
    .await
}

fn needs_refresh(expires_at: Option<NaiveDateTime>) -> bool {
    expires_at.is_none_or(|e| e <= now_naive() + chrono::Duration::minutes(5))
}

/// `refreshUserToken`.
async fn refresh_user_token(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    refresh: &str,
) -> GResult<Option<String>> {
    let params = [
        ("grant_type", Some("refresh_token".to_owned())),
        ("client_id", state.config.var("GOOGLE_CALENDAR_CLIENT_ID")),
        (
            "client_secret",
            state.config.var("GOOGLE_CALENDAR_CLIENT_SECRET"),
        ),
        ("refresh_token", Some(refresh.to_owned())),
    ];
    let data = google::token(state, &params, None).await?;
    let access = opt_str(data.get("access_token"));
    let expires = expiry_from(data.get("expires_in"))?;
    let mut sets = Vec::new();
    if let Some(a) = &access {
        sets.push(Set::Text("googleCalendarAccessToken", Some(a.clone())));
    }
    sets.push(Set::Ts("googleCalendarTokenExpiresAt", Some(expires)));
    sets.push(Set::Ts("updatedAt", Some(now_naive())));
    update_audited(state, audit, Model::Users, user_id, sets).await?;
    Ok(access)
}

/// `getValidUserToken`.
async fn get_valid_user_token(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
) -> GResult<Option<String>> {
    let Some(row) = token_row(state, user_id).await? else {
        return Ok(None);
    };
    let Some(refresh) = row.refresh.filter(|r| !r.is_empty()) else {
        return Ok(None);
    };
    if needs_refresh(row.expires_at) {
        return refresh_user_token(state, audit, user_id, &refresh).await;
    }
    Ok(row.access)
}

/// `generateAuthUrl(userId)` (personal).
pub fn generate_auth_url(state: &AppState, user_id: i32) -> String {
    auth_url(
        state,
        user_id,
        "user",
        "GOOGLE_CALENDAR_USER_REDIRECT_URI",
        USER_CALENDAR_SCOPES,
    )
}

/// `handleCallback(code, state)` (personal) → `redirectUrl`.
pub async fn handle_callback(
    state: &AppState,
    audit: &AuditHandle,
    code: Option<String>,
    st: Option<String>,
) -> String {
    let fe = frontend_url(state);
    let error_url = |msg: &str| {
        format!(
            "{fe}/settings/integrations?error=google_calendar_personal&message={}",
            encode_uri_component(msg)
        )
    };
    let user = match verify_state(state, st.as_deref(), "user") {
        Ok(u) => u,
        Err(msg) => return error_url(msg),
    };
    match callback_inner(state, audit, &user, code).await {
        Ok(()) => format!("{fe}/settings/integrations?connected=google_calendar_personal"),
        Err(err) => {
            tracing::error!(error = %err.message(), "google-calendar-user-callback");
            error_url(&err.message())
        }
    }
}

async fn callback_inner(
    state: &AppState,
    audit: &AuditHandle,
    user: &Value,
    code: Option<String>,
) -> GResult<()> {
    let user_id = user_id_of(user)?;
    let params = [
        ("grant_type", Some("authorization_code".to_owned())),
        ("client_id", state.config.var("GOOGLE_CALENDAR_CLIENT_ID")),
        (
            "client_secret",
            state.config.var("GOOGLE_CALENDAR_CLIENT_SECRET"),
        ),
        ("code", code),
        (
            "redirect_uri",
            state.config.var("GOOGLE_CALENDAR_USER_REDIRECT_URI"),
        ),
    ];
    let data = google::token(state, &params, None).await?;
    let access = opt_str(data.get("access_token"));
    let refresh = opt_str(data.get("refresh_token"));
    let expires = expiry_from(data.get("expires_in"))?;
    let email = match google::userinfo(state, access.as_deref().unwrap_or("undefined"), None).await
    {
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
    update_audited(state, audit, Model::Users, user_id, sets).await?;

    let (state, audit) = (state.clone(), audit.clone());
    mz_core::bg::spawn(async move {
        if let Err(e) = sync_user_calendar(&state, &audit, user_id).await {
            tracing::warn!(user_id, error = %e.message(), "Initial personal sync failed");
        }
    });
    Ok(())
}

fn fail_env<T>(err: &Fail, module: &str) -> Envelope<T> {
    tracing::error!(module, error = %err.message(), "google-calendar-user");
    Envelope::fail(err.message())
}

/// `disconnect(userId)` (personal).
pub async fn disconnect(state: &AppState, audit: &AuditHandle, user_id: i32) -> Envelope<()> {
    let run = async {
        let access: Option<Option<String>> =
            sqlx::query_scalar(r#"SELECT "googleCalendarAccessToken" FROM users WHERE id = $1"#)
                .bind(user_id)
                .fetch_optional(&state.db)
                .await?;
        if let Some(a) = access.flatten().filter(|a| !a.is_empty()) {
            let _ = google::revoke(state, &a, None).await;
        }
        update_audited(
            state,
            audit,
            Model::Users,
            user_id,
            vec![
                Set::Text("googleCalendarAccessToken", None),
                Set::Text("googleCalendarRefreshToken", None),
                Set::Ts("googleCalendarTokenExpiresAt", None),
                Set::Text("googleCalendarEmail", None),
                Set::Text("googleCalendarSyncToken", None),
                Set::Ts("updatedAt", Some(now_naive())),
            ],
        )
        .await?;
        if let Some(company_id) = mz_core::company::find_company_id(&state.db, user_id).await? {
            sqlx::query(
                r#"DELETE FROM "googleCalendarEvent"
                   WHERE "userId" = $1 AND "companyId" = $2 AND source = 'google'"#,
            )
            .bind(user_id)
            .bind(company_id)
            .execute(&state.db)
            .await?;
        }
        GResult::Ok(ok_null("Personal Google Calendar disconnected"))
    };
    run.await
        .unwrap_or_else(|e| fail_env(&e, "google-calendar-user-disconnect"))
}

#[derive(Serialize)]
pub struct Status {
    connected: bool,
    email: Option<String>,
}

/// `getStatus(userId)` (personal).
pub async fn get_status(state: &AppState, user_id: i32) -> Envelope<Status> {
    let row: Result<Option<(Option<String>, Option<String>)>, _> = sqlx::query_as(
        r#"SELECT "googleCalendarEmail", "googleCalendarRefreshToken" FROM users WHERE id = $1"#,
    )
    .bind(user_id)
    .fetch_optional(&state.db)
    .await;
    match row {
        Ok(r) => {
            let (email, refresh) = r.unwrap_or_default();
            Envelope::ok(
                "Success",
                Status {
                    connected: refresh.is_some_and(|r| !r.is_empty()),
                    email,
                },
            )
        }
        Err(e) => fail_env(&Fail::from(e), "google-calendar-user-status"),
    }
}

/// `syncUserCalendar(userId)`.
pub async fn sync_user_calendar(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
) -> GResult<Counts> {
    let Some(company_id) = mz_core::company::find_company_id(&state.db, user_id).await? else {
        return Ok(Counts::default());
    };
    let Some(row) = token_row(state, user_id).await? else {
        return Ok(Counts::default());
    };
    let Some(refresh) = row.refresh.clone().filter(|r| !r.is_empty()) else {
        return Ok(Counts::default());
    };
    let access = if needs_refresh(row.expires_at) {
        js_opt(refresh_user_token(state, audit, user_id, &refresh).await?)
    } else {
        row.access.clone().unwrap_or_else(|| "null".into())
    };
    let calendar_id = personal_calendar(user_id);
    if let Some(st) = row.sync_token.filter(|s| !s.is_empty()) {
        match user_page_sync(
            state,
            audit,
            user_id,
            company_id,
            &calendar_id,
            &access,
            Some(&st),
        )
        .await
        {
            Err(e) if e.http_status() == Some(410) => {
                update_audited(
                    state,
                    audit,
                    Model::Users,
                    user_id,
                    vec![
                        Set::Text("googleCalendarSyncToken", None),
                        Set::Ts("updatedAt", Some(now_naive())),
                    ],
                )
                .await?;
                return user_page_sync(
                    state,
                    audit,
                    user_id,
                    company_id,
                    &calendar_id,
                    &access,
                    None,
                )
                .await;
            }
            other => return other,
        }
    }
    user_page_sync(
        state,
        audit,
        user_id,
        company_id,
        &calendar_id,
        &access,
        None,
    )
    .await
}

/// `fullUserSync` / `incrementalUserSync`.
async fn user_page_sync(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
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
    let mut counts = Counts::default();
    loop {
        let params: Vec<(&str, Option<String>)> = match sync_token {
            None => vec![
                ("singleEvents", Some("true".into())),
                ("orderBy", Some("startTime".into())),
                ("maxResults", Some("250".into())),
                ("timeMin", Some(time_min.clone())),
                ("timeMax", Some(time_max.clone())),
                ("pageToken", page_token.clone()),
            ],
            Some(st) => vec![
                ("syncToken", Some(st.to_owned())),
                ("showDeleted", Some("true".into())),
                ("pageToken", page_token.clone()),
            ],
        };
        let data = google::list_events(state, access, "primary", &params, None).await?;
        page_token = truthy_str(data.get("nextPageToken"));
        next_sync = truthy_str(data.get("nextSyncToken"));
        for item in items_of(&data) {
            // `upsertUserEvent`: satu upsert per event (tanpa transaksi).
            if let Some(e) = event_from_item(&item, Some(user_id))? {
                db::upsert_synced(&state.db, company_id, Some(user_id), calendar_id, &e).await?;
                if e.cancelled {
                    counts.cancelled += 1;
                } else {
                    counts.upserted += 1;
                }
            }
        }
        if page_token.is_none() {
            break;
        }
    }
    if let Some(st) = next_sync {
        update_audited(
            state,
            audit,
            Model::Users,
            user_id,
            vec![
                Set::Text("googleCalendarSyncToken", Some(st)),
                Set::Ts("updatedAt", Some(now_naive())),
            ],
        )
        .await?;
    }
    Ok(counts)
}

#[derive(sqlx::FromRow)]
struct EventRow {
    uuid: String,
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
    #[sqlx(rename = "syncedAt")]
    synced_at: JsDate,
    source: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PersonalEvent {
    id: String,
    source: &'static str,
    origin: String,
    title: String,
    description: Option<String>,
    location: Option<String>,
    start_at: JsDate,
    end_at: JsDate,
    is_all_day: bool,
    status: String,
    html_link: Option<String>,
    synced_at: JsDate,
}

/// `getEvents(userId, dto)` (personal).
pub async fn get_events(
    state: &AppState,
    user_id: i32,
    dto: CalendarQuery,
) -> Paged<PersonalEvent> {
    let run = async {
        let Some(company_id) = mz_core::company::find_company_id(&state.db, user_id).await? else {
            return GResult::Ok(Paged::fail("Company not found"));
        };
        let (skip, take, skip_i, take_i) = skip_take(&dto)?;
        let start = parse_or_fail(&dto.start_date, "startAt")?;
        let end = parse_or_fail(&dto.end_date, "endAt")?;
        let calendar_id = personal_calendar(user_id);
        let filter = |qb: &mut QueryBuilder<'_, Postgres>| {
            qb.push(r#" WHERE "userId" = "#).push_bind(user_id);
            qb.push(r#" AND "companyId" = "#).push_bind(company_id);
            qb.push(r#" AND "calendarId" = "#)
                .push_bind(calendar_id.clone());
            qb.push(r#" AND "startAt" >= "#).push_bind(start);
            qb.push(r#" AND "endAt" <= "#).push_bind(end);
        };
        let mut cq = QueryBuilder::new(r#"SELECT COUNT(*) FROM "googleCalendarEvent""#);
        filter(&mut cq);
        let total: i64 = cq.build_query_scalar().fetch_one(&state.db).await?;
        let mut lq = QueryBuilder::new(
            r#"SELECT uuid::text AS uuid, title, description, location, "startAt", "endAt",
                      "isAllDay", status, "htmlLink", "syncedAt", source
               FROM "googleCalendarEvent""#,
        );
        filter(&mut lq);
        lq.push(r#" ORDER BY "startAt" ASC OFFSET "#)
            .push_bind(skip_i)
            .push(" LIMIT ")
            .push_bind(take_i);
        let rows: Vec<EventRow> = lq.build_query_as().fetch_all(&state.db).await?;
        let data = rows
            .into_iter()
            .map(|e| PersonalEvent {
                id: e.uuid,
                source: "personal",
                origin: e.source,
                title: e.title,
                description: e.description,
                location: e.location,
                start_at: e.start_at,
                end_at: e.end_at,
                is_all_day: e.is_all_day,
                status: e.status,
                html_link: e.html_link,
                synced_at: e.synced_at,
            })
            .collect();
        Ok(Paged {
            success: true,
            message: "Success".into(),
            data: Some(data),
            meta: Some(Meta {
                total,
                skip: crate::util::JsNum(skip),
                take: crate::util::JsNum(take),
            }),
        })
    };
    run.await.unwrap_or_else(|e| {
        tracing::error!(error = %e.message(), "google-calendar-user-get-events");
        Paged::fail(e.message())
    })
}

/// `createUserEvent(userId, dto)`.
pub async fn create_user_event(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    dto: CreateEvent,
) -> Envelope<CreatedEvent> {
    let run = async {
        let start = parse_or_fail(&dto.start_at, "startAt")?;
        let end = parse_or_fail(&dto.end_at, "endAt")?;
        if end <= start {
            return GResult::Ok(Envelope::fail("endAt must be after startAt"));
        }
        let Some(company_id) = mz_core::company::find_company_id(&state.db, user_id).await? else {
            return Ok(Envelope::fail("Company not found"));
        };
        let calendar_id = personal_calendar(user_id);
        let access = get_valid_user_token(state, audit, user_id).await?;
        let mut google_event_id = None;
        let mut html_link = None;
        let mut raw = None;
        let mut source = "local";
        if let Some(token) = access.filter(|t| !t.is_empty()) {
            let payload = insert_payload(&dto, &[]);
            match google::insert_event(state, &token, &payload, None).await {
                Ok(data) => {
                    google_event_id = Some(format!("{}:u{user_id}", js_template(data.get("id"))));
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
                user_id: Some(user_id),
                created_by: user_id,
                calendar_id,
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
        let message = if source == "google" {
            "Personal event created and synced to Google Calendar"
        } else {
            "Personal event created locally (Google Calendar not connected)"
        };
        Ok(Envelope::ok(message, CreatedEvent::personal(saved)))
    };
    run.await
        .unwrap_or_else(|e| fail_env(&e, "google-calendar-create-user-event"))
}

/// Id kalender + id event Google untuk event pribadi (`personal:<uid>` -> `primary`,
/// akhiran `:u<uid>` dibuang); selain itu diteruskan apa adanya.
pub(crate) fn google_delete_target<'a>(
    calendar_id: &'a str,
    event_id: &'a str,
    user_id: i32,
) -> (&'a str, &'a str) {
    if calendar_id.starts_with("personal:") {
        let suffix = format!(":u{user_id}");
        (
            "primary",
            event_id.strip_suffix(suffix.as_str()).unwrap_or(event_id),
        )
    } else {
        (calendar_id, event_id)
    }
}

/// `deleteUserEvent(userId, uuid)`.
pub async fn delete_user_event(
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
               WHERE uuid::text = $1 AND "userId" = $2 AND "companyId" = $3 LIMIT 1"#,
        )
        .bind(uuid)
        .bind(user_id)
        .bind(company_id)
        .fetch_optional(&state.db)
        .await?;
        let Some((id, google_id, calendar_id)) = event else {
            return Ok(Envelope::fail("Event not found"));
        };
        if let Some(gid) = google_id.filter(|g| !g.is_empty()) {
            let token = match get_valid_user_token(state, audit, user_id).await? {
                Some(t) if truthy(Some(&Value::String(t.clone()))) => t,
                _ => {
                    return Ok(Envelope::fail(
                        "Please reconnect your personal Google Calendar",
                    ));
                }
            };
            // Event pribadi: calendarId internal `personal:<id>` = kalender `primary` Google,
            // dan id ber-akhiran `:u<id>` dibuang (DIPERBAIKI 2026-11-17, Node ikut).
            let (g_cal, g_id) = google_delete_target(&calendar_id, &gid, user_id);
            match google::delete_event(state, &token, g_cal, g_id, None).await {
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
        .unwrap_or_else(|e| fail_env(&e, "google-calendar-delete-user-event"))
}

#[cfg(test)]
mod delete_target_tests {
    use super::google_delete_target;

    #[test]
    fn personal_maps_to_primary_and_strips_suffix() {
        assert_eq!(
            google_delete_target("personal:7", "abc:u7", 7),
            ("primary", "abc")
        );
        assert_eq!(
            google_delete_target("personal:7", "abc", 7),
            ("primary", "abc")
        );
        assert_eq!(
            google_delete_target("cal@x", "abc:u7", 7),
            ("cal@x", "abc:u7")
        );
    }
}
