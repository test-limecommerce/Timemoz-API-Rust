//! `LeaveCalendarSyncService` (`integrations/google-calendar/leave-calendar-sync.service.ts`):
//! menulis cuti yang disetujui ke Google Calendar perusahaan sebagai event minimal (tanpa
//! jenis cuti, alasan, lokasi, atau peserta). Idempoten menurut baris `googleCalendarEvent`,
//! jadi aman di-retry antrean maupun tick per jam.
//!
//! Token company lewat `mz_int_gcal::company::get_valid_token` (refresh OAuth bila perlu);
//! URL Google lewat `mz_int_common::api_base::GOOGLE_APIS` (bisa dialihkan saat uji).

use chrono::{Duration, NaiveDate, NaiveDateTime};
use mz_audit::AuditHandle;
use mz_core::AppState;
use mz_int_common::api_base::GOOGLE_APIS;
use mz_int_gcal::util::encode_uri_component;
use mz_overtime::zone::{ndt, wall_to_ms};
use serde_json::{Value, json};
use sqlx::types::Json;

use crate::day_count::to_date_key;
use crate::enums::{LeaveDayType, LeaveStatus};
use crate::jobs::{calendar_tick_job_id, enqueue_calendar_sync};
use crate::policy::{local_now, zone};

const CALENDAR_ID: &str = "primary";
const REENQUEUE_BATCH: i64 = 200;

/// Galat HTTP Google (bentuk minimal `AxiosError`).
#[derive(Debug)]
struct HttpFail {
    status: Option<u16>,
    message: String,
}

impl std::fmt::Display for HttpFail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for HttpFail {}

fn calendar_api(state: &AppState) -> String {
    format!("{}/calendar/v3", GOOGLE_APIS.get(state))
}

/// `axios.<method>(url, body, { headers: { Authorization } })` tanpa timeout axios (klien
/// bersama memasang timeout bawaan).
async fn call(
    state: &AppState,
    method: reqwest::Method,
    url: &str,
    token: &str,
    body: Option<&Value>,
) -> Result<Value, HttpFail> {
    let mut req = state.http.request(method, url).bearer_auth(token);
    if let Some(b) = body {
        req = req.json(b);
    }
    let res = req.send().await.map_err(|e| HttpFail {
        status: None,
        message: if e.is_timeout() {
            "timeout exceeded".to_owned()
        } else {
            "Network Error".to_owned()
        },
    })?;
    let status = res.status();
    let text = res.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(HttpFail {
            status: Some(status.as_u16()),
            message: format!("Request failed with status code {}", status.as_u16()),
        });
    }
    Ok(serde_json::from_str(&text).unwrap_or(Value::String(text)))
}

async fn valid_token(state: &AppState, company_id: i32) -> anyhow::Result<Option<String>> {
    // Antrean tidak punya konteks request: tulisan token (model `company`) dicatat `system`.
    let audit = AuditHandle::detached(state.db.clone());
    mz_int_gcal::company::get_valid_token(state, &audit, company_id)
        .await
        .map_err(|f| anyhow::anyhow!(f.message()))
}

/// Event yang akan dibuat untuk satu pengajuan (murni).
#[derive(Debug, Clone, PartialEq)]
pub struct LeaveEventPlan {
    pub summary: String,
    pub payload: Value,
    pub start_at: NaiveDateTime,
    pub end_at: NaiveDateTime,
    pub is_all_day: bool,
}

