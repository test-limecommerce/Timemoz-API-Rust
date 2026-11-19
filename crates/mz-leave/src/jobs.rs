//! `leave-schedule.constants.ts` + `LeaveJobsService` (`leave-jobs.service.ts`): pintu
//! enqueue antrean `leave-schedule` di atas `mz_core::queue` (kunci `mz:q:leave-schedule:*`,
//! terpisah dari Bull). Semua `enqueue_*` TIDAK pernah gagal (FR-026): galat dicatat lalu
//! ditelan; tick per jam menutup celahnya.
//!
//! Satu antrean untuk semua jenis job; `data` job = `{ "name": <nama job>, "data": <payload> }`
//! (Bull menyimpan nama job terpisah dari payload).

use std::time::Duration;

use chrono::NaiveDateTime;
use mz_core::AppState;
use mz_core::queue::JobOpts;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// `LEAVE_SCHEDULE_QUEUE`.
pub const LEAVE_SCHEDULE_QUEUE: &str = "leave-schedule";

/// `LEAVE_TICK_JOB` (di Rust dijalankan langsung oleh cron, lihat [`crate::cron_jobs`]).
pub const LEAVE_TICK_JOB: &str = "leave.tick";
/// `HOLIDAY_SYNC_JOB`.
pub const HOLIDAY_SYNC_JOB: &str = "leave.holiday-sync";
/// `POLICY_SWITCH_JOB`.
pub const POLICY_SWITCH_JOB: &str = "leave.policy-switch";
/// `CALENDAR_SYNC_JOB`.
pub const CALENDAR_SYNC_JOB: &str = "leave.calendar-sync";
/// `LEAVE_REMINDER_JOB`.
pub const LEAVE_REMINDER_JOB: &str = "leave.reminder";

/// `delay` job pengingat (48 jam).
const REMINDER_DELAY: Duration = Duration::from_secs(48 * 60 * 60);

/// `HolidaySyncPayload`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HolidaySyncPayload {
    pub company_id: i32,
    pub holiday_id: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_date: Option<String>,
}

/// `PolicySwitchPayload`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PolicySwitchPayload {
    pub company_id: i32,
}

/// `CalendarSyncPayload`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CalendarSyncPayload {
    pub leave_request_id: i32,
}

/// `LeaveReminderPayload`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LeaveReminderPayload {
    pub leave_request_id: i32,
    pub updated_at_ms: i64,
}

/// `YYYY-MM-DDTHH` UTC (`now.toISOString().slice(0, 13)`).
fn utc_hour(now: NaiveDateTime) -> String {
    now.format("%Y-%m-%dT%H").to_string()
}

/// `tickJobId(now)`.
pub fn tick_job_id(now: NaiveDateTime) -> String {
    format!("leave-tick:{}", utc_hour(now))
}

/// `holidaySyncJobId(holidayId, updatedAtMs)`.
pub fn holiday_sync_job_id(holiday_id: i32, updated_at_ms: i64) -> String {
    format!("holiday-sync:{holiday_id}:{updated_at_ms}")
}

/// `policySwitchJobId(companyId, updatedAtMs)`.
pub fn policy_switch_job_id(company_id: i32, updated_at_ms: i64) -> String {
    format!("policy-switch:{company_id}:{updated_at_ms}")
}

/// `calendarApprovedJobId(leaveRequestId)`.
pub fn calendar_approved_job_id(leave_request_id: i32) -> String {
    format!("leave-cal:{leave_request_id}:approved")
}

/// `calendarTickJobId(leaveRequestId, now)`.
pub fn calendar_tick_job_id(leave_request_id: i32, now: NaiveDateTime) -> String {
    format!("leave-cal:{leave_request_id}:tick:{}", utc_hour(now))
}

/// `leaveReminderJobId(leaveRequestId, updatedAtMs)`.
pub fn leave_reminder_job_id(leave_request_id: i32, updated_at_ms: i64) -> String {
    format!("leave-remind:{leave_request_id}:{updated_at_ms}")
}

/// Isi `data` job antrean: `{ name, data }`.
pub fn envelope(name: &str, payload: Value) -> Value {
    json!({ "name": name, "data": payload })
}

