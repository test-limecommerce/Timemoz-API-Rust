//! Port `src/integrations/clickup` (Mudah-API): 18 route `ClickupController`
//! (`@RequireFeature('integrations')`, `@Roles(SYSTEM_ADMIN, SUPER_ADMIN, ADMIN)` kecuali
//! `GET projects/linked`), webhook publik `POST integrations/clickup/webhook`
//! (`@PermissionExempt('no-user')`, body mentah + HMAC), cron
//! [`cron_jobs`] (02:30 WIB), dan ekspor [`get_connection_status`] / [`sync_tasks`] untuk
//! agregat `/integrations`.
//!
//! Semua route `@HttpCode(200)`; balasan `{ success, message, data }` (tidak ada yang
//! melempar kecuali validasi DTO dan gerbang).
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic
    )
)]

use axum::body::Bytes;
use axum::extract::{RawQuery, State};
use axum::http::HeaderMap;
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use mz_core::enums::Role;
use mz_core::extract::{PathParams, ValidatedJson};
use mz_core::gate::guard;
use mz_core::permissions::Exempt;
use mz_core::{ApiError, ApiResult, AppState, RequestCtx, RouteSpec};
use mz_int_common::envelope::Envelope;

mod client;
mod cron;
pub mod dto;
pub mod service;
mod tasks;
mod track;
mod util;
mod webhook;

pub use cron::{cron_jobs, scheduled_sync_all_projects};
pub use service::{ClickupStatus, get_connection_status, sync_tasks};
pub use tasks::SyncTotals;
pub use track::{comment_text, format_tracked_duration, track_task_time_comment};

const ADMINS: &[Role] = &[Role::SYSTEM_ADMIN, Role::SUPER_ADMIN, Role::ADMIN];
const STAFF_UP: &[Role] = &[
    Role::SYSTEM_ADMIN,
    Role::SUPER_ADMIN,
    Role::ADMIN,
    Role::MANAGER,
    Role::STAFF,
];

/// Class-level `ClickupController`.
const ADMIN: RouteSpec = RouteSpec::jwt().roles(ADMINS).feature("integrations");
/// `GET projects/linked` (`@Roles` method menimpa class).
const LINKED_LIST: RouteSpec = RouteSpec::jwt().roles(STAFF_UP).feature("integrations");
/// `ClickupWebhookController`.
const WEBHOOK: RouteSpec = RouteSpec::public().exempt(Exempt::NoUser);

/// Router seluruh route ClickUp (path penuh, tanpa prefix).
pub fn router(state: &AppState) -> Router<AppState> {
    const P: &str = "/integrations/clickup";
    Router::new()
        .route(
            &format!("{P}/workspaces"),
            guard(state, ADMIN, get(h_workspaces)),
        )
        .route(
            &format!("{P}/workspaces/{{workspaceId}}/spaces"),
            guard(state, ADMIN, get(h_spaces)),
        )
        .route(
            &format!("{P}/spaces/{{spaceId}}/lists"),
            guard(state, ADMIN, get(h_lists)),
        )
        .route(
            &format!("{P}/sync-projects"),
            guard(state, ADMIN, post(h_sync_projects)),
        )
        .route(
            &format!("{P}/synced-projects"),
            guard(state, ADMIN, get(h_synced_projects)),
        )
        .route(&format!("{P}/folders"), guard(state, ADMIN, get(h_folders)))
        .route(
            &format!("{P}/projects/link"),
            guard(state, ADMIN, post(h_link_project)),
        )
        .route(
            &format!("{P}/projects/{{projectUuid}}/link"),
            guard(state, ADMIN, delete(h_unlink_project)),
        )
        .route(
            &format!("{P}/linked-projects"),
            guard(state, ADMIN, get(h_linked_projects)),
        )
        .route(&format!("{P}/members"), guard(state, ADMIN, get(h_members)))
        .route(
            &format!("{P}/users/link"),
            guard(state, ADMIN, post(h_link_user)),
        )
        .route(
            &format!("{P}/users/{{userUuid}}/link"),
            guard(state, ADMIN, delete(h_unlink_user)),
        )
        .route(
            &format!("{P}/linked-users"),
            guard(state, ADMIN, get(h_linked_users)),
        )
        .route(
            &format!("{P}/projects/linked"),
            guard(state, LINKED_LIST, get(h_linked_project_list)),
        )
        .route(
            &format!("{P}/projects/{{projectUuid}}/sync-tasks"),
            guard(state, ADMIN, post(h_sync_tasks)),
        )
        .route(
            &format!("{P}/tasks/{{taskUuid}}/statuses"),
            guard(state, ADMIN, get(h_task_statuses)),
        )
        .route(
            &format!("{P}/tasks/{{taskUuid}}/status"),
            guard(state, ADMIN, patch(h_change_task_status)),
        )
        .route(
            &format!("{P}/projects/{{projectUuid}}/tasks"),
            guard(state, ADMIN, get(h_project_tasks)),
        )
        .route(
            &format!("{P}/webhook"),
            guard(state, WEBHOOK, post(h_webhook)),
        )
}

