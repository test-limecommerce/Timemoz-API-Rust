//! Port `TasksService.handleCron` (`tasks/tasks.service.ts:88`) + dua route operator
//! `POST /admin/sync/trigger` dan `POST /admin/manager-summary/test` (`admin/admin.controller.ts:190,331`).
//!
//! - [`sync`]: `runSyncForDate` (agregasi `dailyUserProjectSummary` / `dailyProjectSummary` / `dailyCompanySummary`).
//! - [`worklimit`]: `checkWorkLimitsForFinishedDays` + `checkWorkLimits` (breach + email overtime).
//! - [`routes`]: dua route operator.
//!
//! Pengumpulan data & email manager summary dipakai ulang dari `mz_timesheets::jobs` / `mz_timesheets::mail`.

pub mod routes;
pub mod sync;
pub mod worklimit;

use std::time::Duration;

use axum::Router;
use axum::routing::post;
use mz_core::AppState;
use mz_core::RouteSpec;
use mz_core::gate::guard;
use mz_core::permissions::Exempt;
use mz_jobs::{CronJob, Schedule};

/// Route ber-`JwtAdminAuthGuard` (`@PermissionExempt('separate-auth')` + `@AllowWhenSubscriptionLocked`).
const OPERATOR: RouteSpec = RouteSpec::public()
    .exempt(Exempt::SeparateAuth)
    .allow_locked();

/// `@Cron('50 3 * * *', { timeZone: CRON_TIMEZONE })` -> `TasksService.handleCron`.
pub fn cron_jobs() -> Vec<CronJob> {
    vec![CronJob {
        name: "daily-sync",
        schedule: Schedule::daily(&[3], &[50]),
        timeout: Duration::from_secs(3600),
        run: |s| Box::pin(worklimit::handle_cron(s)),
    }]
}

pub fn router(state: &AppState) -> Router<AppState> {
    Router::new()
        .route(
            "/admin/sync/trigger",
            guard(state, OPERATOR, post(routes::sync_trigger)),
        )
        .route(
            "/admin/manager-summary/test",
            guard(state, OPERATOR, post(routes::manager_summary_test)),
        )
}
