//! Port `src/admin-company/**` Mudah-API (28 route operator Timemoz):
//! `admin-company.controller` (8), `features` (3), `lifecycle` + `invite-codes-admin` (4),
//! `manual-subscription` (7), `ownership` (1), `plan-override` (3), `provisioning` (2).
//!
//! Semua controller ber-`@PermissionExempt('separate-auth')` + `@AllowWhenSubscriptionLocked()`:
//! dipasang `RouteSpec::public()` dan diautentikasi extractor `mz_operator::Operator`
//! (`JwtAdminAuthGuard`); `OperatorRolesGuard` = `Operator::require` di handler.
//! Deviasi: `plan/deviations/mz-admin-company.md`.

#![cfg_attr(test, allow(clippy::indexing_slicing, clippy::panic))]

pub mod common;
pub mod company;
pub mod features;
pub mod lifecycle;
pub mod manual;
pub mod ownership;
pub mod plan_limits;
pub mod plan_override;
pub mod provisioning;
pub mod val;

use axum::Router;
use axum::routing::{delete, get, patch, post, put};
use mz_core::AppState;
use mz_core::gate::guard;

use crate::common::SPEC;

/// Router seluruh route crate ini (path penuh, tanpa prefix).
pub fn router(state: &AppState) -> Router<AppState> {
    let g = |m| guard(state, SPEC, m);
    Router::new()
        // ───── admin-company.controller ─────
        .route("/admin-company", g(get(company::h_list)))
        .route("/admin-company", g(post(provisioning::h_create_company)))
        .route("/admin-company/user/{uuid}", g(get(company::h_find_user)))
        .route("/admin-company/{uuid}", g(get(company::h_find_one)))
        .route(
            "/admin-company/set/subscriptions/{uuid}",
            g(patch(company::h_set_subscription)),
        )
        .route(
            "/admin-company/set/offline-mode/{uuid}",
            g(patch(company::h_offline_mode)),
        )
        .route(
            "/admin-company/set/activity-track/{uuid}",
            g(patch(company::h_activity_track)),
        )
        .route(
            "/admin-company/set/plan-boost/{uuid}",
            g(patch(company::h_plan_boost)),
        )
        .route(
            "/admin-company/set/trial/{uuid}",
            g(patch(company::h_set_trial)),
        )
        // ───── features ─────
        .route("/admin-company/{uuid}/features", g(get(features::h_get)))
        .route(
            "/admin-company/{uuid}/features",
            g(patch(features::h_patch)),
        )
        .route(
            "/admin-company/{uuid}/report-exports/reset",
            g(post(features::h_reset_exports)),
        )
        // ───── lifecycle + invite-codes-admin ─────
        .route("/admin-company/{uuid}/delete", g(post(lifecycle::h_delete)))
        .route(
            "/admin-company/{uuid}/restore",
            g(post(lifecycle::h_restore)),
        )
        .route(
            "/admin-company/{uuid}/invite-code/regenerate",
            g(post(lifecycle::h_regenerate)),
        )
        .route(
            "/admin/invite-codes/fill-missing",
            g(post(lifecycle::h_fill_missing)),
        )
        // ───── manual-subscription ─────
        .route(
            "/admin-company/{uuid}/subscription",
            g(get(manual::h_summary)),
        )
        .route(
            "/admin-company/{uuid}/subscription/manual/activate",
            g(post(manual::h_activate)),
        )
        .route(
            "/admin-company/{uuid}/subscription/manual/extend",
            g(post(manual::h_extend)),
        )
        .route(
            "/admin-company/{uuid}/subscription/manual/contract",
            g(put(manual::h_contract)),
        )
        .route(
            "/admin-company/{uuid}/subscription/manual/invoices",
            g(post(manual::h_create_invoice)),
        )
        .route(
            "/admin-company/{uuid}/subscription/manual/invoices/{invoice_uuid}/pay",
            g(post(manual::h_pay_invoice)),
        )
        .route(
            "/admin-company/{uuid}/subscription/manual/stop",
            g(post(manual::h_stop)),
        )
        // ───── ownership ─────
        .route(
            "/admin-company/{uuid}/transfer-owner",
            g(post(ownership::h_transfer)),
        )
        // ───── plan-override ─────
        .route(
            "/admin-company/{uuid}/plan-override",
            g(get(plan_override::h_get)),
        )
        .route(
            "/admin-company/{uuid}/plan-override",
            g(put(plan_override::h_put)),
        )
        .route(
            "/admin-company/{uuid}/plan-override",
            g(delete(plan_override::h_delete)),
        )
        // ───── provisioning ─────
        .route(
            "/admin-company/{uuid}/users",
            g(post(provisioning::h_create_user)),
        )
}
