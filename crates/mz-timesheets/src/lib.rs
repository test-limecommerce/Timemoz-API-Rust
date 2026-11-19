//! Port `src/timesheets` (16 route), `src/timesheet-invoices` (5),
//! `src/manager-summary` (4), dan `src/scheduled-reports` (5) dari Mudah-API.
#![cfg_attr(
    test,
    allow(
        clippy::indexing_slicing,
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic
    )
)]

use axum::Router;
use axum::routing::{delete, get, patch, post, put};
use mz_core::enums::{FeatureModule, PermissionAction, Role};
use mz_core::gate::guard;
use mz_core::{AppState, RouteSpec};

pub mod breakdown;
pub mod calc;
pub mod common;
pub mod dto;
pub mod invoices;
pub mod jobs;
pub use mz_core::jsfmt;
pub mod mail;
pub mod manager_summary;
pub mod pdf;
pub mod pdf_reports;
pub mod rowjson;
pub mod scheduled_reports;
pub mod timesheets;

const ADMIN_ROLES: &[Role] = &[
    Role::SUPER_ADMIN,
    Role::SYSTEM_ADMIN,
    Role::ADMIN,
    Role::MANAGER,
];

const TIME_TRACKING: &[FeatureModule] = &[FeatureModule::TIME_TRACKING];

/// `@RequireFeature('timesheets')` + `@RequireModule(TIME_TRACKING)` + `@UseGuards(JwtAuthGuard)`.
const TS_JWT: RouteSpec = RouteSpec::jwt()
    .feature("timesheets")
    .modules(TIME_TRACKING);
/// Sama + `@Roles(SUPER_ADMIN, SYSTEM_ADMIN, ADMIN, MANAGER)`.
const TS_ADMIN: RouteSpec = RouteSpec::jwt()
    .roles(ADMIN_ROLES)
    .feature("timesheets")
    .modules(TIME_TRACKING);
/// `@RequirePermission(PermissionAction.DELETE)` pada `POST my/withdraw/:uuid`.
const TS_WITHDRAW: RouteSpec = RouteSpec::jwt()
    .feature("timesheets")
    .modules(TIME_TRACKING)
    .action(PermissionAction::DELETE);
/// `GET email-action`: tanpa `JwtAuthGuard`, jadi semua interceptor global dilewati.
const TS_PUBLIC: RouteSpec = RouteSpec::public();

/// `@RequireFeature('invoices')` + `@RequireModule` + `@Roles(...)` (class).
const INVOICE: RouteSpec = RouteSpec::jwt()
    .roles(ADMIN_ROLES)
    .feature("invoices")
    .modules(TIME_TRACKING);

/// `@RequireFeature('reports')` + `@RequireModule` + `@Roles(SUPER_ADMIN, ADMIN, MANAGER)`.
const MANAGER_RW: RouteSpec = RouteSpec::jwt()
    .roles(&[Role::SUPER_ADMIN, Role::ADMIN, Role::MANAGER])
    .feature("reports")
    .modules(TIME_TRACKING);
/// `DELETE manager-summary/config`: `@Roles(SUPER_ADMIN, ADMIN)`.
const MANAGER_DELETE: RouteSpec = RouteSpec::jwt()
    .roles(&[Role::SUPER_ADMIN, Role::ADMIN])
    .feature("reports")
    .modules(TIME_TRACKING);

/// `@RequireFeature('reports')` + `@Roles(SUPER_ADMIN, SYSTEM_ADMIN, ADMIN, MANAGER)`.
const SCHEDULED: RouteSpec = RouteSpec::jwt().roles(ADMIN_ROLES).feature("reports");

pub use jobs::cron_jobs;

