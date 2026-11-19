//! Port modul order sales tracker (tahap S3): `src/orders/**` + bagian order
//! `src/custom-fields/**`.
//!
//! Route (38): admin orders 12 (termasuk `GET /v1/admin/orders/{id}/invoice-pdf`), mobile orders 5
//! (termasuk `POST /v1/mobile/store-visits/{visit_uuid}/no-order`), admin order deliveries 3,
//! mobile order deliveries 4, admin order approvals 3, admin order masters 5, admin order custom
//! fields 5, admin store-visit no-order 1. Tanpa cron. Deviasi: `plan/deviations/mz-orders.md`.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic
    )
)]

pub mod censor;
pub mod common;
pub mod config;
pub mod cursor;
pub mod custom_fields;
pub mod deliveries;
pub mod dto;
pub mod entity;
pub mod export;
pub mod handlers;
pub mod invoice_pdf;
pub mod masters;
pub mod mobile;
pub mod number;
pub mod pricing;
pub mod service;
pub mod transitions;

use axum::Router;
use mz_core::enums::{FeatureModule, Role};
use mz_core::{AppState, RouteSpec};

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

/// `AdminOrdersController`, `AdminOrderCustomFieldsController`, `AdminStoreVisitNoOrderController`
/// (`@RequireFeature('orders')`).
pub(crate) const ORDERS: RouteSpec = RouteSpec::jwt()
    .feature("orders")
    .modules(SALES)
    .roles(ADMIN_ROLES);
/// `MobileOrdersController` (`orders`, + STAFF).
pub(crate) const MOBILE_ORDERS: RouteSpec = RouteSpec::jwt()
    .feature("orders")
    .modules(SALES)
    .roles(MOBILE_ROLES);
/// `AdminOrderMastersController` (`order-masters`).
pub(crate) const ORDER_MASTERS: RouteSpec = RouteSpec::jwt()
    .feature("order-masters")
    .modules(SALES)
    .roles(ADMIN_ROLES);
/// `AdminOrderApprovalsController` (`order-approvals`).
pub(crate) const ORDER_APPROVALS: RouteSpec = RouteSpec::jwt()
    .feature("order-approvals")
    .modules(SALES)
    .roles(ADMIN_ROLES);
/// `AdminOrderDeliveriesController` (`order-deliveries`).
pub(crate) const ORDER_DELIVERIES: RouteSpec = RouteSpec::jwt()
    .feature("order-deliveries")
    .modules(SALES)
    .roles(ADMIN_ROLES);
/// `MobileOrderDeliveriesController` (`order-deliveries`, + STAFF).
pub(crate) const MOBILE_ORDER_DELIVERIES: RouteSpec = RouteSpec::jwt()
    .feature("order-deliveries")
    .modules(SALES)
    .roles(MOBILE_ROLES);

/// Semua route modul ini (path penuh).
pub fn router(state: &AppState) -> Router<AppState> {
    Router::new()
        .merge(masters::routes(state))
        .merge(custom_fields::routes(state))
        .merge(handlers::routes(state))
}
