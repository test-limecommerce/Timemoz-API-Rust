//! Port `src/paypal/paypal.controller.ts` (3 route, termasuk webhook),
//! `proration-payment.service.ts`, dan `paypal-admin.controller.ts` (15 route operator).
//! Klien PayPal: `mz-paypal` (`MZ_PAYPAL_API_BASE` mengalihkan ke server tiruan saat uji).
//!
//! API publik untuk crate lain (tahap B):
//! - [`proration`]: `capture_and_apply`, `apply_completed_payment`, `is_paypal_order_id`,
//!   `proration_return_status`, `Outcome`, `ProrationResult`.
//! - [`webhook::process_event`]: pemrosesan satu event PayPal (sesudah verifikasi tanda tangan).
//! - [`router`].

pub mod admin;
mod mail;
pub mod proration;
mod rows;
mod status;
mod util;
pub mod webhook;

use axum::Router;
use axum::routing::{get, post};
use mz_core::AppState;
use mz_core::gate::{RouteSpec, guard};
use mz_core::permissions::Exempt;

/// `@UseGuards(JwtAuthGuard) @PermissionExempt('self-service')` + `@AllowWhenSubscriptionLocked`.
const STATUS: RouteSpec = RouteSpec::jwt().exempt(Exempt::SelfService).allow_locked();
/// `@PermissionExempt('no-user')` (class-level) — tanpa JWT.
const PUBLIC: RouteSpec = RouteSpec::public().exempt(Exempt::NoUser).allow_locked();
/// `PaypalAdminController`: `JwtAuthGuard` class-level + `JwtAdminAuthGuard` (extractor
/// `Operator`) per handler, `@PermissionExempt('separate-auth')`.
const ADMIN: RouteSpec = RouteSpec::jwt().exempt(Exempt::SeparateAuth).allow_locked();

pub fn router(state: &AppState) -> Router<AppState> {
    let a = |r| guard(state, ADMIN, r);
    Router::new()
        .route(
            "/paypal/subscription/status",
            guard(state, STATUS, get(status::subscription_status)),
        )
        .route(
            "/paypal/proration/return",
            guard(state, PUBLIC, get(status::proration_return)),
        )
        .route(
            "/paypal/webhook",
            guard(state, PUBLIC, post(webhook::handle_webhook)),
        )
        .route("/admin/paypal/products", a(post(admin::create_product)))
        .route(
            "/admin/paypal/products/{productId}",
            a(get(admin::get_product)),
        )
        .route(
            "/admin/paypal/plans",
            a(post(admin::create_plan).get(admin::list_plans)),
        )
        .route(
            "/admin/paypal/plans/quantity-based",
            a(post(admin::create_quantity_based_plan).get(admin::get_plan_quantity_based)),
        )
        .route(
            "/admin/paypal/plans/link",
            a(post(admin::link_plan).get(admin::get_plan_link)),
        )
        .route("/admin/paypal/plans/{planId}", a(get(admin::get_plan)))
        .route(
            "/admin/paypal/plans/{planId}/update-name",
            a(post(admin::update_plan_name)),
        )
        .route(
            "/admin/paypal/plans/{planId}/update-price",
            a(post(admin::update_plan_price)),
        )
        .route(
            "/admin/paypal/plans/{planId}/deactivate",
            a(post(admin::deactivate_plan)),
        )
        .route(
            "/admin/paypal/subscriptions/{subscriptionId}",
            a(get(admin::get_subscription)),
        )
        .route(
            "/admin/paypal/subscriptions/{subscriptionId}/cancel",
            a(post(admin::cancel_subscription)),
        )
        .route(
            "/admin/paypal/subscriptions/{subscriptionId}/suspend",
            a(post(admin::suspend_subscription)),
        )
        .route(
            "/admin/paypal/subscriptions/{subscriptionId}/activate",
            a(post(admin::activate_subscription)),
        )
        .route(
            "/admin/paypal/subscriptions/{subscriptionId}/update-quantity",
            a(post(admin::update_subscription_quantity)),
        )
}
