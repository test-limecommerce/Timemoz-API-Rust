//! Port 6 modul konfigurasi time tracking dari Mudah-API (32 route):
//! `manual-time-approvals` (8), `manual-time-settings` (4), `project-types` (5),
//! `project-budget` (4: 3 `project-budget` + `budget-report`), `project-manual-time-approvers` (2),
//! `app-releases` (9). Cron `project-budget.cron.ts` TIDAK di-port.
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
use axum::extract::DefaultBodyLimit;
use axum::routing::{delete, get, patch, post, put};
use mz_core::enums::{FeatureModule, PermissionAction, Role};
use mz_core::gate::guard;
use mz_core::rate_limit::{RateKey, RateRule};
use mz_core::{AppState, RouteSpec};

pub mod admin_screenshots;
pub mod approvals;
pub mod approvers;
pub mod attachments;
pub mod operator;
pub mod project_budget;
pub mod project_types;
pub mod releases;
pub mod settings;
pub mod util;

const TIME_TRACKING: &[FeatureModule] = &[FeatureModule::TIME_TRACKING];

/// `@Roles(SYSTEM_ADMIN, SUPER_ADMIN, ADMIN, MANAGER)`.
const MANAGERS: &[Role] = &[
    Role::SYSTEM_ADMIN,
    Role::SUPER_ADMIN,
    Role::ADMIN,
    Role::MANAGER,
];

/// `@Roles(SYSTEM_ADMIN, SUPER_ADMIN, ADMIN, MANAGER, STAFF)`.
const STAFF_UP: &[Role] = &[
    Role::SYSTEM_ADMIN,
    Role::SUPER_ADMIN,
    Role::ADMIN,
    Role::MANAGER,
    Role::STAFF,
];

/// `main.ts`: limiter `rl:manual-time-attachment-file` 120/menit per token (`createRateLimiter` bawaan).
const ATTACHMENT_FILE_LIMIT: &[RateRule] = &[RateRule::token(
    "rl:manual-time-attachment-file",
    120,
    60_000,
)];

/// `main.ts`: limiter `rl:app-release-download` 20/menit per IP (`passOnStoreError` bawaan = false).
const RELEASE_DOWNLOAD_LIMIT: &[RateRule] = &[RateRule {
    key_prefix: "rl:app-release-download",
    max: Some(20),
    window_ms: 60_000,
    key: RateKey::Ip,
    pass_on_store_error: false,
}];

// ── manual-time-approvals: @RequireFeature('manual-time-approvals') + @RequireModule(TIME_TRACKING)
const APPROVALS_PUBLIC: RouteSpec = RouteSpec::public()
    .feature("manual-time-approvals")
    .modules(TIME_TRACKING);
const APPROVALS_JWT: RouteSpec = RouteSpec::jwt()
    .feature("manual-time-approvals")
    .modules(TIME_TRACKING);
const APPROVALS_FILE: RouteSpec = RouteSpec::jwt()
    .roles(STAFF_UP)
    .feature("manual-time-approvals")
    .modules(TIME_TRACKING);

// ── manual-time-settings: class-level @Roles + @RequireFeature + @RequireModule.
const SETTINGS_READ: RouteSpec = RouteSpec::jwt()
    .roles(MANAGERS)
    .feature("manual-time-settings")
    .modules(TIME_TRACKING)
    .action(PermissionAction::READ);
const SETTINGS_WRITE: RouteSpec = RouteSpec::jwt()
    .roles(MANAGERS)
    .feature("manual-time-settings")
    .modules(TIME_TRACKING)
    .action(PermissionAction::WRITE);

// ── project-types: @RequireFeature('projects'); list tanpa @Roles.
const TYPES_LIST: RouteSpec = RouteSpec::jwt().feature("projects");
const TYPES_ADMIN: RouteSpec = RouteSpec::jwt()
    .roles(&[Role::SYSTEM_ADMIN])
    .feature("projects");

// ── project-budget / budget-report / approvers: @RequireFeature('projects') + TIME_TRACKING.
const BUDGET_READ: RouteSpec = RouteSpec::jwt().feature("projects").modules(TIME_TRACKING);
const BUDGET_WRITE: RouteSpec = RouteSpec::jwt()
    .roles(MANAGERS)
    .feature("projects")
    .modules(TIME_TRACKING);
const BUDGET_DELETE: RouteSpec = RouteSpec::jwt()
    .roles(&[Role::SYSTEM_ADMIN, Role::SUPER_ADMIN, Role::ADMIN])
    .feature("projects")
    .modules(TIME_TRACKING);

