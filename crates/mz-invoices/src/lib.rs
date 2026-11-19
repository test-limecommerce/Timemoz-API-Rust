//! Port `src/invoices/invoices.controller.ts` + sisa `invoices.service.ts` (Mudah-API) — tahap B
//! port billing (`plan/BILLING_AGENT_BRIEF.md`).
//!
//! Route (`router`): `GET /invoices`, `GET /invoices/{uuid}`, `PUT /invoices/renew/{uuid}`,
//! `POST /invoices/{change-plan,change-active-plan,subscribe,cancel-subscription,
//! resume-subscription,reactivate-subscription}`, `GET /invoices/download/pdf/{uuid}`,
//! `GET /invoices/{uuid}/pdf`.
//!
//! Fondasi: `mz-billing` (nomor invoice, PDF, free tier, `assertNotManualBilling`, akses invoice),
//! `mz-paypal` (klien PayPal), Midtrans payment link di [`midtrans`] (hanya `renewPayment` lama).
//! Deviasi: `plan/deviations/mz-invoices.md`.

#![cfg_attr(
    test,
    allow(
        clippy::indexing_slicing,
        clippy::panic,
        clippy::unwrap_used,
        clippy::expect_used
    )
)]

use axum::Router;
use axum::routing::{get, post, put};
use mz_core::enums::Role;
use mz_core::gate::guard;
use mz_core::{AppState, RouteSpec};

mod db;
pub mod lifecycle;
pub mod list;
pub mod load;
pub mod mail;
pub mod midtrans;
pub mod paypal;
pub mod plan;
pub mod renew;
pub mod routes;
mod rows;
pub mod util;

const ADMINS: &[Role] = &[Role::ADMIN, Role::SUPER_ADMIN, Role::SYSTEM_ADMIN];
const PDF_ROLES: &[Role] = &[
    Role::SYSTEM_ADMIN,
    Role::SUPER_ADMIN,
    Role::ADMIN,
    Role::MANAGER,
];

/// `@Roles(ADMIN, SUPER_ADMIN, SYSTEM_ADMIN)` + class-level `@RequireFeature('billing')` +
/// `@AllowWhenSubscriptionLocked()`.
const SPEC: RouteSpec = RouteSpec::jwt()
    .roles(ADMINS)
    .feature("billing")
    .allow_locked();
/// `GET /invoices/:uuid/pdf`: `@Roles(SYSTEM_ADMIN, SUPER_ADMIN, ADMIN, MANAGER)`.
/// Limiter `main.ts:359-368` `invoicePdfLimiter`: 30/menit per token, `rl:invoice-pdf`.
const PDF_LIMIT: mz_core::rate_limit::RateRule =
    mz_core::rate_limit::RateRule::token("rl:invoice-pdf", 30, 60_000);
const PDF_SPEC: RouteSpec = RouteSpec::jwt()
    .roles(PDF_ROLES)
    .feature("billing")
    .allow_locked()
    .rate_limits(&[PDF_LIMIT]);

/// Router seluruh route `/invoices*` (path penuh, tanpa prefix).
pub fn router(state: &AppState) -> Router<AppState> {
    let g = |spec, m| guard(state, spec, m);
    Router::new()
        .route("/invoices", g(SPEC, get(routes::h_find_all)))
        .route("/invoices/{uuid}", g(SPEC, get(routes::h_find_one)))
        .route("/invoices/renew/{uuid}", g(SPEC, put(routes::h_renew)))
        .route(
            "/invoices/change-plan",
            g(SPEC, post(routes::h_change_plan)),
        )
        .route(
            "/invoices/change-active-plan",
            g(SPEC, post(routes::h_change_active_plan)),
        )
        .route("/invoices/subscribe", g(SPEC, post(routes::h_subscribe)))
        .route(
            "/invoices/cancel-subscription",
            g(SPEC, post(routes::h_cancel)),
        )
        .route(
            "/invoices/resume-subscription",
            g(SPEC, post(routes::h_resume)),
        )
        .route(
            "/invoices/reactivate-subscription",
            g(SPEC, post(routes::h_reactivate)),
        )
        .route(
            "/invoices/download/pdf/{uuid}",
            g(SPEC, get(routes::h_download_pdf)),
        )
        .route("/invoices/{uuid}/pdf", g(PDF_SPEC, get(routes::h_pdf)))
        // `GET` ke path POST statis satu segmen: Nest jatuh ke `GET /invoices/:uuid` (400 sesudah guard).
        .route(
            "/invoices/change-plan",
            g(SPEC, get(routes::h_find_one_static)),
        )
        .route(
            "/invoices/change-active-plan",
            g(SPEC, get(routes::h_find_one_static)),
        )
        .route(
            "/invoices/subscribe",
            g(SPEC, get(routes::h_find_one_static)),
        )
        .route(
            "/invoices/cancel-subscription",
            g(SPEC, get(routes::h_find_one_static)),
        )
        .route(
            "/invoices/resume-subscription",
            g(SPEC, get(routes::h_find_one_static)),
        )
        .route(
            "/invoices/reactivate-subscription",
            g(SPEC, get(routes::h_find_one_static)),
        )
}
