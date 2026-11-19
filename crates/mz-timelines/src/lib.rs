//! Port `src/tracking-timelines` dari Mudah-API (13 route `TrackingTimelineController`).
//!
//! Class: `@PermissionExempt('self-scoped')` + `@RequireModule(TIME_TRACKING)`;
//! per route `@UseGuards(JwtAuthGuard[, RolesGuard])` (+ `@Roles` admin/manager di
//! bulk-update-billable, `:uuid/note`, `:uuid/split`, `:uuid/split-time`).
//! `main.ts`: limiter bawaan ber-token di `app.use('/tracking-timelines/update-offline/:uuid')`
//! — prefix Express, jadi `update-offline/dashboard/:uuid` ikut terkena.
#![cfg_attr(test, allow(clippy::indexing_slicing))]

mod access;
pub mod approve;
mod dto;
mod fail;
mod handlers;
mod manual;
mod range;
mod service;
mod spawn;
mod tx;
mod worklimit;

use axum::Router;
use axum::routing::{delete, get, patch, post};
use mz_core::enums::{FeatureModule, Role};
use mz_core::gate::guard;
use mz_core::permissions::Exempt;
use mz_core::rate_limit::{RateKey, RateRule};
use mz_core::{AppState, RouteSpec};

const SPEC: RouteSpec = RouteSpec::jwt()
    .exempt(Exempt::SelfScoped)
    .modules(&[FeatureModule::TIME_TRACKING]);

/// `@Roles(SYSTEM_ADMIN, SUPER_ADMIN, ADMIN, MANAGER)` + `RolesGuard`.
const MANAGER_SPEC: RouteSpec = SPEC.roles(&[
    Role::SYSTEM_ADMIN,
    Role::SUPER_ADMIN,
    Role::ADMIN,
    Role::MANAGER,
]);

/// `createRateLimiter()` tanpa opsi (main.ts:117): `rl:token`, `RATE_LIMIT_MAX`, 60 dtk, per token.
const OFFLINE_LIMITER: RateRule = RateRule {
    key_prefix: "rl:token",
    max: None,
    window_ms: 60_000,
    key: RateKey::Token,
    pass_on_store_error: false,
};
const OFFLINE_SPEC: RouteSpec = SPEC.rate_limits(&[OFFLINE_LIMITER]);

pub fn router(state: &AppState) -> Router<AppState> {
    let bad_uuid = handlers::uuid_pipe_rejection;
    Router::new()
        .route(
            "/tracking-timelines",
            guard(state, SPEC, post(handlers::create)),
        )
        .route(
            "/tracking-timelines/manual-time",
            guard(state, SPEC, post(handlers::create_manual_time)),
        )
        .route(
            "/tracking-timelines/manual-time",
            guard(state, SPEC, get(bad_uuid).patch(bad_uuid).delete(bad_uuid)),
        )
        .route(
            "/tracking-timelines/create-offline",
            guard(state, SPEC, post(handlers::create_offline)),
        )
        .route(
            "/tracking-timelines/create-offline",
            guard(state, SPEC, get(bad_uuid).patch(bad_uuid).delete(bad_uuid)),
        )
        .route(
            "/tracking-timelines/bulk-update-billable",
            guard(state, MANAGER_SPEC, patch(handlers::bulk_update_billable)),
        )
        .route(
            "/tracking-timelines/bulk-update-billable",
            guard(state, SPEC, get(bad_uuid).delete(bad_uuid)),
        )
        .route(
            "/tracking-timelines/{uuid}",
            guard(state, SPEC, get(handlers::find_one)),
        )
        .route(
            "/tracking-timelines/{uuid}",
            guard(state, SPEC, patch(handlers::update)),
        )
        .route(
            "/tracking-timelines/{uuid}",
            guard(state, SPEC, delete(handlers::remove)),
        )
        .route(
            "/tracking-timelines/{uuid}/note",
            guard(state, MANAGER_SPEC, patch(handlers::update_note)),
        )
        .route(
            "/tracking-timelines/update-offline/{uuid}",
            guard(state, OFFLINE_SPEC, patch(handlers::update)),
        )
        .route(
            "/tracking-timelines/update-offline/dashboard/{uuid}",
            guard(
                state,
                OFFLINE_SPEC,
                patch(handlers::update_dashboard_offline),
            ),
        )
        .route(
            "/tracking-timelines/delete-offline/{uuid}",
            guard(state, SPEC, delete(handlers::remove)),
        )
        .route(
            "/tracking-timelines/{uuid}/split",
            guard(state, MANAGER_SPEC, post(handlers::split_entry)),
        )
        .route(
            "/tracking-timelines/{uuid}/split-time",
            guard(state, MANAGER_SPEC, post(handlers::split_time)),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn router_builds_without_route_conflicts() {
        let Ok(state) = AppState::for_test() else {
            return;
        };
        let _ = router(&state);
    }
}
