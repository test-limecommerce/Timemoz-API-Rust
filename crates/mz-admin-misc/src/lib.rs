//! Port Node untuk route operator "lain-lain" (`plan/ADMIN_AGENT_BRIEF.md`, kepemilikan `misc`):
//!
//! * `admin-pricing/**` (5) — [`pricing`]
//! * `admin-prohibited-apps/**` (5) — [`prohibited_apps`]
//! * `operator-audit/operator-audit-logs.controller.ts` (2) — [`audit_logs`]
//! * `admin-global/mobile-versions.*` (2) — [`mobile_versions`]
//!
//! Semua route ber-`@PermissionExempt('separate-auth')`: `RouteSpec::public()` + extractor
//! [`mz_operator::Operator`] (lihat `mz-operator`). Tidak ada cron/worker di crate ini.
//! Deviasi: `plan/deviations/mz-admin-misc.md`.

pub mod audit_logs;
pub mod common;
pub mod mobile_versions;
pub mod operator_actions;
pub mod pricing;
pub mod prohibited_apps;

use axum::Router;
use axum::routing::{delete, get, patch, post, put};
use mz_core::gate::guard;
use mz_core::{AppState, RouteSpec};

/// Operator (`separate-auth`): gerbang di extractor `Operator`, bukan di `RouteSpec`.
const OPERATOR: RouteSpec = RouteSpec::public();

/// Router seluruh route crate ini (path penuh, tanpa prefix).
pub fn router(state: &AppState) -> Router<AppState> {
    Router::new()
        // ───── admin-pricing ─────
        .route(
            "/admin-pricing",
            guard(state, OPERATOR, get(pricing::h_list)),
        )
        .route(
            "/admin-pricing/create",
            guard(state, OPERATOR, post(pricing::h_create)),
        )
        // Nest: `GET /admin-pricing/create` cocok ke `GET :uuid` (ParseUUIDPipe → 400).
        .route(
            "/admin-pricing/create",
            guard(state, OPERATOR, get(pricing::h_find_literal_create)),
        )
        .route(
            "/admin-pricing/{uuid}",
            guard(state, OPERATOR, get(pricing::h_find_one)),
        )
        .route(
            "/admin-pricing/update/{uuid}",
            guard(state, OPERATOR, patch(pricing::h_update)),
        )
        .route(
            "/admin-pricing/delete/{uuid}",
            guard(state, OPERATOR, delete(pricing::h_delete)),
        )
        // ───── admin-prohibited-apps ─────
        .route(
            "/admin-prohibited-apps",
            guard(state, OPERATOR, get(prohibited_apps::h_list)),
        )
        .route(
            "/admin-prohibited-apps/create",
            guard(state, OPERATOR, post(prohibited_apps::h_create)),
        )
        // Nest: `GET /admin-prohibited-apps/create` cocok ke `GET :uuid` tanpa pipe → 200 success:false.
        .route(
            "/admin-prohibited-apps/create",
            guard(state, OPERATOR, get(prohibited_apps::h_find_literal_create)),
        )
        .route(
            "/admin-prohibited-apps/{uuid}",
            guard(state, OPERATOR, get(prohibited_apps::h_find_one)),
        )
        .route(
            "/admin-prohibited-apps/update/{uuid}",
            guard(state, OPERATOR, patch(prohibited_apps::h_update)),
        )
        .route(
            "/admin-prohibited-apps/delete/{uuid}",
            guard(state, OPERATOR, delete(prohibited_apps::h_delete)),
        )
        // ───── admin/audit-logs (operator) ─────
        .route(
            "/admin/audit-logs",
            guard(state, OPERATOR, get(audit_logs::h_list)),
        )
        .route(
            "/admin/audit-logs/actions",
            guard(state, OPERATOR, get(audit_logs::h_actions)),
        )
        // ───── admin/mobile-versions (operator) ─────
        .route(
            "/admin/mobile-versions",
            guard(state, OPERATOR, get(mobile_versions::h_list)),
        )
        .route(
            "/admin/mobile-versions/{type}",
            guard(state, OPERATOR, put(mobile_versions::h_set)),
        )
}
