//! Port `src/auth` dari Mudah-API (`auth.controller.ts`, 18 route).
//!
//! Level kelas: `@PermissionExempt('no-user')` + `@AllowWhenSubscriptionLocked()`.
//! Route ber-`JwtAuthGuard` (`me`, `logout`, `web-session`, `change-password`,
//! `onboarding/create-organization`) di-override `@PermissionExempt('self-service')`.
//! Rate limit = tabel `rate-limit/auth-rate-limits.ts`.

#![cfg_attr(test, allow(clippy::indexing_slicing))]

pub mod analytics;
pub mod analytics_jobs;
pub mod dto;
pub mod ga;
pub mod handlers;
pub mod js;
pub mod login_attempts;
pub mod rowjson;
pub mod service;

use axum::Router;
use axum::routing::{get, post};
use mz_core::enums::Role;
use mz_core::gate::guard;
use mz_core::permissions::Exempt;
use mz_core::rate_limit::{RateRule, rules};
use mz_core::{AppState, RouteSpec};
use mz_jobs::{CronJob, Schedule};
use std::time::Duration;

/// Route pra-login (`no-user`).
const fn public(limits: &'static [RateRule]) -> RouteSpec {
    RouteSpec::public()
        .exempt(Exempt::NoUser)
        .allow_locked()
        .rate_limits(limits)
}

/// Route ber-`JwtAuthGuard` (`self-service`).
const fn jwt(limits: &'static [RateRule]) -> RouteSpec {
    RouteSpec::jwt()
        .exempt(Exempt::SelfService)
        .allow_locked()
        .rate_limits(limits)
}

const LOGIN: RouteSpec = public(&[rules::AUTH_LOGIN]);
const CONFIRM: RouteSpec = public(&[rules::AUTH_CONFIRM]);
const REGISTER: RouteSpec = public(&[rules::AUTH_REGISTER]);
const ME: RouteSpec = jwt(&[]);
const LOGOUT: RouteSpec = jwt(&[rules::AUTH_LOGOUT]);
const WEB_SESSION: RouteSpec = jwt(&[rules::AUTH_WEB_SESSION]).roles(&[
    Role::SYSTEM_ADMIN,
    Role::SUPER_ADMIN,
    Role::ADMIN,
    Role::MANAGER,
    Role::STAFF,
    Role::GUEST,
]);
const CHANGE_PASSWORD: RouteSpec = jwt(&[rules::AUTH_CHANGE_PASSWORD]);
const CONFIRM_REGISTER: RouteSpec = public(&[rules::AUTH_CONFIRM]);
const RESEND_CONFIRMATION: RouteSpec =
    public(&[rules::AUTH_RESEND_CONFIRM, rules::AUTH_RESEND_CONFIRM_EMAIL]);
const CREATE_ORGANIZATION: RouteSpec = jwt(&[]);
const CHECK_VERIFICATION: RouteSpec = public(&[rules::AUTH_CHECK_VERIFICATION]);
const FORGOT_GENERATE: RouteSpec = public(&[rules::AUTH_FORGOT_GEN, rules::AUTH_FORGOT_GEN_EMAIL]);
const FORGOT_PASSWORD: RouteSpec = public(&[rules::AUTH_RESET]);
const INVITE_INFO: RouteSpec = public(&[rules::AUTH_INVITE_INFO]);
const REGISTER_INVITE: RouteSpec = public(&[rules::AUTH_REGISTER_INVITE]);
const GOOGLE: RouteSpec = public(&[]);

