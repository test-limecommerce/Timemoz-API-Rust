//! Port tahap S2 sales tracker: `src/customers/**` — customers (6 route) + customer masters (5 route).
//!
//! - `/v1/admin/customers*`: `@RequireFeature('customers')`, `@RequireModule(SALES_TRACKER)`,
//!   `@Roles(SYSTEM_ADMIN, SUPER_ADMIN, ADMIN, MANAGER)`; `@TableFields('customers')` pada list/detail/
//!   create/update (kebijakan kolom `mz-table-fields`), export melewati interceptor dan menyensor sendiri.
//! - `/v1/admin/customer-masters/:kind*`: `@RequireFeature('customer-masters')`, modul & role sama.
//! - Model `customer`, `customerType`, `customerGroup`, `customerChannel`, `customerSubChannel` ber-audit
//!   (`mz_audit::AuditHandle`).
//! - `router()` juga mendaftarkan resolver reveal tabel `customers` ke `mz_table_fields::reveal`.
//! - `pub`: [`phone::normalize_store_phone`] (dipakai store-visits S3), [`tax`], [`default_types`],
//!   [`backfill`] (rencana backfill customer dari toko), dan pembantu master/customer.
//!
//! Ringkasan API publik: `plan/sales-mz-customers-api.md`. Deviasi: `plan/deviations/mz-customers.md`.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic
    )
)]

pub mod backfill;
pub mod customers;
pub mod default_types;
pub mod dto;
pub mod export;
pub mod masters;
pub mod phone;
pub mod tax;

use std::sync::Arc;

use axum::Router;
use axum::routing::{delete, get, patch, post};
use mz_core::enums::{FeatureModule, Role};
use mz_core::gate::guard;
use mz_core::{AppState, RouteSpec};
use mz_table_fields::registry::CUSTOMERS_REVEAL_RESOLVER;
use mz_table_fields::reveal::register_resolver;

const ROLES: &[Role] = &[
    Role::SYSTEM_ADMIN,
    Role::SUPER_ADMIN,
    Role::ADMIN,
    Role::MANAGER,
];

/// `AdminCustomersController`.
const CUSTOMERS: RouteSpec = RouteSpec::jwt()
    .feature("customers")
    .modules(&[FeatureModule::SALES_TRACKER])
    .roles(ROLES);
/// `AdminCustomerMastersController`.
const MASTERS: RouteSpec = RouteSpec::jwt()
    .feature("customer-masters")
    .modules(&[FeatureModule::SALES_TRACKER])
    .roles(ROLES);

/// 11 route (path penuh, tanpa prefix). Mendaftarkan resolver reveal `customers`.
pub fn router(state: &AppState) -> Router<AppState> {
    register_resolver(
        CUSTOMERS_REVEAL_RESOLVER,
        Arc::new(customers::CustomersRevealResolver),
    );
    const C: &str = "/v1/admin/customers";
    const M: &str = "/v1/admin/customer-masters/{kind}";
    Router::new()
        // ── customers (`export` statis sebelum `{id}`) ──
        .route(C, guard(state, CUSTOMERS, get(customers::h_find_all)))
        .route(C, guard(state, CUSTOMERS, post(customers::h_create)))
        .route(
            &format!("{C}/export"),
            guard(state, CUSTOMERS, get(customers::h_export)),
        )
        .route(
            &format!("{C}/{{id}}"),
            guard(state, CUSTOMERS, get(customers::h_find_one)),
        )
        .route(
            &format!("{C}/{{id}}"),
            guard(state, CUSTOMERS, patch(customers::h_update)),
        )
        .route(
            &format!("{C}/{{id}}"),
            guard(state, CUSTOMERS, delete(customers::h_remove)),
        )
        // ── customer masters ──
        .route(M, guard(state, MASTERS, get(masters::h_find_all)))
        .route(M, guard(state, MASTERS, post(masters::h_create)))
        .route(
            &format!("{M}/{{id}}"),
            guard(state, MASTERS, get(masters::h_find_one)),
        )
        .route(
            &format!("{M}/{{id}}"),
            guard(state, MASTERS, patch(masters::h_update)),
        )
        .route(
            &format!("{M}/{{id}}"),
            guard(state, MASTERS, delete(masters::h_remove)),
        )
}
