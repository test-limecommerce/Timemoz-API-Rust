//! Lembur PP 35 — bagian latar (Fase 4a): tick per jam (`overtime.cron.ts` +
//! `overtime.processor.ts`), recalc per tanggal, dan nudge dari jalur request
//! (`overtime-jobs.service.ts`, `tracking-timelines/overtime-nudge-dates.ts`).
//!
//! 22 route HR lembur (`/v1/overtime/*`): modul `hr` (`mz_overtime::router`).
//! Lihat `plan/deviations/mz-overtime.md`.

pub mod calendar;
pub mod compute;
pub mod hr;
pub mod leave_hook;
pub mod locks;
pub mod math;
pub mod nudge;
pub mod prune;
pub mod tick;
pub mod worked;
pub mod zone;

use std::time::Duration;

use mz_jobs::{CronJob, Schedule};

pub use hr::router;
pub use leave_hook::{cancel_for_leave, full_day_dates_of_leave};
pub use nudge::{NudgeInterval, nudge, nudge_for_timelines, recalc_enabled};

/// Nama job tick (dipakai `MZ_JOBS` dan kunci dedup slot).
pub const TICK_JOB_NAME: &str = "overtime-tick";
/// `OVERTIME_TICK_TIMEOUT_MS`.
pub const OVERTIME_TICK_TIMEOUT: Duration = Duration::from_millis(600_000);

/// Job cron crate ini.
///
/// `overtime-tick` — Node: src/overtime/overtime.cron.ts:15 (`@Cron('0 10 * * * *')`: detik 0,
/// menit 10, tiap jam) → `enqueueTick` → src/overtime/overtime.processor.ts:205 `handleTick`.
pub fn cron_jobs() -> Vec<CronJob> {
    vec![CronJob {
        name: TICK_JOB_NAME,
        schedule: Schedule::hourly(&[10]),
        timeout: OVERTIME_TICK_TIMEOUT,
        run: |s| Box::pin(tick::handle_tick(s)),
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn tick_schedule_minute_10_every_hour() {
        let jobs = cron_jobs();
        let Some(job) = jobs.first() else {
            return;
        };
        assert_eq!(job.name, "overtime-tick");
        assert_eq!(job.timeout, Duration::from_secs(600));
        let at = |h, m| chrono::Utc.with_ymd_and_hms(2026, 10, 22, h, m, 0).single();
        assert_eq!(
            job.schedule.next_after(at(7, 45).unwrap_or_default()),
            at(8, 10)
        );
        assert_eq!(
            job.schedule.next_after(at(8, 10).unwrap_or_default()),
            at(9, 10)
        );
    }
}
