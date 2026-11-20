//! `LeaveScheduleProcessor` (`leave-schedule/leave-schedule.processor.ts`) + cron
//! `LeaveScheduleCron` (`leave-schedule.cron.ts`).
//!
//! Payload divalidasi sebelum dipakai; payload tak valid dicatat lalu dibuang (tidak
//! di-retry). Langkah per company dibungkus agar satu company gagal tidak menggugurkan yang
//! lain. Galat lain → retry dengan backoff job.

use std::time::Duration;

use chrono::{NaiveDate, NaiveDateTime};
use mz_core::AppState;
use mz_core::queue::{Job, Outcome, WorkerConfig};
use mz_core::time::now_naive;
use mz_overtime::zone::ms_of;
use serde_json::Value;

use crate::calendar_sync::{ensure_leave_event, reenqueue_missing};
use crate::day_count::{parse_date_key, to_date_key};
use crate::enums::LeaveStatus;
use crate::holiday_deduction::{apply_due, recalc_approved_for_dates, reconcile_holiday};
use crate::jobs::{
    CALENDAR_SYNC_JOB, HOLIDAY_SYNC_JOB, LEAVE_REMINDER_JOB, LEAVE_SCHEDULE_QUEUE,
    POLICY_SWITCH_JOB,
};
use crate::mail::{LeaveMailDetails, send_leave_request_to_approver};
use crate::policy::company_time_zone;
use crate::schedule::{
    accrual_catch_up, accrual_catch_up_previous_year, ensure_year_open, expire_carry_over,
    top_up_to_upfront,
};

/// `TICK_TIMEOUT_MS` (15 menit).
pub const TICK_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// Batas satu job antrean (bukan tick). Bull Node tanpa timeout; dibatasi di sini supaya
/// lease tidak pernah habis saat job masih jalan.
pub const JOB_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const JOB_LEASE: Duration = Duration::from_secs(11 * 60);
/// Empat `@Process({ concurrency: 1 })` pada satu antrean Bull = hingga 4 job bersamaan.
const CONCURRENCY: usize = 4;

/// `isPositiveInt(v)`: angka bulat > 0 (JSON number, bukan string).
fn positive_int(v: Option<&Value>) -> Option<i32> {
    let n = v?.as_f64()?;
    if n.fract() != 0.0 || n <= 0.0 || n > f64::from(i32::MAX) {
        return None;
    }
    #[allow(clippy::cast_possible_truncation)]
    Some(n as i32)
}

/// `undefined` → `Ok(None)`; string tanggal ketat → `Ok(Some)`; selain itu `Err`.
fn date_key_field(v: Option<&Value>) -> Result<Option<NaiveDate>, ()> {
    match v {
        None => Ok(None),
        Some(Value::String(s)) => parse_date_key(s).map(Some).ok_or(()),
        Some(_) => Err(()),
    }
}

fn outcome(res: anyhow::Result<()>) -> Outcome {
    match res {
        Ok(()) => Outcome::Done,
        Err(e) => Outcome::Retry(e.to_string()),
    }
}

/// Satu job antrean (`{ name, data }`).
pub async fn process(state: AppState, job: Job) -> Outcome {
    let name = job
        .data
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let data = job.data.get("data").cloned().unwrap_or(Value::Null);
    match name {
        HOLIDAY_SYNC_JOB => handle_holiday_sync(&state, &data).await,
        POLICY_SWITCH_JOB => handle_policy_switch(&state, &data).await,
        CALENDAR_SYNC_JOB => handle_calendar_sync(&state, &data).await,
        LEAVE_REMINDER_JOB => handle_reminder(&state, &data).await,
        other => {
            tracing::warn!(job = %job.id, "leave-schedule: job tak dikenal '{other}', dibuang");
            Outcome::Drop(format!("unknown job {other}"))
        }
    }
}

