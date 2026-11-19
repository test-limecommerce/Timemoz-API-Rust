//! Port `src/leave-holidays/**` (10 route `/leave-holidays*`) + `POST /admin/national-holidays/apply`
//! (operator, `src/admin-global/national-holidays.*`) dari Mudah-API. CRUD `national-holidays`
//! lain tetap di `mz-admin-jobs`.
//!
//! Controller: `@RequireFeature('leave-config')`, `@SkipThrottle()`, `@UseGuards(JwtAuthGuard,
//! RolesGuard)`; `@Roles` per handler (baca: semua role; tulis: tanpa STAFF). Perubahan libur
//! memasang job `leave.holiday-sync` (`mz_leave::jobs`) sesudah commit. Path statis
//! (`import`, `import/*`, `next-year-reminder`) menang atas `:id`; method lain pada path
//! satu-segmen itu jatuh ke handler `:id` seperti di Nest (`parseInt` → NaN → 500).
//!
//! Pasang di `mz-server`: `.merge(mz_leave_holidays::router(&state))`. Handler job
//! `APPLY_HOLIDAYS` ([`run_apply_holidays_job`]) harus didaftarkan ke worker `admin-jobs`
//! (lihat `plan/deviations/mz-leave-holidays.md`).

mod apply;
mod csv;
mod dto;
mod holiday;
mod import;
mod routes;
mod upload;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::routing::{delete, get, patch, post, put};
use mz_core::enums::Role;
use mz_core::gate::guard;
use mz_core::{AppState, RouteSpec};

pub use apply::{
    ApplyCompanyResult, ApplyDto, Created, apply_to_company, run_apply_holidays_job, validate_apply,
};
pub use import::{Classification, MAX_IMPORT_BYTES, MAX_IMPORT_ROWS};
pub use routes::next_year_reminder;

const READ_ROLES: &[Role] = &[
    Role::SYSTEM_ADMIN,
    Role::SUPER_ADMIN,
    Role::ADMIN,
    Role::MANAGER,
    Role::STAFF,
];
const WRITE_ROLES: &[Role] = &[
    Role::SYSTEM_ADMIN,
    Role::SUPER_ADMIN,
    Role::ADMIN,
    Role::MANAGER,
];

/// Kelas controller: `@RequireFeature('leave-config')`.
const READ: RouteSpec = RouteSpec::jwt().feature("leave-config").roles(READ_ROLES);
const WRITE: RouteSpec = RouteSpec::jwt().feature("leave-config").roles(WRITE_ROLES);
/// `@PermissionExempt('separate-auth')` + `@AllowWhenSubscriptionLocked()`; otorisasi lewat
/// extractor `Operator`.
const OPERATOR: RouteSpec = RouteSpec::public().allow_locked();

/// Multer memotong di 256 KB; body multipart sedikit lebih besar masih dibaca.
const BODY_LIMIT: usize = 4 * 1024 * 1024;

pub fn router(state: &AppState) -> Router<AppState> {
    use routes as r;
    Router::new()
        .route("/leave-holidays", guard(state, READ, get(r::h_list)))
        .route("/leave-holidays", guard(state, WRITE, post(r::h_create)))
        .route(
            "/leave-holidays/import/template",
            guard(state, WRITE, get(r::h_template)),
        )
        .route(
            "/leave-holidays/import/preview",
            guard(
                state,
                WRITE,
                post(r::h_preview).layer(DefaultBodyLimit::max(BODY_LIMIT)),
            ),
        )
        .route(
            "/leave-holidays/import",
            guard(
                state,
                WRITE,
                post(r::h_import).layer(DefaultBodyLimit::max(BODY_LIMIT)),
            ),
        )
        .route(
            "/leave-holidays/import",
            guard(state, READ, get(r::h_get_import)),
        )
        .route(
            "/leave-holidays/import",
            guard(state, WRITE, put(r::h_update_import)),
        )
        .route(
            "/leave-holidays/import",
            guard(state, WRITE, delete(r::h_delete_import)),
        )
        .route(
            "/leave-holidays/next-year-reminder",
            guard(state, WRITE, get(r::h_reminder)),
        )
        .route(
            "/leave-holidays/next-year-reminder",
            guard(state, WRITE, put(r::h_update_reminder)),
        )
        .route(
            "/leave-holidays/next-year-reminder",
            guard(state, WRITE, delete(r::h_delete_reminder)),
        )
        .route("/leave-holidays/{id}", guard(state, READ, get(r::h_get)))
        .route(
            "/leave-holidays/{id}",
            guard(state, WRITE, put(r::h_update)),
        )
        .route(
            "/leave-holidays/{id}",
            guard(state, WRITE, delete(r::h_delete)),
        )
        .route(
            "/leave-holidays/{id}/toggle",
            guard(state, WRITE, patch(r::h_toggle)),
        )
        .route(
            "/admin/national-holidays/apply",
            guard(state, OPERATOR, post(apply::h_apply)),
        )
        .route(
            "/admin/national-holidays/apply",
            guard(state, OPERATOR, patch(apply::h_apply_as_uuid_patch)),
        )
        .route(
            "/admin/national-holidays/apply",
            guard(state, OPERATOR, delete(apply::h_apply_as_uuid_delete)),
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
