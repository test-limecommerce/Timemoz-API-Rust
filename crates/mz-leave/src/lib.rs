//! Pustaka domain cuti (tahap 1, tanpa route). Port `src/leave-schedule/**` lengkap
//! (akrual, tahun cuti, potongan libur, peralihan kebijakan, enqueue + worker antrean
//! `leave-schedule`, cron tick) dan fungsi bersama `src/leaves/**` /
//! `leave-policies.service.ts` yang dipakai modul cuti lain. Ringkasan API:
//! `plan/leave-mz-leave-api.md`; deviasi: `plan/deviations/mz-leave.md`.
//!
//! Pasang di `mz-server`: `mz_leave::cron_jobs()` ke registry cron dan
//! `mz_leave::spawn_worker(&state)` di samping worker antrean lain.

pub mod balance;
pub mod calendar;
pub mod calendar_sync;
pub mod carry_over;
pub mod day_count;
pub mod enums;
pub mod holiday_deduction;
pub mod jobs;
pub mod js;
pub mod mail;
pub mod math;
pub mod models;
pub mod policy;
pub mod schedule;
pub mod segments;
pub mod view;
pub mod window;
pub mod worker;

use chrono::{NaiveDate, NaiveDateTime};
use mz_core::AppState;
use mz_jobs::{CronJob, JobFuture, Schedule};
use sqlx::PgConnection;

use crate::enums::LeaveDayType;

pub use worker::spawn_worker;

/// Nama cron tick cuti (env `MZ_JOBS`, kunci dedup slot `mz:job:leave-tick:<slot>`).
pub const TICK_JOB_NAME: &str = "leave-tick";

fn tick(state: AppState) -> JobFuture {
    Box::pin(async move { worker::handle_tick(&state).await })
}

/// Cron crate ini.
///
/// `leave-tick` — Node: `leave-schedule.cron.ts:15` `@Cron('0 5 * * * *')` (menit 5 tiap
/// jam) → `enqueueTick` (jobId `leave-tick:<UTC jam>`, timeout 15 menit) →
/// `leave-schedule.processor.ts:55` `handleTick`. Di Rust dedup antar replika lewat slot
/// `mz_jobs` dan tick dijalankan langsung (tanpa antrean).
pub fn cron_jobs() -> Vec<CronJob> {
    vec![CronJob {
        name: TICK_JOB_NAME,
        schedule: Schedule::hourly(&[5]),
        timeout: worker::TICK_TIMEOUT,
        run: tick,
    }]
}

/// `DOCUMENT_MIME_BY_EXT` (`leaves.service.ts:77`): MIME dokumen pendukung dari ekstensi
/// key (huruf kecil); tak dikenal → `None` (pemanggil memakai `application/octet-stream`).
pub fn document_mime(ext: &str) -> Option<&'static str> {
    match ext {
        "jpg" | "jpeg" => Some("image/jpeg"),
        "png" => Some("image/png"),
        "webp" => Some("image/webp"),
        "gif" => Some("image/gif"),
        "pdf" => Some("application/pdf"),
        _ => None,
    }
}

/// Hook lembur saat cuti disetujui (`leaves.service.ts:454`): `cancelForLeave(tx, {
/// companyId, userId, fullDayDates: fullDayDatesOfLeave(...) })`. Panggil di DALAM transaksi
/// persetujuan, sesudah klaim `leaveRequest` dan SEBELUM `lock_balance`.
#[allow(clippy::too_many_arguments)]
pub async fn cancel_overtime_for_leave(
    conn: &mut PgConnection,
    company_id: i32,
    user_id: i32,
    start: NaiveDate,
    end: NaiveDate,
    start_day_type: LeaveDayType,
    end_day_type: LeaveDayType,
    now: NaiveDateTime,
) -> sqlx::Result<u64> {
    let leave = mz_overtime::calendar::Leave {
        start_date: start,
        end_date: end,
        start_day_type: start_day_type.as_str().to_owned(),
        end_day_type: end_day_type.as_str().to_owned(),
    };
    let dates = mz_overtime::full_day_dates_of_leave(&leave);
    mz_overtime::cancel_for_leave(conn, company_id, user_id, &dates, now).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::time::Duration;

    #[test]
    fn tick_schedule_minute_5_every_hour() {
        let jobs = cron_jobs();
        let Some(job) = jobs.first() else {
            return;
        };
        assert_eq!(job.name, "leave-tick");
        assert_eq!(job.timeout, Duration::from_secs(900));
        let at = |h, m| chrono::Utc.with_ymd_and_hms(2026, 10, 22, h, m, 0).single();
        assert_eq!(
            job.schedule.next_after(at(7, 45).unwrap_or_default()),
            at(8, 5)
        );
        assert_eq!(
            job.schedule.next_after(at(8, 5).unwrap_or_default()),
            at(9, 5)
        );
    }

    #[test]
    fn document_mimes() {
        assert_eq!(document_mime("jpeg"), Some("image/jpeg"));
        assert_eq!(document_mime("pdf"), Some("application/pdf"));
        assert_eq!(document_mime("PDF"), None);
        assert_eq!(document_mime("exe"), None);
    }
}
