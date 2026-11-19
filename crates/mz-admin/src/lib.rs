//! Port `src/admin/admin.controller.ts` (10 dari 20 route) dan `src/admin/users/**` (8 route).
//!
//! Dipasang di `mz-server` lewat `mz_admin::router(state)`. Gerbang operator = extractor
//! [`mz_operator::Operator`] (`JwtAdminAuthGuard`); semua route `@PermissionExempt('separate-auth')`
//! + `@AllowWhenSubscriptionLocked()`, jadi `RouteSpec` hanya memuat pengecualian itu dan limiter
//!   `main.ts` untuk route pra-login (`no-user`).
//!
//! Route `admin.controller.ts` di crate lain: `POST /admin/sync/trigger` & `manager-summary/test` → `mz-tasks-sync`;
//! `*/force*` billing → `mz-billing-tasks` (tahap B billing). BELUM di-port (wajib sebelum cutover, lihat
//! `plan/PLAN.md` §Fase 3): `DELETE /admin/screenshot/queue`, `POST /admin/screenshot/queue/{pause,resume}`.
//! (`screenshot/compress|purge` sudah ada di `mz-timeconfig`.)

pub mod analytics;
pub mod dto;
pub mod operators;
pub mod users;

use axum::Router;
use axum::routing::{delete, get, patch, post};
use mz_core::gate::guard;
use mz_core::permissions::Exempt;
use mz_core::rate_limit::{RateKey, RateRule, rules};
use mz_core::{AppState, RouteSpec};

const MINUTE: u64 = 60_000;
const HOUR: u64 = 60 * MINUTE;

/// `auth-rate-limits.ts`: `POST /admin/auth/login`.
pub const ADMIN_AUTH_LOGIN: RateRule =
    RateRule::auth("rl:admin-auth-login", 10, MINUTE, RateKey::Ip);
/// `POST /admin/auth/forgot-password/generate` (per IP).
pub const ADMIN_AUTH_FORGOT_GEN: RateRule =
    RateRule::auth("rl:admin-auth-forgot-gen", 5, MINUTE, RateKey::Ip);
/// `POST /admin/auth/forgot-password/generate` (per email).
pub const ADMIN_AUTH_FORGOT_GEN_EMAIL: RateRule =
    RateRule::auth("rl:admin-auth-forgot-gen-email", 5, HOUR, RateKey::Email);

const LOGIN_LIMITS: &[RateRule] = &[ADMIN_AUTH_LOGIN];
const FORGOT_GEN_LIMITS: &[RateRule] = &[ADMIN_AUTH_FORGOT_GEN, ADMIN_AUTH_FORGOT_GEN_EMAIL];
/// `POST /admin/auth/forgot-password` memakai bucket `rl:auth-reset` yang sama dengan route user.
const RESET_LIMITS: &[RateRule] = &[rules::AUTH_RESET];

/// Route pra-login (`@PermissionExempt('no-user')`).
const fn pre_login(limits: &'static [RateRule]) -> RouteSpec {
    RouteSpec::public()
        .exempt(Exempt::NoUser)
        .allow_locked()
        .rate_limits(limits)
}

const LOGIN: RouteSpec = pre_login(LOGIN_LIMITS);
const FORGOT_GENERATE: RouteSpec = pre_login(FORGOT_GEN_LIMITS);
const FORGOT_PASSWORD: RouteSpec = pre_login(RESET_LIMITS);
/// Route ber-`JwtAdminAuthGuard` (`@PermissionExempt('separate-auth')`); tokennya diperiksa extractor
/// [`mz_operator::Operator`] di handler.
const OPERATOR: RouteSpec = RouteSpec::public()
    .exempt(Exempt::SeparateAuth)
    .allow_locked();

pub fn router(state: &AppState) -> Router<AppState> {
    Router::new()
        // ── admin.controller.ts ─────────────────────────────────────────────
        .route(
            "/admin/analytics/users",
            guard(state, OPERATOR, get(analytics::user_analytics)),
        )
        .route(
            "/admin/analytics/live-activity",
            guard(state, OPERATOR, get(analytics::live_activity)),
        )
        .route(
            "/admin/auth/login",
            guard(state, LOGIN, post(operators::login)),
        )
        .route(
            "/admin/auth/forgot-password",
            guard(state, FORGOT_PASSWORD, post(operators::forgot_password)),
        )
        .route(
            "/admin/auth/forgot-password/generate",
            guard(
                state,
                FORGOT_GENERATE,
                post(operators::forgot_password_generate),
            ),
        )
        .route("/admin", guard(state, OPERATOR, get(operators::find_all)))
        .route(
            "/admin/create",
            // `GET /admin/create` jatuh ke `GET /admin/:uuid` di Nest.
            guard(
                state,
                OPERATOR,
                post(operators::create).get(operators::find_one_create),
            ),
        )
        .route(
            "/admin/update/{uuid}",
            guard(state, OPERATOR, patch(operators::update)),
        )
        .route(
            "/admin/delete/{uuid}",
            guard(state, OPERATOR, delete(operators::remove)),
        )
        // `GET /admin/:uuid` terdaftar SESUDAH route statis (Nest: `AdminUsersController` lebih dulu).
        .route(
            "/admin/{uuid}",
            guard(state, OPERATOR, get(operators::find_one)),
        )
        // ── users/admin-users.controller.ts ────────────────────────────────
        .route("/admin/users", guard(state, OPERATOR, get(users::search)))
        .route(
            "/admin/users/{uuid}",
            guard(state, OPERATOR, get(users::detail)),
        )
        .route(
            "/admin/users/{uuid}/send-reset-password",
            guard(state, OPERATOR, post(users::send_reset_password)),
        )
        .route(
            "/admin/users/{uuid}/verify-email",
            guard(state, OPERATOR, post(users::verify_email)),
        )
        .route(
            "/admin/users/{uuid}/email",
            guard(state, OPERATOR, patch(users::change_email)),
        )
        .route(
            "/admin/users/{uuid}/disable",
            guard(state, OPERATOR, post(users::disable)),
        )
        .route(
            "/admin/users/{uuid}/enable",
            guard(state, OPERATOR, post(users::enable)),
        )
        .route(
            "/admin/users/{uuid}/revoke-sessions",
            guard(state, OPERATOR, post(users::revoke_sessions)),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limits_match_auth_rate_limits_ts() {
        assert_eq!(LOGIN.rate_limits.len(), 1);
        assert_eq!(FORGOT_GENERATE.rate_limits.len(), 2);
        assert_eq!(
            FORGOT_PASSWORD.rate_limits.first().map(|r| r.key_prefix),
            Some("rl:auth-reset")
        );
        assert_eq!(ADMIN_AUTH_LOGIN.max, Some(10));
        assert_eq!(ADMIN_AUTH_FORGOT_GEN_EMAIL.window_ms, HOUR);
        assert!(!LOGIN.auth && LOGIN.allow_locked && LOGIN.exempt == Some(Exempt::NoUser));
        assert!(!OPERATOR.auth && OPERATOR.exempt == Some(Exempt::SeparateAuth));
    }

    #[tokio::test]
    async fn router_builds() {
        let state = AppState::for_test();
        assert!(state.is_ok());
        if let Ok(state) = state {
            let _ = router(&state);
        }
    }
}