/// Cron analytics. Node: `src/analytics/analytics-milestones.service.ts:73` (`analytics-milestones`,
/// `@Cron('30 5 * * *', WIB)`) dan `:83` (`analytics-retry-sweep`, `@Cron('7,37 * * * *')`).
pub fn cron_jobs() -> Vec<CronJob> {
    vec![
        CronJob {
            name: "analytics-retry-sweep",
            schedule: Schedule::hourly(&[7, 37]),
            timeout: Duration::from_secs(5 * 60),
            run: |s| Box::pin(analytics_jobs::run_retry_sweep(s)),
        },
        CronJob {
            name: "analytics-milestones",
            schedule: Schedule::daily(&[5], &[30]),
            timeout: Duration::from_secs(15 * 60),
            run: |s| Box::pin(analytics_jobs::run_milestones(s)),
        },
    ]
}

pub fn router(state: &AppState) -> Router<AppState> {
    use handlers as h;
    Router::new()
        .route("/auth/login", guard(state, LOGIN, post(h::login)))
        .route("/auth/confirm", guard(state, CONFIRM, post(h::confirm)))
        .route("/auth/register", guard(state, REGISTER, post(h::register)))
        .route("/auth/me", guard(state, ME, get(h::me)))
        .route("/auth/logout", guard(state, LOGOUT, post(h::logout)))
        .route(
            "/auth/web-session",
            guard(state, WEB_SESSION, post(h::web_session)),
        )
        .route(
            "/auth/change-password",
            guard(state, CHANGE_PASSWORD, post(h::change_password)),
        )
        .route(
            "/auth/confirm-register",
            guard(state, CONFIRM_REGISTER, post(h::confirm_register)),
        )
        .route(
            "/auth/resend-confirmation",
            guard(state, RESEND_CONFIRMATION, post(h::resend_confirmation)),
        )
        .route(
            "/auth/onboarding/create-organization",
            guard(state, CREATE_ORGANIZATION, post(h::create_organization)),
        )
        .route(
            "/auth/check-verification",
            guard(state, CHECK_VERIFICATION, get(h::check_verification)),
        )
        .route(
            "/auth/forgot-password/generate",
            guard(state, FORGOT_GENERATE, post(h::forgot_password_generate)),
        )
        .route(
            "/auth/forgot-password",
            guard(state, FORGOT_PASSWORD, post(h::forgot_password)),
        )
        .route(
            "/auth/invite/{inviteCode}",
            guard(state, INVITE_INFO, get(h::invite_info)),
        )
        .route(
            "/auth/register-invite",
            guard(state, REGISTER_INVITE, post(h::register_invite)),
        )
        .route("/auth/google", guard(state, GOOGLE, get(h::google_login)))
        .route(
            "/auth/google/register",
            guard(state, GOOGLE, get(h::google_register)),
        )
        .route(
            "/auth/google-redirect",
            guard(state, GOOGLE, get(h::google_callback)),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs_match_node_decorators() {
        assert!(!LOGIN.auth && LOGIN.allow_locked && LOGIN.exempt == Some(Exempt::NoUser));
        assert!(ME.auth && ME.allow_locked && ME.exempt == Some(Exempt::SelfService));
        assert_eq!(WEB_SESSION.roles.map(<[Role]>::len), Some(6));
        assert_eq!(FORGOT_GENERATE.rate_limits.len(), 2);
        assert_eq!(RESEND_CONFIRMATION.rate_limits.len(), 2);
        assert_eq!(
            RESEND_CONFIRMATION
                .rate_limits
                .iter()
                .map(|r| r.key_prefix)
                .collect::<Vec<_>>(),
            vec!["rl:auth-resend-confirm", "rl:auth-resend-confirm-email"]
        );
        assert_eq!(
            CONFIRM_REGISTER.rate_limits.first().map(|r| r.key_prefix),
            Some("rl:auth-confirm")
        );
        assert!(CREATE_ORGANIZATION.rate_limits.is_empty() && ME.rate_limits.is_empty());
    }

    #[tokio::test]
    async fn router_builds() {
        let state = AppState::for_test();
        assert!(state.is_ok(), "AppState::for_test harus bisa dibuat");
        if let Ok(state) = state {
            let _ = router(&state);
        }
    }
}
