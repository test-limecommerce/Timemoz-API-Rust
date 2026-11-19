//! Port `src/companies/*` (7 route `/companies*`; `switch` tidak di-port) dan `src/access-levels/*` (5 route
//! `/access-levels*`). Pasang dengan `.merge(mz_companies::router(&state))`.
//!
//! Tulisan ke model ber-audit (`company`, `companyWorkPattern`, `userCompany`, `accessLevel`)
//! lewat `mz_audit::AuditHandle`; invalidasi cache lintas proses lewat `mz_core::invalidation`.
//! Deviasi: `plan/deviations/mz-companies.md`.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic
    )
)]

mod access_levels;
mod common;
mod companies;
mod dto;
mod work_pattern;

use axum::Router;
use axum::routing::{delete, get, patch, post};
use mz_core::AppState;
use mz_core::enums::Role;
use mz_core::gate::{RouteSpec, guard};

pub use companies::generate_invite_code;

// ── @Controller('companies') — `@RequireFeature('company-settings')` level kelas ──
const COMPANY_FEATURE: &str = "company-settings";
const LIST: RouteSpec = RouteSpec::jwt()
    .roles(&[Role::SYSTEM_ADMIN])
    .feature(COMPANY_FEATURE)
    .allow_locked();
const ADMINS: &[Role] = &[Role::SYSTEM_ADMIN, Role::SUPER_ADMIN, Role::ADMIN];
const GET_ONE: RouteSpec = RouteSpec::jwt().roles(ADMINS).feature(COMPANY_FEATURE);
const PATCH_ONE: RouteSpec = RouteSpec::jwt().roles(ADMINS).feature(COMPANY_FEATURE);
const DELETE_ONE: RouteSpec = RouteSpec::jwt()
    .roles(&[Role::SYSTEM_ADMIN])
    .feature(COMPANY_FEATURE);
const REGENERATE: RouteSpec = RouteSpec::jwt()
    .roles(&[Role::SUPER_ADMIN, Role::ADMIN])
    .feature(COMPANY_FEATURE);
const SIGN_URL: RouteSpec = RouteSpec::jwt()
    .roles(&[Role::SYSTEM_ADMIN])
    .feature(COMPANY_FEATURE);

// ── @Controller('access-levels') — `@RequireFeature('access-levels')`, `@SkipThrottle()` ──
const AL_FEATURE: &str = "access-levels";
const AL_CREATE: RouteSpec = RouteSpec::jwt()
    .roles(&[Role::SYSTEM_ADMIN])
    .feature(AL_FEATURE);
const AL_READ: RouteSpec = RouteSpec::jwt()
    .roles(&[Role::SYSTEM_ADMIN, Role::SUPER_ADMIN])
    .feature(AL_FEATURE);
const AL_WRITE: RouteSpec = RouteSpec::jwt()
    .roles(&[Role::SYSTEM_ADMIN])
    .feature(AL_FEATURE);

pub fn router(state: &AppState) -> Router<AppState> {
    let router = Router::new()
        .route("/companies", get_list(state))
        .route(
            "/companies/{uuid}",
            guard(state, GET_ONE, get(companies::find_one)),
        )
        .route(
            "/companies/{uuid}",
            guard(state, PATCH_ONE, patch(companies::update)),
        )
        .route(
            "/companies/{uuid}",
            guard(state, DELETE_ONE, delete(companies::remove)),
        )
        .route(
            "/companies/invite/regenerate",
            guard(state, REGENERATE, post(companies::regenerate_invite_link)),
        )
        .route(
            "/companies/{uuid}/profile-picture/presigned-url",
            guard(state, SIGN_URL, get(companies::sign_url)),
        );
    router
        .route(
            "/access-levels",
            guard(state, AL_CREATE, post(access_levels::create)),
        )
        .route(
            "/access-levels",
            guard(state, AL_READ, get(access_levels::find_all)),
        )
        .route(
            "/access-levels/{uuid}",
            guard(state, AL_READ, get(access_levels::find_one)),
        )
        .route(
            "/access-levels/{uuid}",
            guard(state, AL_WRITE, patch(access_levels::update)),
        )
        .route(
            "/access-levels/{uuid}",
            guard(state, AL_WRITE, delete(access_levels::remove)),
        )
}

fn get_list(state: &AppState) -> axum::routing::MethodRouter<AppState> {
    guard(state, LIST, get(companies::find_all))
}
