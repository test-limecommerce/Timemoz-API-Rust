//! Port `src/integrations/jira` (`jira.controller.ts`, `jira.service.ts`,
//! `jira-connect.guard.ts`, `dto/*`).
//!
//! - [`router`]: 16 route `integrations/jira/*`.
//! - [`cron_jobs`]: `scheduledSyncAllProjects` (03:20 WIB) dan
//!   `scheduledRefreshWebhooks` (Senin 01:10 WIB).
//! - [`get_connection_status`], [`sync_tasks`] (+ [`sync_tasks_with`]): dipakai agregat
//!   `/integrations`.
//!
//! - [`track_worklog`]: `trackWorklog`, dipanggil `mz-timelines` sesudah timeline berubah.

mod creds;
mod cron;
mod handlers;
mod http;
mod service;
mod sync;
mod track;
mod util;

use axum::Router;
use axum::routing::{delete, get, patch, post};
use mz_core::AppState;
use mz_core::enums::Role;
use mz_core::gate::{RouteSpec, guard};

pub use cron::{
    SYNC_JOB, WEBHOOK_JOB, cron_jobs, scheduled_refresh_webhooks, scheduled_sync_all_projects,
};
pub use service::{
    JiraProjectItem, JiraStatus, LinkProjectData, LinkUserData, LinkedProject, LinkedProjectRow,
    LinkedUser, MemberItem, OAuthData, StatusChange, TaskRow, TasksPage, TransitionItem,
    get_connection_status, sync_tasks, sync_tasks_with,
};
pub use sync::SyncCounts;
pub use track::{track_worklog, worklog_started};

const ADMINS: &[Role] = &[Role::SYSTEM_ADMIN, Role::SUPER_ADMIN, Role::ADMIN];

/// `@RequireFeature('integrations')` (kelas) + `@UseGuards(JwtAuthGuard, RolesGuard)` +
/// `@Roles(SYSTEM_ADMIN, SUPER_ADMIN, ADMIN)`. `@SkipThrottle()` tidak berefek di Rust.
const ADMIN: RouteSpec = RouteSpec::jwt().roles(ADMINS).feature("integrations");
/// Route tanpa guard (webhook, callback OAuth). Tanpa `req.user` interceptor global lewat.
const PUBLIC: RouteSpec = RouteSpec::public().feature("integrations");

pub fn router(state: &AppState) -> Router<AppState> {
    let s = state;
    Router::new()
        .route(
            "/integrations/jira/webhook",
            guard(s, PUBLIC, post(handlers::webhook)),
        )
        .route(
            "/integrations/jira/status",
            guard(s, ADMIN, get(handlers::status)),
        )
        // `JiraConnectGuard` dijalankan di handler (token di query).
        .route(
            "/integrations/jira/connect",
            guard(s, PUBLIC, get(handlers::connect)),
        )
        .route(
            "/integrations/jira/callback",
            guard(s, PUBLIC, get(handlers::callback)),
        )
        .route(
            "/integrations/jira/disconnect",
            guard(s, ADMIN, delete(handlers::disconnect)),
        )
        .route(
            "/integrations/jira/projects",
            guard(s, ADMIN, get(handlers::projects)),
        )
        .route(
            "/integrations/jira/linked-projects",
            guard(s, ADMIN, get(handlers::linked_projects)),
        )
        .route(
            "/integrations/jira/projects/link",
            guard(s, ADMIN, post(handlers::link_project)),
        )
        .route(
            "/integrations/jira/projects/{projectUuid}/link",
            guard(s, ADMIN, delete(handlers::unlink_project)),
        )
        .route(
            "/integrations/jira/projects/{projectUuid}/sync-tasks",
            guard(s, ADMIN, post(handlers::sync_tasks)),
        )
        .route(
            "/integrations/jira/projects/{projectUuid}/tasks",
            guard(s, ADMIN, get(handlers::project_tasks)),
        )
        .route(
            "/integrations/jira/tasks/{taskUuid}/statuses",
            guard(s, ADMIN, get(handlers::task_statuses)),
        )
        .route(
            "/integrations/jira/tasks/{taskUuid}/status",
            guard(s, ADMIN, patch(handlers::change_task_status)),
        )
        .route(
            "/integrations/jira/members",
            guard(s, ADMIN, get(handlers::members)),
        )
        .route(
            "/integrations/jira/users/link",
            guard(s, ADMIN, post(handlers::link_user)),
        )
        .route(
            "/integrations/jira/users/{userUuid}/link",
            guard(s, ADMIN, delete(handlers::unlink_user)),
        )
}