type Out<T> = ApiResult<Json<Envelope<T>>>;

/// Node: src/integrations/clickup/clickup.controller.ts:30
async fn h_workspaces(State(s): State<AppState>, ctx: RequestCtx) -> Out<Vec<service::Workspace>> {
    Ok(Json(
        service::get_workspaces(&s, ctx.require_user_id()?).await,
    ))
}

/// Node: src/integrations/clickup/clickup.controller.ts:37
async fn h_spaces(
    State(s): State<AppState>,
    ctx: RequestCtx,
    PathParams(workspace_id): PathParams<String>,
) -> Out<Vec<service::Space>> {
    Ok(Json(
        service::get_spaces(&s, ctx.require_user_id()?, &workspace_id).await,
    ))
}

/// Node: src/integrations/clickup/clickup.controller.ts:45
async fn h_lists(
    State(s): State<AppState>,
    ctx: RequestCtx,
    PathParams(space_id): PathParams<String>,
) -> Out<Vec<service::ListEntry>> {
    Ok(Json(
        service::get_lists(&s, ctx.require_user_id()?, &space_id).await,
    ))
}

/// Node: src/integrations/clickup/clickup.controller.ts:52
async fn h_sync_projects(
    State(s): State<AppState>,
    ctx: RequestCtx,
    ValidatedJson(body): ValidatedJson<dto::SyncProjectsBody>,
) -> Out<service::SyncProjectsResult> {
    let uid = ctx.require_user_id()?;
    Ok(Json(service::sync_projects(&s, uid, &body.lists()).await))
}

/// Node: src/integrations/clickup/clickup.controller.ts:59
async fn h_synced_projects(
    State(s): State<AppState>,
    ctx: RequestCtx,
) -> Out<Vec<service::SyncedProject>> {
    Ok(Json(
        service::get_synced_projects(&s, ctx.require_user_id()?).await,
    ))
}

/// Node: src/integrations/clickup/clickup.controller.ts:66
async fn h_folders(State(s): State<AppState>, ctx: RequestCtx) -> Out<Vec<service::FolderEntry>> {
    Ok(Json(service::get_folders(&s, ctx.require_user_id()?).await))
}

/// Node: src/integrations/clickup/clickup.controller.ts:73
async fn h_link_project(
    State(s): State<AppState>,
    ctx: RequestCtx,
    ValidatedJson(body): ValidatedJson<dto::LinkProjectBody>,
) -> Out<service::LinkProjectResult> {
    let uid = ctx.require_user_id()?;
    Ok(Json(service::link_project(&s, uid, &body.into_dto()).await))
}

/// Node: src/integrations/clickup/clickup.controller.ts:80
async fn h_unlink_project(
    State(s): State<AppState>,
    ctx: RequestCtx,
    PathParams(project_uuid): PathParams<String>,
) -> Out<()> {
    Ok(Json(
        service::unlink_project(&s, ctx.require_user_id()?, &project_uuid).await,
    ))
}

/// Node: src/integrations/clickup/clickup.controller.ts:88
async fn h_linked_projects(
    State(s): State<AppState>,
    ctx: RequestCtx,
) -> Out<Vec<service::LinkedProject>> {
    Ok(Json(
        service::get_linked_projects(&s, ctx.require_user_id()?).await,
    ))
}