/// Payload `POST events` + `startAt`/`endAt` baris lokal. Setengah hari tunggal → event
/// berjam (pagi 09-13, sore 13-17, zona company); selain itu event seharian `date`
/// eksklusif (akhir + 1 hari).
pub fn plan_leave_event(
    uuid: &str,
    employee_name: &str,
    start: NaiveDate,
    end: NaiveDate,
    start_day_type: LeaveDayType,
    time_zone: &str,
) -> LeaveEventPlan {
    let start_key = to_date_key(start);
    let single_day_half = start == end && start_day_type != LeaveDayType::FULL_DAY;
    let morning = start_day_type == LeaveDayType::HALF_DAY_MORNING;
    let suffix = match (single_day_half, morning) {
        (true, true) => " (½ hari pagi)",
        (true, false) => " (½ hari sore)",
        _ => "",
    };
    let summary = format!("Cuti — {employee_name}{suffix}");
    let (start_v, end_v, start_at, end_at) = if single_day_half {
        let (from, to) = if morning { (9, 13) } else { (13, 17) };
        let tz = zone(time_zone);
        let wall = |h: u32| start.and_hms_opt(h, 0, 0).unwrap_or_default();
        (
            json!({ "dateTime": format!("{start_key}T{from:02}:00:00"), "timeZone": time_zone }),
            json!({ "dateTime": format!("{start_key}T{to:02}:00:00"), "timeZone": time_zone }),
            ndt(wall_to_ms(wall(from), tz)),
            ndt(wall_to_ms(wall(to), tz)),
        )
    } else {
        let exclusive_end = end + Duration::days(1);
        (
            json!({ "date": start_key }),
            json!({ "date": to_date_key(exclusive_end) }),
            start.and_hms_opt(0, 0, 0).unwrap_or_default(),
            exclusive_end.and_hms_opt(0, 0, 0).unwrap_or_default(),
        )
    };
    let payload = json!({
        "summary": summary,
        "start": start_v,
        "end": end_v,
        "transparency": "opaque",
        "extendedProperties": { "private": { "mudahLeaveRequestUuid": uuid } },
    });
    LeaveEventPlan {
        summary,
        payload,
        start_at,
        end_at,
        is_all_day: !single_day_half,
    }
}

#[derive(sqlx::FromRow)]
#[sqlx(rename_all = "camelCase")]
struct SyncRow {
    uuid: String,
    company_id: i32,
    status: LeaveStatus,
    start_date: NaiveDate,
    end_date: NaiveDate,
    start_day_type: LeaveDayType,
    reviewed_by_id: Option<i32>,
    user_name: String,
    time_zone: String,
    google_event_id: Option<String>,
}

