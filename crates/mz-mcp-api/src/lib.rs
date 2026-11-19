//! Port `src/mcp-api/**`: API MCP untuk klien USER (bukan operator; yang operator ada di
//! `mz-admin-mcp`). Auth = token Bearer `mcp_pat_*` (PAT, dibuat lewat `POST /mcp-api/tokens`) atau
//! `mcp_oauth_*` (access token OAuth 2.1 + PKCE S256). Seluruh endpoint data READ-ONLY dan
//! dibatasi ke company pemanggil. Lihat `plan/deviations/mz-mcp-api.md`.
//!
//! 21 route:
//! - discovery (publik): `GET /.well-known/oauth-authorization-server`,
//!   `GET /.well-known/oauth-protected-resource`
//! - OAuth: `POST /mcp-api/oauth/register`, `GET|POST /mcp-api/oauth/authorize`,
//!   `POST /mcp-api/oauth/token`, `POST /mcp-api/oauth/revoke`
//! - PAT (JWT user): `POST|GET /mcp-api/tokens`, `DELETE /mcp-api/tokens/:uuid`,
//!   `GET /mcp-api/audit-logs`
//! - data (PAT/OAuth): `users`, `users/me`, `projects`, `projects/:uuid`, `reports/project/:uuid`,
//!   `reports/team`, `trackings/recent`, `trackings/summary`, `tasks`, `calendar/events`
//!
//! Path `/.well-known/*` ada di akar (bukan di bawah `/mcp-api`); `mz-admin-mcp` memakai
//! `/admin-mcp/.well-known/*`, jadi TIDAK bentrok. Reverse proxy (nginx) harus meneruskan
//! `/.well-known/oauth-*` ke API ini.

pub mod audit_logs;
pub mod calendar;
pub mod common;
pub mod dto;
pub mod oauth;
pub mod pat;
pub mod projects;
pub mod reports;
pub mod tasks;
pub mod tokens;
pub mod trackings;
pub mod users;

use axum::Router;
use axum::routing::{delete, get, post};
use mz_core::AppState;
use mz_core::gate::{RouteSpec, guard};
use mz_core::permissions::Exempt;

use crate::pat::pat;

/// `@PermissionExempt('no-user')`: flow OAuth publik / dokumen discovery.
const PUBLIC: RouteSpec = RouteSpec::public();
/// `@UseGuards(JwtAuthGuard) @PermissionExempt('self-service')` (authorize POST, revoke).
const SELF_SERVICE: RouteSpec = RouteSpec::jwt().exempt(Exempt::SelfService);
/// `@UseGuards(JwtAuthGuard) @PermissionExempt('self-scoped')` (tokens, audit-logs).
const SELF_SCOPED: RouteSpec = RouteSpec::jwt().exempt(Exempt::SelfScoped);

pub fn router(state: &AppState) -> Router<AppState> {
    let public = |r| guard(state, PUBLIC, r);
    let service = |r| guard(state, SELF_SERVICE, r);
    let scoped = |r| guard(state, SELF_SCOPED, r);
    let patg = |r| pat(state, r);
    Router::new()
        // ───── discovery ─────
        .route(
            "/.well-known/oauth-authorization-server",
            public(get(oauth::h_metadata)),
        )
        .route(
            "/.well-known/oauth-protected-resource",
            public(get(oauth::h_protected_resource)),
        )
        // ───── OAuth ─────
        // `register`: kode Node identik dengan admin-mcp (`RegisterClientDto`, tabel `mcpOAuthClient`).
        .route(
            "/mcp-api/oauth/register",
            public(post(mz_admin_mcp::oauth::h_register)),
        )
        .route(
            "/mcp-api/oauth/authorize",
            public(get(oauth::h_authorize_get)),
        )
        .route(
            "/mcp-api/oauth/authorize",
            service(post(oauth::h_authorize_post)),
        )
        .route("/mcp-api/oauth/token", public(post(oauth::h_token)))
        .route("/mcp-api/oauth/revoke", service(post(oauth::h_revoke)))
        // ───── tokens (PAT milik user) ─────
        .route("/mcp-api/tokens", scoped(post(tokens::h_create)))
        .route("/mcp-api/tokens", scoped(get(tokens::h_list)))
        .route("/mcp-api/tokens/{uuid}", scoped(delete(tokens::h_revoke)))
        .route("/mcp-api/audit-logs", scoped(get(audit_logs::h_list)))
        // ───── data (PatAuthGuard) ─────
        .route("/mcp-api/users/me", patg(get(users::h_me)))
        .route("/mcp-api/users", patg(get(users::h_list)))
        .route("/mcp-api/projects", patg(get(projects::h_list)))
        .route("/mcp-api/projects/{uuid}", patg(get(projects::h_detail)))
        .route(
            "/mcp-api/reports/project/{uuid}",
            patg(get(reports::h_project)),
        )
        .route("/mcp-api/reports/team", patg(get(reports::h_team)))
        .route(
            "/mcp-api/trackings/summary",
            patg(get(trackings::h_summary)),
        )
        .route("/mcp-api/trackings/recent", patg(get(trackings::h_recent)))
        .route("/mcp-api/tasks", patg(get(tasks::h_list)))
        .route("/mcp-api/calendar/events", patg(get(calendar::h_events)))
}
