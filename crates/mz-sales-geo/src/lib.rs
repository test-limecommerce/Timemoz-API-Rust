//! Port tahap S1 sales tracker: fondasi geofence & negative zone & visit master (20 route) +
//! rumah helper bersama (`geo`, `plan`, `visit_masters::{resolve_visit_types, find_default, ...}`).
//!
//! - `src/mobile-geofences/**`: 6 route `/mobile/geofences*` + 4 route `/v1/admin/geofences*`
//!   (`@RequireFeature('geofences')`, `@SkipThrottle()`; tanpa `@RequireModule`).
//! - `src/negative-zones/**`: 4 route `/v1/admin/negative-zones*` + 1 route `/mobile/negative-zones`
//!   (`@RequireFeature('negative-zones')`).
//! - `src/visit-masters/**`: 5 route `/v1/admin/visit-masters/:kind*`
//!   (`@RequireFeature('visit-masters')`, `@RequireModule(SALES_TRACKER)`).
//!
//! Model `geofence` & `negativeZone` ditulis lewat `mz_audit::AuditHandle`. Ringkasan API publik:
//! `plan/sales-mz-sales-geo-api.md`. Deviasi: `plan/deviations/mz-sales-geo.md`.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic
    )
)]

pub mod dto;
pub mod geo;
pub mod geofences;
pub mod negative_zones;
pub mod plan;
pub mod visit_masters;

use axum::Router;
use axum::routing::{delete, get, patch, post};
use mz_core::enums::{FeatureModule, Role};
use mz_core::gate::guard;
use mz_core::{AppState, RouteSpec};

const MOBILE_ROLES: &[Role] = &[
    Role::SYSTEM_ADMIN,
    Role::SUPER_ADMIN,
    Role::ADMIN,
    Role::MANAGER,
    Role::STAFF,
];
const ADMIN_ROLES: &[Role] = &[Role::SYSTEM_ADMIN, Role::SUPER_ADMIN, Role::ADMIN];
const ADMIN_MANAGER_ROLES: &[Role] = &[
    Role::SYSTEM_ADMIN,
    Role::SUPER_ADMIN,
    Role::ADMIN,
    Role::MANAGER,
];

/// `MobileGeofencesController`.
const MOBILE_GEOFENCE: RouteSpec = RouteSpec::jwt().feature("geofences").roles(MOBILE_ROLES);
/// `AdminGeofencesController` (kelas: SYSTEM_ADMIN, SUPER_ADMIN, ADMIN).
const ADMIN_GEOFENCE: RouteSpec = RouteSpec::jwt().feature("geofences").roles(ADMIN_ROLES);
/// `GET /v1/admin/geofences` menimpa `@Roles` dengan MANAGER.
const ADMIN_GEOFENCE_LIST: RouteSpec = RouteSpec::jwt()
    .feature("geofences")
    .roles(ADMIN_MANAGER_ROLES);
const MOBILE_NEGATIVE: RouteSpec = RouteSpec::jwt()
    .feature("negative-zones")
    .roles(MOBILE_ROLES);
const ADMIN_NEGATIVE: RouteSpec = RouteSpec::jwt()
    .feature("negative-zones")
    .roles(ADMIN_MANAGER_ROLES);
const VISIT_MASTERS: RouteSpec = RouteSpec::jwt()
    .feature("visit-masters")
    .modules(&[FeatureModule::SALES_TRACKER])
    .roles(ADMIN_MANAGER_ROLES);

/// Semua route modul ini. Pasang di `mz-server` (kedua prefiks: `/mobile/*` dan `/v1/admin/*`).
pub fn router(state: &AppState) -> Router<AppState> {
    Router::new()
        // ── mobile geofences (statis sebelum `:id/schedule`) ──
        .route(
            "/mobile/geofences",
            guard(state, MOBILE_GEOFENCE, get(geofences::h_mobile_list)),
        )
        .route(
            "/mobile/geofences/dashboard",
            guard(state, MOBILE_GEOFENCE, get(geofences::h_dashboard)),
        )
        .route(
            "/mobile/geofences/nearby",
            guard(state, MOBILE_GEOFENCE, get(geofences::h_nearby)),
        )
        .route(
            "/mobile/geofences/event",
            guard(state, MOBILE_GEOFENCE, get(geofences::h_event_history)),
        )
        .route(
            "/mobile/geofences/event",
            guard(state, MOBILE_GEOFENCE, post(geofences::h_log_event)),
        )
        .route(
            "/mobile/geofences/{id}/schedule",
            guard(state, MOBILE_GEOFENCE, get(geofences::h_schedule)),
        )
        // ── admin geofences ──
        .route(
            "/v1/admin/geofences",
            guard(state, ADMIN_GEOFENCE_LIST, get(geofences::h_admin_list)),
        )
        .route(
            "/v1/admin/geofences",
            guard(state, ADMIN_GEOFENCE, post(geofences::h_admin_create)),
        )
        .route(
            "/v1/admin/geofences/{id}",
            guard(state, ADMIN_GEOFENCE, patch(geofences::h_admin_update)),
        )
        .route(
            "/v1/admin/geofences/{id}",
            guard(state, ADMIN_GEOFENCE, delete(geofences::h_admin_remove)),
        )
        // ── negative zones ──
        .route(
            "/mobile/negative-zones",
            guard(state, MOBILE_NEGATIVE, get(negative_zones::h_mobile_list)),
        )
        .route(
            "/v1/admin/negative-zones",
            guard(state, ADMIN_NEGATIVE, get(negative_zones::h_admin_list)),
        )
        .route(
            "/v1/admin/negative-zones",
            guard(state, ADMIN_NEGATIVE, post(negative_zones::h_admin_create)),
        )
        .route(
            "/v1/admin/negative-zones/{id}",
            guard(state, ADMIN_NEGATIVE, patch(negative_zones::h_admin_update)),
        )
        .route(
            "/v1/admin/negative-zones/{id}",
            guard(
                state,
                ADMIN_NEGATIVE,
                delete(negative_zones::h_admin_remove),
            ),
        )
        // ── visit masters ──
        .route(
            "/v1/admin/visit-masters/{kind}",
            guard(state, VISIT_MASTERS, get(visit_masters::h_find_all)),
        )
        .route(
            "/v1/admin/visit-masters/{kind}",
            guard(state, VISIT_MASTERS, post(visit_masters::h_create)),
        )
        .route(
            "/v1/admin/visit-masters/{kind}/{id}",
            guard(state, VISIT_MASTERS, get(visit_masters::h_find_one)),
        )
        .route(
            "/v1/admin/visit-masters/{kind}/{id}",
            guard(state, VISIT_MASTERS, patch(visit_masters::h_update)),
        )
        .route(
            "/v1/admin/visit-masters/{kind}/{id}",
            guard(state, VISIT_MASTERS, delete(visit_masters::h_remove)),
        )
}