// ── app-releases: publik = `@PermissionExempt('no-user')`; admin = `separate-auth`
//    (JwtAdminAuthGuard via extractor `operator::Operator`).
const RELEASES_PUBLIC: RouteSpec = RouteSpec::public();
const RELEASES_DOWNLOAD: RouteSpec = RouteSpec::public().rate_limits(RELEASE_DOWNLOAD_LIMIT);
const RELEASES_ADMIN: RouteSpec = RouteSpec::public();

/// Router seluruh route crate ini (path penuh, tanpa prefix).
pub fn router(state: &AppState) -> Router<AppState> {
    Router::new()
        // ───── manual-time-approvals ─────
        .route(
            "/manual-time-approvals/email-action",
            guard(state, APPROVALS_PUBLIC, get(approvals::h_email_action)),
        )
        .route(
            "/manual-time-approvals/my-pending",
            guard(state, APPROVALS_JWT, get(approvals::h_my_pending)),
        )
        .route(
            "/manual-time-approvals/assigned-projects",
            guard(state, APPROVALS_JWT, get(approvals::h_assigned)),
        )
        .route(
            "/manual-time-approvals",
            guard(state, APPROVALS_JWT, get(approvals::h_find_all)),
        )
        .route(
            "/manual-time-approvals/{uuid}/approve",
            guard(state, APPROVALS_JWT, patch(approvals::h_approve)),
        )
        .route(
            "/manual-time-approvals/{uuid}/reject",
            guard(state, APPROVALS_JWT, patch(approvals::h_reject)),
        )
        .route(
            "/manual-time-approvals/attachments/{uuid}/file",
            guard(
                state,
                APPROVALS_FILE.rate_limits(ATTACHMENT_FILE_LIMIT),
                get(attachments::h_file),
            ),
        )
        .route(
            "/manual-time-approvals/attachments/{uuid}/file/url",
            guard(
                state,
                APPROVALS_FILE
                    .headers(&[("cache-control", mz_files::SIGNED_URL_RESPONSE_CACHE_CONTROL)]),
                get(attachments::h_file_url),
            ),
        )
        // ───── manual-time-settings ─────
        .route(
            "/v1/manual-time-settings",
            guard(state, SETTINGS_READ, get(settings::h_get)),
        )
        .route(
            "/v1/manual-time-settings",
            guard(state, SETTINGS_WRITE, put(settings::h_put)),
        )
        .route(
            "/v1/manual-time-settings/employees",
            guard(state, SETTINGS_READ, get(settings::h_employees)),
        )
        .route(
            "/v1/manual-time-settings/employees/uuids",
            guard(state, SETTINGS_READ, get(settings::h_employee_uuids)),
        )
        // ───── project-types ─────
        .route(
            "/project-types",
            guard(state, TYPES_LIST, get(project_types::h_find_all)),
        )
        .route(
            "/project-types",
            guard(state, TYPES_ADMIN, post(project_types::h_create)),
        )
        .route(
            "/project-types/{uuid}",
            guard(state, TYPES_ADMIN, get(project_types::h_find_one)),
        )
        .route(
            "/project-types/{uuid}",
            guard(state, TYPES_ADMIN, patch(project_types::h_update)),
        )
        .route(
            "/project-types/{uuid}",
            guard(state, TYPES_ADMIN, delete(project_types::h_remove)),
        )
        // ───── project-budget + budget-report ─────
        .route(
            "/project-budget/{projectUuid}",
            guard(state, BUDGET_READ, get(project_budget::h_find_one)),
        )
        .route(
            "/project-budget/{projectUuid}",
            guard(state, BUDGET_WRITE, post(project_budget::h_upsert)),
        )
        .route(
            "/project-budget/{projectUuid}",
            guard(state, BUDGET_DELETE, delete(project_budget::h_remove)),
        )
        .route(
            "/budget-report",
            guard(state, BUDGET_READ, get(project_budget::h_report)),
        )
        // ───── project-manual-time-approvers ─────
        .route(
            "/projects/{projectUuid}/manual-time-approvers",
            guard(state, BUDGET_WRITE, post(approvers::h_add)),
        )
        .route(
            "/projects/{projectUuid}/manual-time-approvers/{approverUuid}",
            guard(state, BUDGET_WRITE, delete(approvers::h_remove)),
        )
        // ───── app-releases (publik) ─────
        .route(
            "/app-releases/changelog",
            guard(state, RELEASES_PUBLIC, get(releases::h_changelog)),
        )
        .route(
            "/app-releases/release-notes/{platform}",
            guard(state, RELEASES_PUBLIC, get(releases::h_release_notes)),
        )
        .route(
            "/app-releases/{platform}/{channelFile}",
            guard(state, RELEASES_PUBLIC, get(releases::h_update_yaml)),
        )
        .route(
            "/app-releases/download/{platform}/{version}/{filename}",
            guard(state, RELEASES_DOWNLOAD, get(releases::h_download)),
        )
        // ───── admin/app-releases (operator) ─────
        .route(
            "/admin/app-releases/upload",
            guard(
                state,
                RELEASES_ADMIN,
                post(releases::h_upload).layer(DefaultBodyLimit::max(
                    usize::try_from(releases::MAX_UPLOAD_BYTES).unwrap_or(usize::MAX)
                        + 2 * 1024 * 1024,
                )),
            ),
        )
        .route(
            "/admin/app-releases/upload",
            guard(
                state,
                RELEASES_ADMIN,
                get(releases::h_admin_one_upload).delete(releases::h_delete_upload),
            ),
        )
        .route(
            "/admin/app-releases",
            guard(state, RELEASES_ADMIN, get(releases::h_admin_list)),
        )
        .route(
            "/admin/app-releases/{uuid}",
            guard(state, RELEASES_ADMIN, get(releases::h_admin_one)),
        )
        .route(
            "/admin/app-releases/{uuid}/toggle",
            guard(state, RELEASES_ADMIN, patch(releases::h_toggle)),
        )
        .route(
            "/admin/app-releases/{uuid}",
            guard(state, RELEASES_ADMIN, delete(releases::h_delete)),
        )
        // ───── admin/screenshot (operator, `admin.controller.ts`) ─────
        .route(
            "/admin/screenshot/compress",
            guard(state, RELEASES_ADMIN, post(admin_screenshots::h_compress)),
        )
        .route(
            "/admin/screenshot/purge",
            guard(state, RELEASES_ADMIN, post(admin_screenshots::h_purge)),
        )
        .route(
            "/admin/screenshot/queue",
            guard(
                state,
                RELEASES_ADMIN,
                delete(admin_screenshots::h_queue_clear),
            ),
        )
        .route(
            "/admin/screenshot/queue/pause",
            guard(
                state,
                RELEASES_ADMIN,
                post(admin_screenshots::h_queue_pause),
            ),
        )
        .route(
            "/admin/screenshot/queue/resume",
            guard(
                state,
                RELEASES_ADMIN,
                post(admin_screenshots::h_queue_resume),
            ),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use tower::ServiceExt;

    fn app() -> Option<Router> {
        let state = AppState::for_test().ok()?;
        Some(router(&state).with_state(state))
    }

    async fn status_of(method: &str, uri: &str) -> Option<(StatusCode, Option<String>)> {
        let res = app()?
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .body(Body::empty())
                    .ok()?,
            )
            .await
            .ok()?;
        let loc = res
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        Some((res.status(), loc))
    }

    #[tokio::test]
    async fn router_builds_without_route_conflicts() {
        assert!(app().is_some());
    }

    #[tokio::test]
    async fn email_action_rejects_bad_action_with_302() {
        let (status, loc) = status_of(
            "GET",
            "/manual-time-approvals/email-action?token=x&action=nope",
        )
        .await
        .unwrap_or((StatusCode::IM_A_TEAPOT, None));
        assert_eq!(status, StatusCode::FOUND);
        let loc = loc.unwrap_or_default();
        assert!(
            loc.ends_with(
                "/manual-time-approvals?tab=approvals&status=error&message=Invalid+action"
            )
        );
    }

    #[tokio::test]
    async fn protected_routes_need_a_token() {
        for (m, uri) in [
            ("GET", "/manual-time-approvals"),
            (
                "PATCH",
                "/manual-time-approvals/00000000-0000-4000-8000-000000000000/approve",
            ),
            ("GET", "/v1/manual-time-settings"),
            ("GET", "/project-types"),
            (
                "POST",
                "/project-budget/00000000-0000-4000-8000-000000000000",
            ),
            ("GET", "/budget-report"),
            (
                "POST",
                "/projects/00000000-0000-4000-8000-000000000000/manual-time-approvers",
            ),
            ("GET", "/admin/app-releases"),
            (
                "DELETE",
                "/admin/app-releases/00000000-0000-4000-8000-000000000000",
            ),
        ] {
            let got = status_of(m, uri).await.map(|r| r.0);
            assert_eq!(got, Some(StatusCode::UNAUTHORIZED), "{m} {uri}");
        }
    }
}