/// `handleHolidaySync(job)`: rekonsiliasi potongan libur + hitung ulang cuti tersetujui
/// pada tanggal lama/baru.
pub async fn handle_holiday_sync(state: &AppState, p: &Value) -> Outcome {
    let company_id = positive_int(p.get("companyId"));
    let holiday_id = positive_int(p.get("holidayId"));
    let previous = date_key_field(p.get("previousDate"));
    let current = date_key_field(p.get("currentDate"));
    let (Some(company_id), Some(holiday_id), Ok(previous), Ok(current)) =
        (company_id, holiday_id, previous, current)
    else {
        tracing::warn!("{HOLIDAY_SYNC_JOB}: payload tidak valid, dibuang");
        return Outcome::Drop("payload tidak valid".into());
    };
    outcome(
        async {
            // timeZone tersimpan dipakai apa adanya ('UTC' sah; tanpa fallback Asia/Jakarta).
            let Some(tz) = company_time_zone(&state.db, company_id).await? else {
                return Ok(());
            };
            let now = now_naive();
            reconcile_holiday(&state.db, company_id, Some(holiday_id), &tz, now).await?;
            let dates: Vec<NaiveDate> = [previous, current].into_iter().flatten().collect();
            recalc_approved_for_dates(&state.db, company_id, &dates, &tz).await?;
            Ok(())
        }
        .await,
    )
}

/// `handlePolicySwitch(job)`.
pub async fn handle_policy_switch(state: &AppState, p: &Value) -> Outcome {
    let Some(company_id) = positive_int(p.get("companyId")) else {
        tracing::warn!("{POLICY_SWITCH_JOB}: payload tidak valid, dibuang");
        return Outcome::Drop("payload tidak valid".into());
    };
    match top_up_to_upfront(&state.db, company_id, now_naive()).await {
        Ok(n) => {
            tracing::info!("{POLICY_SWITCH_JOB}: {n} saldo dinaikkan (company {company_id})");
            Outcome::Done
        }
        Err(e) => Outcome::Retry(e.to_string()),
    }
}

/// `handleCalendarSync(job)`.
pub async fn handle_calendar_sync(state: &AppState, p: &Value) -> Outcome {
    let Some(id) = positive_int(p.get("leaveRequestId")) else {
        tracing::warn!("{CALENDAR_SYNC_JOB}: payload tidak valid, dibuang");
        return Outcome::Drop("payload tidak valid".into());
    };
    outcome(ensure_leave_event(state, id).await)
}

#[derive(sqlx::FromRow)]
#[sqlx(rename_all = "camelCase")]
struct ReminderRow {
    status: LeaveStatus,
    created_at: NaiveDateTime,
    updated_at: Option<NaiveDateTime>,
    start_date: NaiveDate,
    end_date: NaiveDate,
    start_day_type: String,
    end_day_type: String,
    total_days: f64,
    reason: String,
    leave_type: Option<String>,
    type_name: Option<String>,
    user_name: String,
    approver_name: Option<String>,
    approver_username: Option<String>,
}