/// Node: src/integrations/clickup/clickup.controller.ts:95
async fn h_members(State(s): State<AppState>, ctx: RequestCtx) -> Out<Vec<service::Member>> {
    Ok(Json(service::get_members(&s, ctx.require_user_id()?).await))
}

/// Node: src/integrations/clickup/clickup.controller.ts:102
async fn h_link_user(
    State(s): State<AppState>,
    ctx: RequestCtx,
    ValidatedJson(body): ValidatedJson<dto::LinkUserBody>,
) -> Out<service::LinkUserResult> {
    let uid = ctx.require_user_id()?;
    Ok(Json(service::link_user(&s, uid, &body.into_dto()).await))
}

/// Node: src/integrations/clickup/clickup.controller.ts:109
async fn h_unlink_user(
    State(s): State<AppState>,
    ctx: RequestCtx,
    PathParams(user_uuid): PathParams<String>,
) -> Out<()> {
    Ok(Json(
        service::unlink_user(&s, ctx.require_user_id()?, &user_uuid).await,
    ))
}

/// Node: src/integrations/clickup/clickup.controller.ts:117
async fn h_linked_users(
    State(s): State<AppState>,
    ctx: RequestCtx,
) -> Out<Vec<service::LinkedUser>> {
    Ok(Json(
        service::get_linked_users(&s, ctx.require_user_id()?).await,
    ))
}

/// Node: src/integrations/clickup/clickup.controller.ts:124
async fn h_linked_project_list(
    State(s): State<AppState>,
    ctx: RequestCtx,
) -> Out<Vec<service::LinkedProjectItem>> {
    Ok(Json(
        service::get_linked_project_list(&s, ctx.require_user_id()?).await,
    ))
}

/// Node: src/integrations/clickup/clickup.controller.ts:132
async fn h_sync_tasks(
    State(s): State<AppState>,
    ctx: RequestCtx,
    PathParams(project_uuid): PathParams<String>,
    ValidatedJson(body): ValidatedJson<dto::SyncTasksBody>,
) -> Out<SyncTotals> {
    let uid = ctx.require_user_id()?;
    let (closed, subtasks) = body.flags();
    Ok(Json(
        sync_tasks(&s, uid, &project_uuid, closed, subtasks).await,
    ))
}

/// Node: src/integrations/clickup/clickup.controller.ts:150
async fn h_task_statuses(
    State(s): State<AppState>,
    ctx: RequestCtx,
    PathParams(task_uuid): PathParams<String>,
) -> Out<Vec<service::TaskStatus>> {
    Ok(Json(
        service::get_task_statuses(&s, ctx.require_user_id()?, &task_uuid).await,
    ))
}

/// Node: src/integrations/clickup/clickup.controller.ts:158
async fn h_change_task_status(
    State(s): State<AppState>,
    ctx: RequestCtx,
    PathParams(task_uuid): PathParams<String>,
    ValidatedJson(body): ValidatedJson<dto::ChangeTaskStatusBody>,
) -> Out<service::StatusChanged> {
    let uid = ctx.require_user_id()?;
    Ok(Json(
        service::change_task_status(&s, uid, &task_uuid, &body.status()).await,
    ))
}

/// Node: src/integrations/clickup/clickup.controller.ts:170
async fn h_project_tasks(
    State(s): State<AppState>,
    ctx: RequestCtx,
    PathParams(project_uuid): PathParams<String>,
    RawQuery(raw): RawQuery,
) -> Out<service::ProjectTasks> {
    let q = dto::project_tasks_query(raw.as_deref()).map_err(ApiError::Validation)?;
    let uid = ctx.require_user_id()?;
    Ok(Json(
        service::get_project_tasks(&s, uid, &project_uuid, &q).await,
    ))
}

/// `{ received: true }`.
#[derive(serde::Serialize)]
struct Received {
    received: bool,
}

/// Node: src/integrations/clickup/clickup-webhook.controller.ts:14
async fn h_webhook(
    State(s): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Json<Received>> {
    let sig = headers
        .get("x-signature")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    // Tanpa body, `express.raw` membiarkan `req.body = {}` → `Buffer.from('{}')`.
    let raw: &[u8] = if body.is_empty() { b"{}" } else { &body };
    webhook::handle(&s, sig, raw).await?;
    Ok(Json(Received { received: true }))
}
