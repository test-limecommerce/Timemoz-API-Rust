//! Port `src/table-fields` (Mudah-API, "table field visibility"): 6 route
//! `TableFieldsController` (`/v1/admin/table-fields/*`) + fungsi kebijakan `pub` untuk route
//! sales-tracker yang belum di-port.
//!
//! Route: `GET tables`, `GET|PUT|DELETE matrix`, `POST reveal`, `GET reveal-logs`.
//!
//! API `pub` untuk modul lain (TIDAK dipasang ke route mana pun di crate ini):
//! * [`registry`] — `TABLES`, `column_of`, `configurable_columns`, `FieldVisibilityMode`;
//! * [`policy`] — [`policy::resolve`] / [`policy::censored_columns`] (cache 30 dtk per proses),
//!   [`policy::invalidate_company`] (kirim pesan `table-field-policy` ke Node);
//! * [`apply`] — `apply_field_policy`, `censor_response_body`, `field_policy_envelope`
//!   (interceptor `@TableFields`), `TableFieldsTarget`;
//! * [`search_sort`] — `allowed_search_branches`, `assert_sortable_column` (§6.4);
//! * [`quota`] / [`reveal`] — kuota reveal harian, `RevealResolver` + `register_resolver`.
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
use axum::routing::{delete, get, post, put};
use mz_core::enums::{PermissionAction, Role};
use mz_core::gate::guard;
use mz_core::{AppState, RouteSpec};

pub mod apply;
mod config;
mod dto;
pub mod error;
pub mod policy;
pub mod quota;
pub mod registry;
pub mod reveal;
pub mod search_sort;

const ADMINS: &[Role] = &[Role::SYSTEM_ADMIN, Role::SUPER_ADMIN, Role::ADMIN];
/// `reveal` menambah MANAGER (kolom `revealable` justru diatur UNTUK jabatan di bawah ADMIN).
const REVEAL_ROLES: &[Role] = &[
    Role::SYSTEM_ADMIN,
    Role::SUPER_ADMIN,
    Role::ADMIN,
    Role::MANAGER,
];

/// Class-level: `@RequireFeature('table-fields')` + `@Roles(SYSTEM_ADMIN, SUPER_ADMIN, ADMIN)`.
const BASE: RouteSpec = RouteSpec::jwt().roles(ADMINS).feature("table-fields");
/// `@Roles(…, MANAGER)`, `@RequireFeature('stores')`, `@RequirePermission(READ)`.
const REVEAL: RouteSpec = RouteSpec::jwt()
    .roles(REVEAL_ROLES)
    .feature("stores")
    .action(PermissionAction::READ);

/// Router 6 route (path penuh, tanpa prefix).
pub fn router(state: &AppState) -> Router<AppState> {
    const P: &str = "/v1/admin/table-fields";
    Router::new()
        .route(
            &format!("{P}/tables"),
            guard(state, BASE, get(config::list_tables)),
        )
        .route(
            &format!("{P}/matrix"),
            guard(state, BASE, get(config::get_matrix)),
        )
        .route(
            &format!("{P}/matrix"),
            guard(state, BASE, put(config::put_matrix)),
        )
        .route(
            &format!("{P}/matrix"),
            guard(state, BASE, delete(config::reset_matrix)),
        )
        .route(
            &format!("{P}/reveal"),
            guard(state, REVEAL, post(reveal::reveal)),
        )
        .route(
            &format!("{P}/reveal-logs"),
            guard(state, BASE, get(reveal::list_logs)),
        )
}
