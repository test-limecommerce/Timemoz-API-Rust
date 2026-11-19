//! Port modul `roles` Mudah-API (docs/custom_roles.md §11 F9–F11, §F16): 15 route.
//!
//! - `/v1/admin/roles*` (11) — `roles.controller.ts` + `roles.service.ts`
//! - `GET /v1/admin/permissions/{features,templates}` — `permissions-admin.controller.ts`
//! - `GET /v1/roles/assignable` — `assignable-roles.controller.ts`
//! - `GET /v1/me/permissions` — `permissions/me-permissions.controller.ts`
//!
//! Setiap perubahan role/permission: invalidasi cache Rust + pesan protokol `role`
//! (`plan/cache-invalidation-protocol.md`), persis `PermissionsService.invalidateRole` Node.

pub mod dto;
pub mod entity;
pub mod error;
pub mod escalation;
#[cfg(test)]
mod golden;
pub mod handlers;
pub mod preview;
pub mod reach;
pub mod role_code;
pub mod service;

use axum::Router;
use axum::routing::{delete, get, patch, post, put};
use mz_core::enums::Role;
use mz_core::gate::guard;
use mz_core::permissions::Exempt;
use mz_core::{AppState, RouteSpec};

/// `@Roles(SYSTEM_ADMIN, SUPER_ADMIN, ADMIN)` + `@RequireFeature('roles')` (class-level,
/// `RolesController` & `PermissionsAdminController`). `@SkipThrottle`: tanpa limiter.
const ADMIN_ROLES: RouteSpec = RouteSpec::jwt()
    .roles(&[Role::SYSTEM_ADMIN, Role::SUPER_ADMIN, Role::ADMIN])
    .feature("roles");

/// `AssignableRolesController`: `@Roles(.., MANAGER)` + `@RequireFeature('users')`.
const ASSIGNABLE: RouteSpec = RouteSpec::jwt()
    .roles(&[
        Role::SYSTEM_ADMIN,
        Role::SUPER_ADMIN,
        Role::ADMIN,
        Role::MANAGER,
    ])
    .feature("users");

/// `MePermissionsController`: `JwtAuthGuard` saja + `@PermissionExempt('self-service')`
/// (class-level `@RequireFeature('roles')` selalu dikalahkan exempt per-handler).
const ME: RouteSpec = RouteSpec::jwt()
    .feature("roles")
    .exempt(Exempt::SelfService);

pub fn router(state: &AppState) -> Router<AppState> {
    use handlers as h;
    let g = |spec, route| guard(state, spec, route);
    Router::new()
        .route("/v1/admin/roles", g(ADMIN_ROLES, get(h::list)))
        .route("/v1/admin/roles", g(ADMIN_ROLES, post(h::create)))
        .route("/v1/admin/roles/preview", g(ADMIN_ROLES, post(h::preview)))
        // Method lain di path statis `preview` = `:uuid` di Nest → ParseUUIDPipe 400.
        .route(
            "/v1/admin/roles/preview",
            g(
                ADMIN_ROLES,
                get(h::preview_as_uuid)
                    .patch(h::preview_as_uuid)
                    .delete(h::preview_as_uuid),
            ),
        )
        .route("/v1/admin/roles/{uuid}", g(ADMIN_ROLES, get(h::find_one)))
        .route("/v1/admin/roles/{uuid}", g(ADMIN_ROLES, patch(h::update)))
        .route("/v1/admin/roles/{uuid}", g(ADMIN_ROLES, delete(h::remove)))
        .route(
            "/v1/admin/roles/{uuid}/permissions",
            g(ADMIN_ROLES, put(h::replace_permissions)),
        )
        .route(
            "/v1/admin/roles/{uuid}/customize",
            g(ADMIN_ROLES, put(h::customize)),
        )
        .route(
            "/v1/admin/roles/{uuid}/revert",
            g(ADMIN_ROLES, post(h::revert)),
        )
        .route(
            "/v1/admin/roles/{uuid}/clone",
            g(ADMIN_ROLES, post(h::clone)),
        )
        .route(
            "/v1/admin/roles/{uuid}/impact",
            g(ADMIN_ROLES, get(h::impact)),
        )
        .route(
            "/v1/admin/permissions/features",
            g(ADMIN_ROLES, get(h::features)),
        )
        .route(
            "/v1/admin/permissions/templates",
            g(ADMIN_ROLES, get(h::templates)),
        )
        .route("/v1/roles/assignable", g(ASSIGNABLE, get(h::assignable)))
        .route("/v1/me/permissions", g(ME, get(h::me_permissions)))
}
