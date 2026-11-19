//! Port `src/settings` (Mudah-API): 17 route `SettingsController` (`/settings*`,
//! class-level `@RequireFeature('company-settings')` + `@SkipThrottle()`).
//!
//! * CRUD `setting` (SYSTEM_ADMIN), endpoint publik `latest-version`, `latest-version-mobile`,
//!   `prohibited-apps`, `pricing-lists`, webhook pembayaran Midtrans,
//! * `default-manual-time-approvers` (3 route), ClickUp per company (6 route).
//!
//! Manual-time-settings/approvers (`mz-timeconfig`) dan klien/OAuth ClickUp (`mz-int-clickup`)
//! adalah route lain; di sini hanya yang dipanggil `SettingsService`.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic
    )
)]

use axum::Router;
use axum::routing::{delete, get, patch, post};
use mz_core::enums::Role;
use mz_core::gate::guard;
use mz_core::{AppState, RouteSpec};

mod approvers;
mod clickup;
mod crud;
mod dto;
mod public;
pub mod webhook;

const SYS: &[Role] = &[Role::SYSTEM_ADMIN];
const ADMINS: &[Role] = &[Role::SYSTEM_ADMIN, Role::SUPER_ADMIN, Role::ADMIN];

/// `@Roles(SYSTEM_ADMIN)` + class-level `@RequireFeature('company-settings')`.
const SYS_SPEC: RouteSpec = RouteSpec::jwt().roles(SYS).feature("company-settings");
/// `@Roles(SYSTEM_ADMIN, SUPER_ADMIN, ADMIN)`.
const ADMIN_SPEC: RouteSpec = RouteSpec::jwt().roles(ADMINS).feature("company-settings");
/// Tanpa guard JWT (class-level `@RequireFeature` tetap tercatat; tanpa user tidak menggerbang).
const PUBLIC_SPEC: RouteSpec = RouteSpec::public().feature("company-settings");

/// Router seluruh route `/settings*` (path penuh, tanpa prefix).
///
/// Nest mencocokkan `PATCH|DELETE /settings/:uuid` untuk path statis satu segmen
/// (`/settings/prohibited-apps` dst.) karena route `:uuid` ikut terdaftar; axum memberi
/// 405 (jadi 404 `Cannot PATCH ...` di `mz-server`). `tail_routes` meniru perilaku Nest.
pub fn router(state: &AppState) -> Router<AppState> {
    let mut r = Router::new()
        .route("/settings", guard(state, SYS_SPEC, get(crud::find_all)))
        .route("/settings", guard(state, SYS_SPEC, post(crud::create)))
        .route(
            "/settings/{uuid}",
            guard(state, SYS_SPEC, patch(crud::update)),
        )
        .route(
            "/settings/{uuid}",
            guard(state, SYS_SPEC, delete(crud::delete)),
        )
        .route(
            "/settings/latest-version",
            guard(state, PUBLIC_SPEC, get(public::latest_version)),
        )
        .route(
            "/settings/latest-version-mobile",
            guard(state, PUBLIC_SPEC, get(public::latest_version_mobile)),
        )
        .route(
            "/settings/prohibited-apps",
            guard(state, PUBLIC_SPEC, get(public::prohibited_apps)),
        )
        .route(
            "/settings/pricing-lists",
            guard(state, PUBLIC_SPEC, get(public::pricing_lists)),
        )
        .route(
            "/settings/webhook/payments",
            guard(state, PUBLIC_SPEC, post(webhook::midtrans_webhook)),
        )
        .route(
            "/settings/default-manual-time-approvers",
            guard(state, ADMIN_SPEC, get(approvers::list)),
        )
        .route(
            "/settings/default-manual-time-approvers",
            guard(state, ADMIN_SPEC, post(approvers::add)),
        )
        .route(
            "/settings/default-manual-time-approvers/{uuid}",
            guard(state, ADMIN_SPEC, delete(approvers::remove)),
        )
        .route(
            "/settings/company/clickup-token",
            guard(state, ADMIN_SPEC, post(clickup::save_token)),
        )
        .route(
            "/settings/company/clickup-token",
            guard(state, ADMIN_SPEC, get(clickup::get_token)),
        )
        .route(
            "/settings/company/clickup",
            guard(state, ADMIN_SPEC, delete(clickup::disconnect)),
        )
        .route(
            "/settings/company/clickup-workspaces",
            guard(state, ADMIN_SPEC, get(clickup::workspaces)),
        )
        .route(
            "/settings/company/clickup-workspace",
            guard(state, ADMIN_SPEC, post(clickup::save_workspace)),
        );
    for path in [
        "/settings/latest-version",
        "/settings/latest-version-mobile",
        "/settings/prohibited-apps",
        "/settings/pricing-lists",
        "/settings/default-manual-time-approvers",
    ] {
        r = r
            .route(path, guard(state, SYS_SPEC, patch(crud::update_tail)))
            .route(path, guard(state, SYS_SPEC, delete(crud::delete_tail)));
    }
    r
}