pub fn router(state: &AppState) -> Router<AppState> {
    Router::new()
        // ── timesheets ──
        .route(
            "/timesheets/email-action",
            guard(state, TS_PUBLIC, get(timesheets::email_action)),
        )
        .route(
            "/timesheets/submit",
            guard(state, TS_JWT, post(timesheets::submit)),
        )
        .route(
            "/timesheets/my",
            guard(state, TS_JWT, get(timesheets::get_my_timesheets)),
        )
        // Method yang hanya dimiliki `:uuid` di Node ditelan `:uuid`.
        .route(
            "/timesheets/email-action",
            guard(state, TS_ADMIN, delete(timesheets::static_as_uuid)),
        )
        .route(
            "/timesheets/submit",
            guard(
                state,
                TS_ADMIN,
                get(timesheets::static_as_uuid).delete(timesheets::static_as_uuid),
            ),
        )
        .route(
            "/timesheets/my",
            guard(state, TS_ADMIN, delete(timesheets::static_as_uuid)),
        )
        .route(
            "/timesheets/my/current",
            guard(state, TS_JWT, get(timesheets::get_my_current)),
        )
        .route(
            "/timesheets/my/withdraw/{uuid}",
            guard(state, TS_WITHDRAW, post(timesheets::withdraw_my)),
        )
        .route(
            "/timesheets/admin/create",
            guard(state, TS_ADMIN, post(timesheets::admin_create)),
        )
        .route(
            "/timesheets",
            guard(state, TS_ADMIN, get(timesheets::find_all)),
        )
        .route(
            "/timesheets/{uuid}",
            guard(state, TS_ADMIN, get(timesheets::find_one)),
        )
        .route(
            "/timesheets/{uuid}",
            guard(state, TS_ADMIN, delete(timesheets::delete)),
        )
        .route(
            "/timesheets/{uuid}/export/pdf",
            guard(state, TS_ADMIN, get(timesheets::export_pdf)),
        )
        .route(
            "/timesheets/{uuid}/export/csv",
            guard(state, TS_ADMIN, get(timesheets::export_csv)),
        )
        .route(
            "/timesheets/{uuid}/approve",
            guard(state, TS_ADMIN, patch(timesheets::approve)),
        )
        .route(
            "/timesheets/{uuid}/reject",
            guard(state, TS_ADMIN, patch(timesheets::reject)),
        )
        .route(
            "/timesheets/{uuid}/edit",
            guard(state, TS_ADMIN, patch(timesheets::edit)),
        )
        .route(
            "/timesheets/{uuid}/recalculate",
            guard(state, TS_ADMIN, patch(timesheets::recalculate)),
        )
        .route(
            "/timesheets/{uuid}/reopen",
            guard(state, TS_ADMIN, patch(timesheets::reopen)),
        )
        // ── timesheet-invoices ──
        .route(
            "/timesheet-invoices/{uuid}/generate",
            guard(state, INVOICE, post(invoices::generate)),
        )
        .route(
            "/timesheet-invoices",
            guard(state, INVOICE, get(invoices::find_all)),
        )
        .route(
            "/timesheet-invoices/{uuid}",
            guard(state, INVOICE, get(invoices::find_one)),
        )
        .route(
            "/timesheet-invoices/{uuid}/download/pdf",
            guard(state, INVOICE, get(invoices::download_pdf)),
        )
        .route(
            "/timesheet-invoices/{uuid}/status",
            guard(state, INVOICE, patch(invoices::update_status)),
        )
        // ── manager-summary ──
        .route(
            "/manager-summary/config",
            guard(state, MANAGER_RW, post(manager_summary::create)),
        )
        .route(
            "/manager-summary/config",
            guard(state, MANAGER_RW, get(manager_summary::find_one)),
        )
        .route(
            "/manager-summary/config",
            guard(state, MANAGER_RW, put(manager_summary::update)),
        )
        .route(
            "/manager-summary/config",
            guard(state, MANAGER_DELETE, delete(manager_summary::remove)),
        )
        // ── scheduled-reports ──
        .route(
            "/scheduled-reports",
            guard(state, SCHEDULED, post(scheduled_reports::create)),
        )
        .route(
            "/scheduled-reports",
            guard(state, SCHEDULED, get(scheduled_reports::find_all)),
        )
        .route(
            "/scheduled-reports/{uuid}",
            guard(state, SCHEDULED, get(scheduled_reports::find_one)),
        )
        .route(
            "/scheduled-reports/{uuid}",
            guard(state, SCHEDULED, patch(scheduled_reports::update)),
        )
        .route(
            "/scheduled-reports/{uuid}",
            guard(state, SCHEDULED, delete(scheduled_reports::remove)),
        )
}

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::routing::{delete, get, patch, post, put};

    /// Pastikan tidak ada konflik pola path matchit pada kumpulan route ini
    /// (axum panik saat pendaftaran bila ada).
    #[test]
    fn route_table_has_no_conflicts() {
        async fn h() {}
        let _: Router = Router::new()
            .route("/timesheets/email-action", get(h))
            .route("/timesheets/submit", post(h))
            .route("/timesheets/my", get(h))
            .route("/timesheets/my/current", get(h))
            .route("/timesheets/my/withdraw/{uuid}", post(h))
            .route("/timesheets/admin/create", post(h))
            .route("/timesheets", get(h))
            .route("/timesheets/{uuid}", get(h))
            .route("/timesheets/{uuid}", delete(h))
            .route("/timesheets/{uuid}/export/pdf", get(h))
            .route("/timesheets/{uuid}/export/csv", get(h))
            .route("/timesheets/{uuid}/approve", patch(h))
            .route("/timesheets/{uuid}/reject", patch(h))
            .route("/timesheets/{uuid}/edit", patch(h))
            .route("/timesheets/{uuid}/recalculate", patch(h))
            .route("/timesheets/{uuid}/reopen", patch(h))
            .route("/timesheet-invoices/{uuid}/generate", post(h))
            .route("/timesheet-invoices", get(h))
            .route("/timesheet-invoices/{uuid}", get(h))
            .route("/timesheet-invoices/{uuid}/download/pdf", get(h))
            .route("/timesheet-invoices/{uuid}/status", patch(h))
            .route("/manager-summary/config", post(h))
            .route("/manager-summary/config", get(h))
            .route("/manager-summary/config", put(h))
            .route("/manager-summary/config", delete(h))
            .route("/scheduled-reports", post(h))
            .route("/scheduled-reports", get(h))
            .route("/scheduled-reports/{uuid}", get(h))
            .route("/scheduled-reports/{uuid}", patch(h))
            .route("/scheduled-reports/{uuid}", delete(h));
    }
}
