//! Port `src/screenshots` (8 route). Cron `screenshots.cron.ts` TIDAK di-port.

pub mod access;
pub mod dto;
pub mod handlers;
pub mod service;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::routing::{delete, get, patch, post, put};
use mz_core::enums::{FeatureModule, Role};
use mz_core::gate::guard;
use mz_core::{AppState, RouteSpec};

/// `@RequireFeature('screenshots') @RequireModule(TIME_TRACKING)` + `@UseGuards(JwtAuthGuard)`.
const SPEC: RouteSpec = RouteSpec::jwt()
    .feature("screenshots")
    .modules(&[FeatureModule::TIME_TRACKING]);

/// `image` & `image/url`: `@Roles(SYSTEM_ADMIN..STAFF)` + `RolesGuard`.
/// `@Header('Cache-Control', SIGNED_URL_RESPONSE_CACHE_CONTROL)` — juga pada error sesudah guard.
const IMAGE_URL_SPEC: RouteSpec =
    IMAGE_SPEC.headers(&[("cache-control", mz_files::SIGNED_URL_RESPONSE_CACHE_CONTROL)]);

const IMAGE_SPEC: RouteSpec = SPEC.roles(&[
    Role::SYSTEM_ADMIN,
    Role::SUPER_ADMIN,
    Role::ADMIN,
    Role::MANAGER,
    Role::STAFF,
]);

/// Parser JSON bawaan Express: 100kb.
const JSON_LIMIT: usize = 100 * 1024;

pub fn router(state: &AppState) -> Router<AppState> {
    Router::new()
        .route(
            "/screenshots",
            guard(
                state,
                SPEC,
                post(handlers::create).layer(DefaultBodyLimit::max(JSON_LIMIT)),
            ),
        )
        .route(
            "/screenshots/{uuid}",
            guard(state, SPEC, get(handlers::find_one)),
        )
        .route(
            "/screenshots/{uuid}",
            guard(
                state,
                SPEC,
                patch(handlers::update).layer(DefaultBodyLimit::max(JSON_LIMIT)),
            ),
        )
        .route(
            "/screenshots/{uuid}",
            guard(state, SPEC, delete(handlers::remove)),
        )
        .route(
            "/screenshots/blur/{uuid}",
            guard(state, SPEC, put(handlers::blur_image)),
        )
        .route(
            "/screenshots/{uuid}/open-url",
            guard(state, SPEC, get(handlers::get_open_url)),
        )
        // Proxy gambar ber-JWT sengaja TANPA rate limiter (keputusan di `main.ts`).
        .route(
            "/screenshots/{uuid}/image",
            guard(state, IMAGE_SPEC, get(handlers::image)),
        )
        .route(
            "/screenshots/{uuid}/image/url",
            guard(state, IMAGE_URL_SPEC, get(handlers::image_url)),
        )
}
