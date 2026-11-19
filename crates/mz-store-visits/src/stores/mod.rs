//! Toko: `admin-stores.controller.ts` (9 route `/v1/admin/stores`) + `mobile-stores.controller.ts`
//! (4 route `/mobile/stores`) + resolver reveal tabel `stores`.
//!
//! Gerbang: `@RequireFeature('stores')`, `@RequireModule(SALES_TRACKER)`, `@Roles` admin/mobile.
//! Limiter `main.ts`: `rl:admin-stores-read` (GET list + GET `/{id}`), `rl:admin-stores-write`
//! (POST, PATCH `/:id` — juga cocok dengan `bulk-group`/`bulk-customer` di express —, DELETE),
//! `rl:export:company` (export, per company), `rl:mobile-stores-list`, `rl:mobile-stores-detail`,
//! `rl:mobile-stores-resolve-qr`.

pub mod admin;
pub mod common;
pub mod dto;
pub mod export;
pub mod mobile;

use std::sync::Arc;

use axum::Router;
use axum::middleware::from_fn_with_state;
use axum::routing::{delete, get, patch, post};
use mz_core::enums::{FeatureModule, PermissionAction};
use mz_core::gate::guard;
use mz_core::rate_limit::RateRule;
use mz_core::{AppState, RouteSpec};
use mz_table_fields::registry::STORES_REVEAL_RESOLVER;
use mz_table_fields::reveal::register_resolver;

use crate::{ADMIN_ROLES, MOBILE_ROLES};

const MINUTE: u64 = 60_000;

const ADMIN_READ: RateRule = RateRule::token("rl:admin-stores-read", 60, MINUTE);
const ADMIN_WRITE: RateRule = RateRule::token("rl:admin-stores-write", 60, MINUTE);
const MOBILE_LIST: RateRule = RateRule::token("rl:mobile-stores-list", 20, MINUTE);
const MOBILE_DETAIL: RateRule = RateRule::token("rl:mobile-stores-detail", 60, MINUTE);
const MOBILE_RESOLVE_QR: RateRule = RateRule::token("rl:mobile-stores-resolve-qr", 20, MINUTE);

/// `AdminStoresController`.
const ADMIN: RouteSpec = RouteSpec::jwt()
    .feature("stores")
    .modules(&[FeatureModule::SALES_TRACKER])
    .roles(ADMIN_ROLES);
/// `MobileStoresController`.
const MOBILE: RouteSpec = RouteSpec::jwt()
    .feature("stores")
    .modules(&[FeatureModule::SALES_TRACKER])
    .roles(MOBILE_ROLES);

/// 13 route + pendaftaran `STORES_REVEAL_RESOLVER`.
pub(crate) fn routes(state: &AppState) -> Router<AppState> {
    register_resolver(
        STORES_REVEAL_RESOLVER,
        Arc::new(admin::StoresRevealResolver),
    );
    const A: &str = "/v1/admin/stores";
    const M: &str = "/mobile/stores";
    const READ: RouteSpec = ADMIN.rate_limits(&[ADMIN_READ]);
    const WRITE: RouteSpec = ADMIN.rate_limits(&[ADMIN_WRITE]);
    Router::new()
        // ── admin (path statis sebelum `{id}`) ──
        .route(A, guard(state, READ, get(admin::h_find_all)))
        .route(A, guard(state, WRITE, post(admin::h_create)))
        .route(
            &format!("{A}/export"),
            guard(state, ADMIN, get(admin::h_export)).layer(from_fn_with_state(
                state.clone(),
                crate::export::export_limiter,
            )),
        )
        .route(
            &format!("{A}/bulk-group"),
            guard(state, WRITE, patch(admin::h_bulk_group)),
        )
        .route(
            &format!("{A}/bulk-customer"),
            guard(state, WRITE, patch(admin::h_bulk_customer)),
        )
        .route(
            &format!("{A}/{{id}}"),
            guard(state, READ, get(admin::h_find_one)),
        )
        .route(
            &format!("{A}/{{id}}"),
            guard(state, WRITE, patch(admin::h_update)),
        )
        .route(
            &format!("{A}/{{id}}"),
            guard(state, WRITE, delete(admin::h_remove)),
        )
        .route(
            &format!("{A}/{{id}}/eligible-sales"),
            guard(state, ADMIN, get(admin::h_eligible_sales)),
        )
        // ── mobile ──
        .route(
            M,
            guard(
                state,
                MOBILE.rate_limits(&[MOBILE_LIST]),
                get(mobile::h_find_all),
            ),
        )
        .route(M, guard(state, MOBILE, post(mobile::h_create)))
        .route(
            &format!("{M}/resolve-qr"),
            guard(
                state,
                MOBILE
                    .action(PermissionAction::READ)
                    .rate_limits(&[MOBILE_RESOLVE_QR]),
                post(mobile::h_resolve_qr),
            ),
        )
        .route(
            &format!("{M}/{{id}}"),
            guard(
                state,
                MOBILE.rate_limits(&[MOBILE_DETAIL]),
                get(mobile::h_find_one),
            ),
        )
}