/// `handleReminder(job)`: pengingat 48 jam ke approver bila pengajuan masih PENDING dan
/// belum berubah sejak dijadwalkan (`updatedAt`/`createdAt` = `updatedAtMs`).
pub async fn handle_reminder(state: &AppState, p: &Value) -> Outcome {
    let Some(id) = positive_int(p.get("leaveRequestId")) else {
        return Outcome::Drop("payload tidak valid".into());
    };
    let updated_at_ms = p.get("updatedAtMs").and_then(Value::as_f64);
    let row: Result<Option<ReminderRow>, sqlx::Error> = sqlx::query_as(
        r#"SELECT r.status::text AS status, r."createdAt", r."updatedAt", r."startDate",
                  r."endDate", r."startDayType"::text AS "startDayType",
                  r."endDayType"::text AS "endDayType", r."totalDays", r.reason,
                  r."leaveType"::text AS "leaveType", t.name AS "typeName",
                  u.name AS "userName", a.name AS "approverName", a.username AS "approverUsername"
           FROM "leaveRequest" r
           JOIN users u ON u.id = r."userId"
           LEFT JOIN users a ON a.id = r."approverId"
           LEFT JOIN "companyLeaveType" t ON t.id = r."leaveTypeId"
           WHERE r.id = $1"#,
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await;
    let r = match row {
        Ok(Some(r)) => r,
        Ok(None) => return Outcome::Done,
        Err(e) => return Outcome::Retry(e.to_string()),
    };
    if r.status != LeaveStatus::PENDING {
        return Outcome::Done;
    }
    let same = |t: Option<NaiveDateTime>| {
        t.is_some_and(|t| updated_at_ms.is_some_and(|ms| ms == ms_of(t) as f64))
    };
    if !same(r.updated_at) && !same(Some(r.created_at)) {
        return Outcome::Done;
    }
    let (Some(email), Some(name)) = (
        r.approver_username.filter(|u| !u.is_empty()),
        r.approver_name,
    ) else {
        return Outcome::Done;
    };
    let details = LeaveMailDetails {
        employee_name: r.user_name,
        leave_type_name: r
            .type_name
            .or(r.leave_type)
            .unwrap_or_else(|| "Cuti".to_owned()),
        start_date: to_date_key(r.start_date),
        end_date: to_date_key(r.end_date),
        start_day_type: r.start_day_type,
        end_day_type: r.end_day_type,
        total_days: r.total_days,
        reason: r.reason,
    };
    send_leave_request_to_approver(state, &details, &email, &name, true).await;
    Outcome::Done
}

/// `handleTick()`: semua company yang punya `leavePolicy`, berurutan.
pub async fn handle_tick(state: &AppState) -> anyhow::Result<()> {
    handle_tick_at(state, now_naive()).await
}

/// [`handle_tick`] dengan "sekarang" eksplisit (uji).
pub async fn handle_tick_at(state: &AppState, now: NaiveDateTime) -> anyhow::Result<()> {
    let companies: Vec<(i32, String)> = sqlx::query_as(
        r#"SELECT c.id, c."timeZone" FROM company c
           WHERE EXISTS (SELECT 1 FROM "leavePolicy" p WHERE p."companyId" = c.id)
           ORDER BY c.id"#,
    )
    .fetch_all(&state.db)
    .await?;
    for (id, tz) in companies {
        if let Err(e) = tick_company(state, id, &tz, now).await {
            tracing::error!("leave.tick gagal untuk company {id}: {e}");
        }
    }
    Ok(())
}

/// `tickCompany`: urutan langkah tetap.
pub async fn tick_company(
    state: &AppState,
    company_id: i32,
    time_zone: &str,
    now: NaiveDateTime,
) -> anyhow::Result<()> {
    // Akrual Y-1 (Desember, bulan yang terlewat) lebih dulu: saldo Y yang dibuka sesudahnya
    // langsung menghitung bawaan dari Y-1 yang lengkap. Saldo Y yang sudah ada dikoreksi oleh
    // recompute_next_year_carry_over di dalam transaksi kreditnya.
    accrual_catch_up_previous_year(&state.db, company_id, time_zone, now).await?;
    ensure_year_open(&state.db, company_id, time_zone, now).await?;
    // Bagian Y-1 di sini hanya satu query kosong; yang dikreditkan adalah tahun berjalan.
    accrual_catch_up(&state.db, company_id, time_zone, now).await?;
    expire_carry_over(&state.db, company_id, time_zone, now).await?;
    apply_due(&state.db, company_id, time_zone, now).await?;
    reenqueue_missing(state, company_id, time_zone, now).await?;
    tracing::debug!("leave.tick company {company_id} ({time_zone})");
    Ok(())
}

/// Pasang worker antrean `leave-schedule` (proses API; Node: `@Processor` di setiap replika).
pub fn spawn_worker(state: &AppState) -> tokio::task::JoinHandle<()> {
    let cfg = WorkerConfig {
        concurrency: CONCURRENCY,
        lease: JOB_LEASE,
        handler_timeout: JOB_TIMEOUT,
        poll: mz_core::queue::DEFAULT_POLL,
    };
    let st = state.clone();
    mz_core::queue::spawn_worker_with(state, LEAVE_SCHEDULE_QUEUE, cfg, move |job: Job| {
        process(st.clone(), job)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn payload_validation() {
        assert_eq!(positive_int(Some(&json!(3))), Some(3));
        assert_eq!(positive_int(Some(&json!(3.0))), Some(3));
        assert_eq!(positive_int(Some(&json!(0))), None);
        assert_eq!(positive_int(Some(&json!(-1))), None);
        assert_eq!(positive_int(Some(&json!(1.5))), None);
        assert_eq!(positive_int(Some(&json!("3"))), None);
        assert_eq!(positive_int(None), None);
        assert_eq!(date_key_field(None), Ok(None));
        assert!(date_key_field(Some(&json!("2026-10-01"))).is_ok_and(|d| d.is_some()));
        assert_eq!(date_key_field(Some(&json!("2026-1-01"))), Err(()));
        assert_eq!(date_key_field(Some(&json!(null))), Err(()));
    }
}
