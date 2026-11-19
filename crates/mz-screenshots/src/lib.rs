//! Port `src/screenshots` (8 route), `src/activity-logs` (3 route) dan `src/reports` (9 route)
//! dari Mudah-API. Kerja latar (cron + consumer Bull `screenshot`) ada di [`jobs`].
//!
//! Deviasi: `plan/deviations/mz-screenshots.md`.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic
    )
)]
// Primitif gambar PDF meniru API pdfkit (banyak argumen koordinat).
#![allow(clippy::too_many_arguments)]

pub mod activity_logs;
mod division;
pub mod jobs;
pub mod reports;
pub mod screenshots;
mod util;

use axum::Router;
use mz_core::AppState;

/// Job cron screenshot (didaftarkan orkestrator ke `mz-server`).
pub use jobs::cron_jobs;

/// Gabungan 20 route: screenshots + activity-logs + reports.
pub fn router(state: &AppState) -> Router<AppState> {
    Router::new()
        .merge(screenshots::router(state))
        .merge(activity_logs::router(state))
        .merge(reports::router(state))
}