/// `enqueueHolidaySync(payload, updatedAtMs)` — attempts 3, backoff eksponensial 5 dtk.
/// Dipanggil SESUDAH commit oleh leave-holidays & `POST /admin/national-holidays/apply`.
pub async fn enqueue_holiday_sync(
    state: &AppState,
    payload: &HolidaySyncPayload,
    updated_at_ms: i64,
) {
    add(
        state,
        HOLIDAY_SYNC_JOB,
        serde_json::to_value(payload).unwrap_or(Value::Null),
        holiday_sync_job_id(payload.holiday_id, updated_at_ms),
        JobOpts::new(3, Duration::from_secs(5)),
        Duration::ZERO,
    )
    .await;
}

/// `enqueuePolicySwitch({ companyId }, updatedAtMs)` — attempts 3, backoff 5 dtk. Dipanggil
/// leave-policies sesudah commit bila mode berubah MONTHLY → UPFRONT.
pub async fn enqueue_policy_switch(state: &AppState, company_id: i32, updated_at_ms: i64) {
    add(
        state,
        POLICY_SWITCH_JOB,
        json!({ "companyId": company_id }),
        policy_switch_job_id(company_id, updated_at_ms),
        JobOpts::new(3, Duration::from_secs(5)),
        Duration::ZERO,
    )
    .await;
}

/// `enqueueCalendarSync(leaveRequestId, jobId)` — attempts 5, backoff 30 dtk. `job_id` =
/// [`calendar_approved_job_id`] (persetujuan) atau [`calendar_tick_job_id`] (tick).
pub async fn enqueue_calendar_sync(state: &AppState, leave_request_id: i32, job_id: String) {
    add(
        state,
        CALENDAR_SYNC_JOB,
        json!({ "leaveRequestId": leave_request_id }),
        job_id,
        JobOpts::new(5, Duration::from_secs(30)),
        Duration::ZERO,
    )
    .await;
}

/// `enqueueReminder(leaveRequestId, updatedAtMs)` — tertunda 48 jam, attempts 3, backoff
/// 60 dtk. `updated_at_ms` = `(updatedAt ?? createdAt).getTime()` pengajuan PENDING.
pub async fn enqueue_reminder(state: &AppState, leave_request_id: i32, updated_at_ms: i64) {
    add(
        state,
        LEAVE_REMINDER_JOB,
        json!({ "leaveRequestId": leave_request_id, "updatedAtMs": updated_at_ms }),
        leave_reminder_job_id(leave_request_id, updated_at_ms),
        JobOpts::new(3, Duration::from_secs(60)),
        REMINDER_DELAY,
    )
    .await;
}

/// `add(name, payload, opts)`: tidak pernah melempar.
async fn add(
    state: &AppState,
    name: &str,
    payload: Value,
    job_id: String,
    opts: JobOpts,
    delay: Duration,
) {
    let res = mz_core::queue::enqueue_delayed(
        state,
        LEAVE_SCHEDULE_QUEUE,
        Some(job_id.clone()),
        envelope(name, payload),
        opts,
        delay,
    )
    .await;
    if let Err(e) = res {
        tracing::error!(job = name, job_id = %job_id, error = %e, "Gagal enqueue {name} ({job_id})");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> NaiveDateTime {
        mz_core::time::parse_js_date(s).unwrap_or_default()
    }

    #[test]
    fn job_ids_match_node() {
        let now = at("2026-11-17T08:05:42.123Z");
        assert_eq!(tick_job_id(now), "leave-tick:2026-11-17T08");
        assert_eq!(
            holiday_sync_job_id(7, 1_700_000_000_123),
            "holiday-sync:7:1700000000123"
        );
        assert_eq!(policy_switch_job_id(2, 5), "policy-switch:2:5");
        assert_eq!(calendar_approved_job_id(9), "leave-cal:9:approved");
        assert_eq!(
            calendar_tick_job_id(9, now),
            "leave-cal:9:tick:2026-11-17T08"
        );
        assert_eq!(leave_reminder_job_id(9, 11), "leave-remind:9:11");
    }

    #[test]
    fn payload_shapes() {
        let p = HolidaySyncPayload {
            company_id: 1,
            holiday_id: 2,
            previous_date: None,
            current_date: Some("2026-10-01".into()),
        };
        assert_eq!(
            serde_json::to_string(&envelope(
                HOLIDAY_SYNC_JOB,
                serde_json::to_value(&p).unwrap_or_default()
            ))
            .unwrap_or_default(),
            r#"{"name":"leave.holiday-sync","data":{"companyId":1,"holidayId":2,"currentDate":"2026-10-01"}}"#
        );
    }
}
