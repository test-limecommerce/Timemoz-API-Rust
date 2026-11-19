//! Port modul produk sales tracker (tahap S2): `src/products/**`, `src/catalog-query/**`,
//! validator generik `src/custom-fields/custom-fields.validator.ts`, `src/mobile-catalog/**`.
//!
//! Route (37): admin products 14, admin price lists 11, admin product masters 5, admin product
//! custom fields 5, product image publik 1, mobile catalog sync 1. Antrean import produk
//! (`product-import`) berjalan di `mz_core::queue` — worker: [`import::spawn_worker`].
//! Ringkasan API publik untuk S3: `plan/sales-mz-products-api.md`. Deviasi:
//! `plan/deviations/mz-products.md`.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic
    )
)]

pub mod catalog_query;
pub mod common;
pub mod custom_fields;
pub mod custom_fields_validator;
pub mod dto;
pub mod export;
pub mod images;
pub mod import;
pub mod masters;
pub mod mobile_catalog;
pub mod price_lists;
pub mod products;
pub mod util;

use axum::Router;
use axum::routing::{delete, get, patch, post};
use mz_core::enums::{FeatureModule, Role};
use mz_core::gate::guard;
use mz_core::{AppState, RouteSpec};

pub use import::spawn_worker;

/// `@Roles(SYSTEM_ADMIN, SUPER_ADMIN, ADMIN, MANAGER)`.
pub(crate) const ADMIN_ROLES: &[Role] = &[
    Role::SYSTEM_ADMIN,
    Role::SUPER_ADMIN,
    Role::ADMIN,
    Role::MANAGER,
];
/// Mobile: + STAFF.
pub(crate) const MOBILE_ROLES: &[Role] = &[
    Role::SYSTEM_ADMIN,
    Role::SUPER_ADMIN,
    Role::ADMIN,
    Role::MANAGER,
    Role::STAFF,
];
pub(crate) const SALES: &[FeatureModule] = &[FeatureModule::SALES_TRACKER];

/// `AdminProductsController` & `AdminProductCustomFieldsController` (`@RequireFeature('products')`).
pub(crate) const PRODUCTS: RouteSpec = RouteSpec::jwt()
    .feature("products")
    .modules(SALES)
    .roles(ADMIN_ROLES);
/// `AdminProductMastersController` (`@RequireFeature('product-masters')`).
pub(crate) const PRODUCT_MASTERS: RouteSpec = RouteSpec::jwt()
    .feature("product-masters")
    .modules(SALES)
    .roles(ADMIN_ROLES);
/// `AdminPriceListsController` (`@RequireFeature('price-lists')`).
pub(crate) const PRICE_LISTS: RouteSpec = RouteSpec::jwt()
    .feature("price-lists")
    .modules(SALES)
    .roles(ADMIN_ROLES);

/// Semua route modul ini (path penuh).
pub fn router(state: &AppState) -> Router<AppState> {
    const M: &str = "/v1/admin/product-masters/{kind}";
    const MI: &str = "/v1/admin/product-masters/{kind}/{id}";
    Router::new()
        .route(M, guard(state, PRODUCT_MASTERS, get(masters::h_find_all)))
        .route(M, guard(state, PRODUCT_MASTERS, post(masters::h_create)))
        .route(MI, guard(state, PRODUCT_MASTERS, get(masters::h_find_one)))
        .route(MI, guard(state, PRODUCT_MASTERS, patch(masters::h_update)))
        .route(MI, guard(state, PRODUCT_MASTERS, delete(masters::h_remove)))
        .merge(custom_fields::routes(state))
        .merge(products::routes(state))
        .merge(images::routes(state))
        .merge(import::routes(state))
        .merge(price_lists::routes(state))
        .merge(mobile_catalog::routes(state))
}