/// `ensureLeaveEvent(leaveRequestId)`. Bukan APPROVED → hapus event. Galat Google selain
/// 401/403 dikembalikan (antrean mencoba ulang).
pub async fn ensure_leave_event(state: &AppState, leave_request_id: i32) -> anyhow::Result<()> {
    let row: Option<SyncRow> = sqlx::query_as(
        r#"SELECT r.uuid::text AS uuid, r."companyId", r.status::text AS status, r."startDate",
                  r."endDate", r."startDayType"::text AS "startDayType", r."reviewedById",
                  u.name AS "userName", c."timeZone", g."googleEventId"
           FROM "leaveRequest" r
           JOIN users u ON u.id = r."userId"
           JOIN company c ON c.id = r."companyId"
           LEFT JOIN "googleCalendarEvent" g ON g."leaveRequestId" = r.id
           WHERE r.id = $1"#,
    )
    .bind(leave_request_id)
    .fetch_optional(&state.db)
    .await?;
    let Some(r) = row else { return Ok(()) };
    if r.status != LeaveStatus::APPROVED {
        return remove_leave_event(state, leave_request_id).await;
    }
    if r.google_event_id.as_deref().is_some_and(|g| !g.is_empty()) {
        return Ok(());
    }
    let Some(token) = valid_token(state, r.company_id).await? else {
        return Ok(());
    };

    let plan = plan_leave_event(
        &r.uuid,
        &r.user_name,
        r.start_date,
        r.end_date,
        r.start_day_type,
        &r.time_zone,
    );
    let url = format!(
        "{}/calendars/{CALENDAR_ID}/events?sendUpdates=none",
        calendar_api(state)
    );
    let data = match call(
        state,
        reqwest::Method::POST,
        &url,
        &token,
        Some(&plan.payload),
    )
    .await
    {
        Ok(d) => d,
        Err(e) if matches!(e.status, Some(401 | 403)) => {
            tracing::warn!(
                "Google menolak ({}) saat membuat event cuti {leave_request_id}; dicoba lagi oleh tick",
                e.status.unwrap_or_default()
            );
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };
    let Some(event_id) = data
        .get("id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return Ok(());
    };
    let html_link = data.get("htmlLink").and_then(Value::as_str);

    sqlx::query(
        r#"INSERT INTO "googleCalendarEvent"
             ("companyId", "userId", "createdByUserId", title, source, "googleEventId",
              "calendarId", "startAt", "endAt", "isAllDay", status, "htmlLink", "rawMeta",
              "leaveRequestId")
           VALUES ($1, NULL, $2, $3, 'google', $4, $5, $6, $7, $8, 'confirmed', $9, $10, $11)
           ON CONFLICT ("leaveRequestId") DO UPDATE SET
             "companyId" = EXCLUDED."companyId", "userId" = NULL,
             "createdByUserId" = EXCLUDED."createdByUserId", title = EXCLUDED.title,
             source = EXCLUDED.source, "googleEventId" = EXCLUDED."googleEventId",
             "calendarId" = EXCLUDED."calendarId", "startAt" = EXCLUDED."startAt",
             "endAt" = EXCLUDED."endAt", "isAllDay" = EXCLUDED."isAllDay",
             status = EXCLUDED.status, "htmlLink" = EXCLUDED."htmlLink",
             "rawMeta" = EXCLUDED."rawMeta""#,
    )
    .bind(r.company_id)
    .bind(r.reviewed_by_id)
    .bind(&plan.summary)
    .bind(event_id)
    .bind(CALENDAR_ID)
    .bind(plan.start_at)
    .bind(plan.end_at)
    .bind(plan.is_all_day)
    .bind(html_link)
    .bind(Json(&data))
    .bind(leave_request_id)
    .execute(&state.db)
    .await?;
    Ok(())
}

/// `removeLeaveEvent(leaveRequestId)`: hapus event Google (404/410 dianggap sudah hilang;
/// 401/403 → berhenti tanpa menghapus baris) lalu hapus baris `googleCalendarEvent`.
pub async fn remove_leave_event(state: &AppState, leave_request_id: i32) -> anyhow::Result<()> {
    let row: Option<(i32, i32, Option<String>, String)> = sqlx::query_as(
        r#"SELECT id, "companyId", "googleEventId", "calendarId" FROM "googleCalendarEvent"
           WHERE "leaveRequestId" = $1"#,
    )
    .bind(leave_request_id)
    .fetch_optional(&state.db)
    .await?;
    let Some((id, company_id, google_event_id, calendar_id)) = row else {
        return Ok(());
    };
    if let Some(event_id) = google_event_id.filter(|g| !g.is_empty())
        && let Some(token) = valid_token(state, company_id).await?
    {
        let url = format!(
            "{}/calendars/{}/events/{}?sendUpdates=none",
            calendar_api(state),
            encode_uri_component(&calendar_id),
            encode_uri_component(&event_id)
        );
        match call(state, reqwest::Method::DELETE, &url, &token, None).await {
            Ok(_) => {}
            Err(e) if matches!(e.status, Some(401 | 403)) => {
                tracing::warn!(
                    "Google menolak ({}) saat menghapus event cuti {leave_request_id}",
                    e.status.unwrap_or_default()
                );
                return Ok(());
            }
            Err(e) if matches!(e.status, Some(404 | 410)) => {}
            Err(e) => return Err(e.into()),
        }
    }
    sqlx::query(r#"DELETE FROM "googleCalendarEvent" WHERE id = $1"#)
        .bind(id)
        .execute(&state.db)
        .await?;
    Ok(())
}

/// `reenqueueMissing(companyId, timeZone, now)`: cuti APPROVED yang belum berakhir dan belum
/// punya baris event (maks 200) di-enqueue ulang — hanya bila company terhubung Google.
pub async fn reenqueue_missing(
    state: &AppState,
    company_id: i32,
    time_zone: &str,
    now: NaiveDateTime,
) -> anyhow::Result<()> {
    let today = local_now(now, time_zone).date();
    let missing: Vec<i32> = sqlx::query_scalar(
        r#"SELECT r.id FROM "leaveRequest" r
           WHERE r."companyId" = $1 AND r.status = 'APPROVED' AND r."endDate" >= $2
             AND NOT EXISTS (SELECT 1 FROM "googleCalendarEvent" g WHERE g."leaveRequestId" = r.id)
           ORDER BY r.id
           LIMIT $3"#,
    )
    .bind(company_id)
    .bind(today)
    .bind(REENQUEUE_BATCH)
    .fetch_all(&state.db)
    .await?;
    if missing.is_empty() {
        return Ok(());
    }
    // Cek token (bisa memicu refresh OAuth) hanya bila memang ada yang perlu disinkron.
    if valid_token(state, company_id).await?.is_none() {
        return Ok(());
    }
    for id in missing {
        enqueue_calendar_sync(state, id, calendar_tick_job_id(id, now)).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        crate::day_count::parse_date_key(s).unwrap_or_default()
    }

    fn at(s: &str) -> NaiveDateTime {
        mz_core::time::parse_js_date(s).unwrap_or_default()
    }

    #[test]
    fn full_day_range_is_all_day_exclusive() {
        let p = plan_leave_event(
            "u-1",
            "Budi",
            d("2026-10-05"),
            d("2026-10-07"),
            LeaveDayType::FULL_DAY,
            "Asia/Jakarta",
        );
        assert_eq!(p.summary, "Cuti — Budi");
        assert_eq!(
            serde_json::to_string(&p.payload).unwrap_or_default(),
            r#"{"summary":"Cuti — Budi","start":{"date":"2026-10-05"},"end":{"date":"2026-10-08"},"transparency":"opaque","extendedProperties":{"private":{"mudahLeaveRequestUuid":"u-1"}}}"#
        );
        assert_eq!(
            (p.start_at, p.end_at, p.is_all_day),
            (at("2026-10-05T00:00:00Z"), at("2026-10-08T00:00:00Z"), true)
        );
        // Multi-hari setengah hari tetap seharian.
        let p = plan_leave_event(
            "u",
            "B",
            d("2026-10-05"),
            d("2026-10-06"),
            LeaveDayType::HALF_DAY_MORNING,
            "UTC",
        );
        assert!(p.is_all_day);
        assert_eq!(p.summary, "Cuti — B");
    }

    #[test]
    fn single_half_day_is_timed_in_company_zone() {
        let p = plan_leave_event(
            "u",
            "Sari",
            d("2026-10-05"),
            d("2026-10-05"),
            LeaveDayType::HALF_DAY_AFTERNOON,
            "Asia/Jakarta",
        );
        assert_eq!(p.summary, "Cuti — Sari (½ hari sore)");
        assert_eq!(
            p.payload.get("start").cloned().unwrap_or_default(),
            json!({"dateTime": "2026-10-05T13:00:00", "timeZone": "Asia/Jakarta"})
        );
        assert_eq!(
            p.payload.get("end").cloned().unwrap_or_default(),
            json!({"dateTime": "2026-10-05T17:00:00", "timeZone": "Asia/Jakarta"})
        );
        assert_eq!(
            (p.start_at, p.end_at, p.is_all_day),
            (
                at("2026-10-05T06:00:00Z"),
                at("2026-10-05T10:00:00Z"),
                false
            )
        );
        let p = plan_leave_event(
            "u",
            "Sari",
            d("2026-10-05"),
            d("2026-10-05"),
            LeaveDayType::HALF_DAY_MORNING,
            "UTC",
        );
        assert_eq!(p.summary, "Cuti — Sari (½ hari pagi)");
        assert_eq!(p.start_at, at("2026-10-05T09:00:00Z"));
    }
}
