//! Port `src/trackings` dari Mudah-API (8 route).
//!
//! Controller: `@PermissionExempt('self-scoped')` + `@RequireModule(TIME_TRACKING)`
//! di level class, `@UseGuards(JwtAuthGuard)` per route (tanpa `@Roles`).

mod calc;
pub mod division;
mod dto;
mod handlers;
mod model;
mod service;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::routing::MethodRouter;
use axum::routing::{delete, get, patch, post};
use mz_core::enums::FeatureModule;
use mz_core::gate::guard;
use mz_core::permissions::Exempt;
use mz_core::{AppState, RouteSpec};

const SPEC: RouteSpec = RouteSpec::jwt()
    .exempt(Exempt::SelfScoped)
    .modules(&[FeatureModule::TIME_TRACKING]);

/// Parser JSON bawaan Express: 100kb.
const JSON_LIMIT: usize = 100 * 1024;

/// GET/PATCH/DELETE pada path statis yang di Node ditelan `:uuid`.
fn shadowed() -> MethodRouter<AppState> {
    get(handlers::static_as_uuid)
        .patch(handlers::static_as_uuid)
        .delete(handlers::static_as_uuid)
}

pub fn router(state: &AppState) -> Router<AppState> {
    Router::new()
        .route("/trackings", guard(state, SPEC, get(handlers::find_all)))
        .route(
            "/trackings",
            guard(
                state,
                SPEC,
                post(handlers::create).layer(DefaultBodyLimit::max(JSON_LIMIT)),
            ),
        )
        .route(
            "/trackings/date-time",
            guard(
                state,
                SPEC,
                get(handlers::server_date_time)
                    .patch(handlers::static_as_uuid)
                    .delete(handlers::static_as_uuid),
            ),
        )
        .route(
            "/trackings/sync-local-db",
            guard(
                state,
                SPEC,
                post(handlers::sync_local_db)
                    .layer(DefaultBodyLimit::max(JSON_LIMIT))
                    .merge(shadowed()),
            ),
        )
        .route(
            "/trackings/count/{uuid}",
            guard(state, SPEC, get(handlers::count_tracking)),
        )
        .route(
            "/trackings/{uuid}",
            guard(state, SPEC, get(handlers::find_one)),
        )
        .route(
            "/trackings/{uuid}",
            guard(
                state,
                SPEC,
                patch(handlers::update).layer(DefaultBodyLimit::max(JSON_LIMIT)),
            ),
        )
        .route(
            "/trackings/{uuid}",
            guard(state, SPEC, delete(handlers::remove)),
        )
}
