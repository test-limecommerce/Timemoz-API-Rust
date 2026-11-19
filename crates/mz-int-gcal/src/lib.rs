//! Port `src/integrations/google-calendar/*` (kecuali `leave-calendar-sync.service.ts`,
//! ditunda sampai modul cuti di-port). Lihat `plan/deviations/mz-int-gcal.md`.
//!
//! - [`router`]: 10 route `google-calendar/*` (OAuth company), 7 route
//!   `google-calendar/user/*` (OAuth per user), `POST google-calendar/webhook`.
//! - [`cron_jobs`]: `scheduled-sync-all-companies` dan `scheduled-renew-webhook-channels`.

pub mod company;
pub mod db;
pub mod dto;
pub mod google;
pub mod mail;
mod routes;
pub mod user;
pub mod util;

use std::time::Duration;

use mz_core::AppState;
use mz_jobs::{CronJob, JobFuture, Schedule};

pub use routes::router;

fn sync_all(state: AppState) -> JobFuture {
    Box::pin(async move { company::scheduled_sync_all_companies(&state).await })
}

fn renew_channels(state: AppState) -> JobFuture {
    Box::pin(async move { company::scheduled_renew_webhook_channels(&state).await })
}

/// Cron `GoogleCalendarService` (`google-calendar.service.ts:1214`, `:1250`).
pub fn cron_jobs() -> Vec<CronJob> {
    vec![
        CronJob {
            // `@Cron('0 35 * * * *')` — tiap jam menit 35 (tanpa zona; zona tak relevan).
            name: "scheduled-sync-all-companies",
            schedule: Schedule::hourly(&[35]),
            timeout: Duration::from_secs(55 * 60),
            run: sync_all,
        },
        CronJob {
            // `@Cron('0 40 1,22 * * *', { timeZone: CRON_TIMEZONE })` — 01:40 & 22:40 WIB.
            name: "scheduled-renew-webhook-channels",
            schedule: Schedule::daily(&[1, 22], &[40]),
            timeout: Duration::from_secs(30 * 60),
            run: renew_channels,
        },
    ]
}
