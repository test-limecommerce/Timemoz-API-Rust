//! Port `src/admin-mixpanel/**` dan `src/admin-mcp/**` (MCP admin: analitik Mixpanel,
//! server OAuth admin, PAT operator). Lihat `plan/deviations/mz-admin-mcp.md`.
//!
//! - [`router`]: 8 route `/admin/mixpanel/*`, 13 route `/admin-mcp/mixpanel/*`, OAuth
//!   `/admin-mcp/oauth/*` (5) + `/admin-mcp/.well-known/*` (2), `/admin/mcp-tokens` (3).
//! - [`cron_jobs`]: `mixpanel-daily-sync` (`MixpanelSyncService.handleDailySync`, 01:00 WIB).

pub mod analytics;
pub mod body;
pub mod oauth;
pub mod pat;
pub mod sync;
pub mod tokens;

mod mixpanel_routes;

use axum::Router;
use axum::routing::{delete, get, post};
use mz_core::AppState;
use mz_core::gate::{RouteSpec, guard};
use mz_jobs::CronJob;

/// Semua controller ini `@PermissionExempt('separate-auth' | 'no-user')` +
/// `@AllowWhenSubscriptionLocked()`: auth ditangani extractor (`Operator` / `AdminPat`).
const SPEC: RouteSpec = RouteSpec::public();

pub fn cron_jobs() -> Vec<CronJob> {
    sync::cron_jobs()
}

pub fn router(state: &AppState) -> Router<AppState> {
    use mixpanel_routes::{admin, mcp};
    let g = |r| guard(state, SPEC, r);
    Router::new()
        // ───── /admin/mixpanel (JwtAdminAuthGuard) ─────
        .route("/admin/mixpanel/overview", g(get(admin::h_overview)))
        .route("/admin/mixpanel/traffic", g(get(admin::h_traffic)))
        .route("/admin/mixpanel/pages", g(get(admin::h_pages)))
        .route("/admin/mixpanel/cta", g(get(admin::h_cta)))
        .route("/admin/mixpanel/funnel", g(get(admin::h_funnel)))
        .route("/admin/mixpanel/campaigns", g(get(admin::h_campaigns)))
        .route(
            "/admin/mixpanel/registrations",
            g(get(admin::h_registrations)),
        )
        .route("/admin/mixpanel/sync", g(post(admin::h_sync)))
        // ───── /admin-mcp/mixpanel (AdminPatAuthGuard) ─────
        .route("/admin-mcp/mixpanel/overview", g(get(mcp::h_overview)))
        .route("/admin-mcp/mixpanel/traffic", g(get(mcp::h_traffic)))
        .route("/admin-mcp/mixpanel/pages", g(get(mcp::h_pages)))
        .route("/admin-mcp/mixpanel/cta", g(get(mcp::h_cta)))
        .route("/admin-mcp/mixpanel/funnel", g(get(mcp::h_funnel)))
        .route("/admin-mcp/mixpanel/campaigns", g(get(mcp::h_campaigns)))
        .route(
            "/admin-mcp/mixpanel/registrations",
            g(get(mcp::h_registrations)),
        )
        .route("/admin-mcp/mixpanel/geo", g(get(mcp::h_geo)))
        .route("/admin-mcp/mixpanel/devices", g(get(mcp::h_devices)))
        .route("/admin-mcp/mixpanel/hourly", g(get(mcp::h_hourly)))
        .route(
            "/admin-mcp/mixpanel/day-of-week",
            g(get(mcp::h_day_of_week)),
        )
        .route(
            "/admin-mcp/mixpanel/new-vs-returning",
            g(get(mcp::h_new_vs_returning)),
        )
        .route("/admin-mcp/mixpanel/comparison", g(get(mcp::h_comparison)))
        // ───── /admin-mcp/oauth + .well-known ─────
        .route("/admin-mcp/oauth/register", g(post(oauth::h_register)))
        .route(
            "/admin-mcp/oauth/authorize",
            g(get(oauth::h_authorize_get)).merge(g(post(oauth::h_authorize_post))),
        )
        .route("/admin-mcp/oauth/token", g(post(oauth::h_token)))
        .route("/admin-mcp/oauth/revoke", g(post(oauth::h_revoke)))
        .route(
            "/admin-mcp/.well-known/oauth-authorization-server",
            g(get(oauth::h_metadata)),
        )
        .route(
            "/admin-mcp/.well-known/oauth-protected-resource",
            g(get(oauth::h_protected_resource)),
        )
        // ───── /admin/mcp-tokens (JwtAdminAuthGuard) ─────
        .route(
            "/admin/mcp-tokens",
            g(post(tokens::h_create)).merge(g(get(tokens::h_list))),
        )
        .route("/admin/mcp-tokens/{uuid}", g(delete(tokens::h_revoke)))
}
