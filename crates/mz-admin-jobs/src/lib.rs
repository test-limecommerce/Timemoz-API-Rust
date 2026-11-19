//! Port `src/admin-jobs/**` (job operator ber-DB: backup/restore/merge company) dan
//! `src/admin-global/national-holidays.*` (Mudah-API).
//!
//! - [`router`]: `admin/jobs/*` (6 route) + `admin/national-holidays` (4 route; `apply` tetap
//!   di Node, lihat `plan/deviations/mz-admin-jobs.md`).
//! - [`spawn_worker`]: worker antrean `admin-jobs` (concurrency 1) + reaper. Job yang dibuat Rust
//!   hanya dijalankan worker ini; job Node tetap dijalankan processor Bull Node.

use axum::Router;
use axum::routing::{delete, get, patch, post};
use mz_core::AppState;
use mz_core::gate::{RouteSpec, guard};

pub mod company_data;
pub mod holidays;
pub mod jobs;
pub mod validate;

pub use jobs::worker::spawn_worker;

/// `@PermissionExempt('separate-auth')` + `@AllowWhenSubscriptionLocked()`: tanpa gerbang user,
/// otorisasi lewat extractor `Operator`.
const OPERATOR: RouteSpec = RouteSpec::public().allow_locked();

/// Router seluruh route crate ini (path penuh, tanpa prefix).
pub fn router(state: &AppState) -> Router<AppState> {
    use jobs::routes as j;
    let g = |r| guard(state, OPERATOR, r);
    Router::new()
        .route("/admin/jobs", g(get(j::h_list)))
        .route("/admin/jobs/backup", g(post(j::h_backup)))
        .route("/admin/jobs/backup", g(get(j::h_get_static)))
        .route("/admin/jobs/restore", g(post(j::h_restore)))
        .route("/admin/jobs/restore", g(get(j::h_get_static)))
        .route("/admin/jobs/merge", g(post(j::h_merge)))
        .route("/admin/jobs/merge", g(get(j::h_get_static)))
        .route("/admin/jobs/{uuid}", g(get(j::h_get)))
        .route("/admin/jobs/{uuid}/download", g(get(j::h_download)))
        .route("/admin/national-holidays", g(get(holidays::h_list)))
        .route("/admin/national-holidays", g(post(holidays::h_create)))
        .route(
            "/admin/national-holidays/{uuid}",
            g(patch(holidays::h_update)),
        )
        .route(
            "/admin/national-holidays/{uuid}",
            g(delete(holidays::h_remove)),
        )
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn router_tanpa_konflik() {
        if let Ok(state) = mz_core::AppState::for_test() {
            let _ = super::router(&state);
        }
    }
}
