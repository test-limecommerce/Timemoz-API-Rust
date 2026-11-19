//! Port `src/users` dari Mudah-API (28 route `UsersController`; brief menyebut 29, controller hanya punya 28 handler).
#![cfg_attr(test, allow(clippy::indexing_slicing))]

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::routing::{delete, get, patch, post, put};
use mz_core::enums::{PermissionAction, Role};
use mz_core::gate::guard;
use mz_core::permissions::Exempt;
use mz_core::rate_limit::rules;
use mz_core::{AppState, RouteSpec};

pub mod dto;
pub mod handlers;
pub mod model;
pub mod service;

use handlers::{account, devices, invite, local_db, rates};

/// `@Roles(SYSTEM_ADMIN, SUPER_ADMIN, ADMIN, MANAGER, STAFF, GUEST)`.
const ALL_ROLES: &[Role] = &[
    Role::SYSTEM_ADMIN,
    Role::SUPER_ADMIN,
    Role::ADMIN,
    Role::MANAGER,
    Role::STAFF,
    Role::GUEST,
];
/// `@Roles(SUPER_ADMIN, SYSTEM_ADMIN, ADMIN)`.
const ADMINS: &[Role] = &[Role::SUPER_ADMIN, Role::SYSTEM_ADMIN, Role::ADMIN];
/// `@Roles(SUPER_ADMIN, SYSTEM_ADMIN, ADMIN, MANAGER)`.
const MANAGERS: &[Role] = &[
    Role::SUPER_ADMIN,
    Role::SYSTEM_ADMIN,
    Role::ADMIN,
    Role::MANAGER,
];

/// `@RequireFeature('users')` (level class) + `@UseGuards(JwtAuthGuard)`.
const BASE: RouteSpec = RouteSpec::jwt().feature("users");
/// `@UseGuards(JwtAuthGuard, RolesGuard)` + `@Roles(...)`.
const ADMIN_ONLY: RouteSpec = BASE.roles(ADMINS);
const MANAGER_UP: RouteSpec = BASE.roles(MANAGERS);
/// `@PermissionExempt('self-service')`.
const SELF: RouteSpec = BASE.exempt(Exempt::SelfService);
/// `delete-account`: POST tetapi soft-delete akun (`@RequirePermission(DELETE)`, §5.3a) + limiter `main.ts`.
const DELETE_ACCOUNT: RouteSpec = BASE
    .roles(ALL_ROLES)
    .action(PermissionAction::DELETE)
    .rate_limits(&[rules::USERS_DELETE_ACCOUNT]);
/// Logout desktop & halaman pilih paket tetap terbuka saat company terkunci.
const SELF_LOCKED_OK: RouteSpec = SELF.allow_locked();

/// `express.json({ limit: '50mb' })` untuk `/users/local-db`.
const LOCAL_DB_BODY_LIMIT: usize = 50 * 1024 * 1024;
/// `express.json({ limit: '10mb' })` untuk `/users/log`.
const LOG_BODY_LIMIT: usize = 10 * 1024 * 1024;

