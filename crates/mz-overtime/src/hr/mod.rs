//! 22 route HR lembur PP 35 (`/v1/overtime/*`): settings (4) + status (1), requests (7),
//! approvals (7), reports (3). Node: `src/overtime/*.controller.ts`.
//!
//! Kepemilikan submodul (port paralel): `common` + `settings` (fondasi), `requests`,
//! `approvals`, `reports`. Modul lain memakai `common`, jangan menyalinnya.

use axum::Router;
use mz_core::AppState;

pub mod approvals;
pub mod common;
pub mod reports;
pub mod requests;
pub mod settings;

/// Semua route `/v1/overtime/*` (dipasang `mz-server`).
pub fn router(state: &AppState) -> Router<AppState> {
    Router::new()
        .merge(settings::router(state))
        .merge(requests::router(state))
        .merge(approvals::router(state))
        .merge(reports::router(state))
}