/// Urutan route Nest penting untuk path yang bentrok (`/users/:uuid` vs `/users/log`, dst).
/// Axum memilih segmen statis lebih dulu, jadi `me`, `local-db`, `log`, `connected`, `check`,
/// `revoke`, `invite`, `delete-account` tidak pernah tertangkap sebagai `{uuid}`.
pub fn router(state: &AppState) -> Router<AppState> {
    Router::new()
        // Didaftarkan sebelum route ber-param supaya 'delete-account' tidak tertangkap sebagai :uuid.
        .route(
            "/users/delete-account",
            guard(state, DELETE_ACCOUNT, post(account::delete_account)),
        )
        .route(
            "/users/{uuid}/disable",
            guard(state, ADMIN_ONLY, post(account::disable)),
        )
        .route(
            "/users/{uuid}/enable",
            guard(state, ADMIN_ONLY, post(account::enable)),
        )
        .route(
            "/users/invite",
            guard(state, MANAGER_UP, post(invite::invite)),
        )
        .route("/users", guard(state, BASE, get(account::find_all)))
        .route(
            "/users/me/product-tour",
            guard(state, SELF, get(account::get_product_tour)),
        )
        .route(
            "/users/me/product-tour",
            guard(state, SELF, patch(account::update_product_tour)),
        )
        // LOCAL DB & LOG harus mengalahkan `GET /users/:uuid`.
        .route(
            "/users/local-db",
            guard(
                state,
                SELF,
                post(local_db::upsert_local_db).layer(DefaultBodyLimit::max(LOCAL_DB_BODY_LIMIT)),
            ),
        )
        .route(
            "/users/local-db",
            guard(state, SELF, get(local_db::get_local_db)),
        )
        .route(
            "/users/log",
            guard(
                state,
                SELF,
                post(local_db::create_log).layer(DefaultBodyLimit::max(LOG_BODY_LIMIT)),
            ),
        )
        .route("/users/log", guard(state, SELF, get(local_db::get_logs)))
        .route("/users/{uuid}", guard(state, BASE, get(account::find_one)))
        .route(
            "/users/{uuid}",
            guard(state, MANAGER_UP, patch(account::update)),
        )
        .route(
            "/users/{uuid}",
            guard(state, ADMIN_ONLY, delete(account::remove)),
        )
        .route(
            "/users/revoke/{macAddress}",
            guard(state, SELF_LOCKED_OK, delete(devices::remove_connected)),
        )
        .route(
            "/users/invite/batch",
            guard(state, MANAGER_UP, post(invite::invite_batch)),
        )
        .route(
            "/users/connected/devices",
            guard(state, SELF, get(devices::find_connected_device)),
        )
        .route(
            "/users/connected/devices/revoke/{id}",
            guard(state, SELF, delete(devices::revoke_connected_device)),
        )
        .route(
            "/users/check/subscription",
            guard(state, SELF_LOCKED_OK, get(account::check_subscription)),
        )
        // ==================== PAY RATES ====================
        .route(
            "/users/{uuid}/pay-rates",
            guard(state, ADMIN_ONLY, get(rates::get_pay_rates)),
        )
        .route(
            "/users/{uuid}/pay-rates",
            guard(state, ADMIN_ONLY, post(rates::create_pay_rate)),
        )
        .route(
            "/users/{uuid}/pay-rates/{rateUuid}",
            guard(state, ADMIN_ONLY, patch(rates::update_pay_rate)),
        )
        .route(
            "/users/{uuid}/pay-rates/{rateUuid}",
            guard(state, ADMIN_ONLY, delete(rates::delete_pay_rate)),
        )
        // ==================== PROJECT RATES ====================
        .route(
            "/users/{uuid}/project-rates",
            guard(state, ADMIN_ONLY, get(rates::get_project_rates)),
        )
        .route(
            "/users/{uuid}/project-rates/{projectUuid}",
            guard(state, ADMIN_ONLY, patch(rates::set_project_rate)),
        )
        // ==================== WORK TIME LIMITS ====================
        .route(
            "/users/{uuid}/work-time-limit",
            guard(state, ADMIN_ONLY, get(rates::get_work_time_limit)),
        )
        .route(
            "/users/{uuid}/work-time-limit",
            guard(state, ADMIN_ONLY, put(rates::upsert_work_time_limit)),
        )
        .route(
            "/users/{uuid}/work-time-limit",
            guard(state, ADMIN_ONLY, delete(rates::remove_work_time_limit)),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode};
    use tower::ServiceExt;

    /// Semua 28 route controller (method, path contoh).
    const ROUTES: [(&str, &str); 28] = [
        ("POST", "/users/delete-account"),
        (
            "POST",
            "/users/123e4567-e89b-12d3-a456-426614174000/disable",
        ),
        ("POST", "/users/123e4567-e89b-12d3-a456-426614174000/enable"),
        ("POST", "/users/invite"),
        ("GET", "/users"),
        ("GET", "/users/me/product-tour"),
        ("PATCH", "/users/me/product-tour"),
        ("POST", "/users/local-db"),
        ("GET", "/users/local-db"),
        ("POST", "/users/log"),
        ("GET", "/users/log"),
        ("GET", "/users/123e4567-e89b-12d3-a456-426614174000"),
        ("PATCH", "/users/123e4567-e89b-12d3-a456-426614174000"),
        ("DELETE", "/users/123e4567-e89b-12d3-a456-426614174000"),
        ("DELETE", "/users/revoke/12:12:12:12:12"),
        ("POST", "/users/invite/batch"),
        ("GET", "/users/connected/devices"),
        ("DELETE", "/users/connected/devices/revoke/1"),
        ("GET", "/users/check/subscription"),
        (
            "GET",
            "/users/123e4567-e89b-12d3-a456-426614174000/pay-rates",
        ),
        (
            "POST",
            "/users/123e4567-e89b-12d3-a456-426614174000/pay-rates",
        ),
        (
            "PATCH",
            "/users/123e4567-e89b-12d3-a456-426614174000/pay-rates/123e4567-e89b-12d3-a456-426614174001",
        ),
        (
            "DELETE",
            "/users/123e4567-e89b-12d3-a456-426614174000/pay-rates/123e4567-e89b-12d3-a456-426614174001",
        ),
        (
            "GET",
            "/users/123e4567-e89b-12d3-a456-426614174000/project-rates",
        ),
        (
            "PATCH",
            "/users/123e4567-e89b-12d3-a456-426614174000/project-rates/123e4567-e89b-12d3-a456-426614174001",
        ),
        (
            "GET",
            "/users/123e4567-e89b-12d3-a456-426614174000/work-time-limit",
        ),
        (
            "PUT",
            "/users/123e4567-e89b-12d3-a456-426614174000/work-time-limit",
        ),
        (
            "DELETE",
            "/users/123e4567-e89b-12d3-a456-426614174000/work-time-limit",
        ),
    ];

    /// Setiap route cocok dengan handler-nya (bukan 404/405) dan ditolak gerbang JWT (401)
    /// SEBELUM menyentuh DB — membuktikan path yang bentrok tidak saling menimpa.
    #[tokio::test]
    async fn every_route_resolves_and_is_jwt_guarded() {
        let Ok(state) = AppState::for_test() else {
            return assert_eq!(0, 1);
        };
        for (method, path) in ROUTES {
            let app = router(&state).with_state(state.clone());
            let req = Request::builder()
                .method(Method::from_bytes(method.as_bytes()).unwrap_or(Method::GET))
                .uri(path)
                .body(Body::empty());
            let Ok(req) = req else {
                return assert_eq!(0, 1);
            };
            let res = app.oneshot(req).await.unwrap_or_default();
            assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "{method} {path}");
        }
    }

    /// Segmen statis (`me`, `log`, ...) tidak jatuh ke `{uuid}` pada method yang tidak terdaftar:
    /// `GET /users/me` tetap tertangkap `GET /users/{uuid}` (Node: 400 uuid), bukan 404.
    #[tokio::test]
    async fn non_uuid_segment_falls_back_to_param_route() {
        let Ok(state) = AppState::for_test() else {
            return assert_eq!(0, 1);
        };
        let app = router(&state).with_state(state.clone());
        let Ok(req) = Request::builder().uri("/users/me").body(Body::empty()) else {
            return assert_eq!(0, 1);
        };
        let res = app.oneshot(req).await.unwrap_or_default();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }
}
