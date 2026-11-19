//! Port `src/integrations/discord` (17 route `integrations/discord/*`).
//!
//! - [`router`]: route controller `discord.controller.ts` (gerbang `@RequireFeature('integrations')`
//!   level kelas, `@Roles` admin, `@RequirePermission(READ)` pada notify/test-notify/post-tasks).
//! - [`get_connection_status`]: `DiscordService.getConnectionStatus` untuk agregat
//!   `/integrations/status`.
//!
//! Semua balasan service berbentuk `{ success, message, data }` HTTP 200 — termasuk
//! kegagalan (Node menangkap semua lemparan). Deviasi: `plan/deviations/mz-int-discord.md`.

mod api;
pub mod dto;
mod handlers;
pub mod service;
pub mod util;

use axum::Router;
use axum::routing::{delete, get, post, put};
use mz_core::AppState;
use mz_core::enums::{PermissionAction, Role};
use mz_core::gate::{RouteSpec, guard};
use mz_int_common::envelope::Envelope;

pub use service::DiscordStatus;

/// Node: `discord.service.ts:218` (`getConnectionStatus`) — tidak pernah melempar.
pub async fn get_connection_status(state: &AppState, user_id: i32) -> Envelope<DiscordStatus> {
    service::connection_status(state, user_id).await
}

const ADMINS: &[Role] = &[Role::SYSTEM_ADMIN, Role::SUPER_ADMIN, Role::ADMIN];
const ADMINS_MANAGER: &[Role] = &[
    Role::SYSTEM_ADMIN,
    Role::SUPER_ADMIN,
    Role::ADMIN,
    Role::MANAGER,
];

/// `@Roles(SYSTEM_ADMIN, SUPER_ADMIN, ADMIN)` + `@UseGuards(JwtAuthGuard, RolesGuard)`.
const ADMIN: RouteSpec = RouteSpec::jwt().roles(ADMINS).feature("integrations");
/// `... @RequirePermission(PermissionAction.READ)`.
const ADMIN_READ: RouteSpec = ADMIN.action(PermissionAction::READ);
/// `post-tasks`: admin + MANAGER, `@RequirePermission(READ)`.
const POST_TASKS: RouteSpec = RouteSpec::jwt()
    .roles(ADMINS_MANAGER)
    .feature("integrations")
    .action(PermissionAction::READ);
/// `callback`: tanpa guard (dipanggil Discord); `@SkipThrottle` tidak berefek (tidak ada
/// `ThrottlerGuard` global di Node).
const PUBLIC: RouteSpec = RouteSpec::public().feature("integrations");

pub fn router(state: &AppState) -> Router<AppState> {
    use handlers as h;
    const BASE: &str = "/integrations/discord";
    let p = |s: &str| format!("{BASE}{s}");
    Router::new()
        .route(&p("/status"), guard(state, ADMIN, get(h::status)))
        .route(&p("/connect"), guard(state, ADMIN, post(h::connect)))
        .route(&p("/callback"), guard(state, PUBLIC, get(h::callback)))
        .route(
            &p("/disconnect"),
            guard(state, ADMIN, delete(h::disconnect)),
        )
        .route(&p("/channels"), guard(state, ADMIN, get(h::channels)))
        .route(
            &p("/linked-channels"),
            guard(state, ADMIN, get(h::linked_channels)),
        )
        .route(
            &p("/channels/link"),
            guard(state, ADMIN, post(h::link_channel)),
        )
        .route(
            &p("/channels/{projectUuid}/link"),
            guard(state, ADMIN, delete(h::unlink_channel)),
        )
        .route(
            &p("/projects/{projectUuid}/notify"),
            guard(state, ADMIN_READ, post(h::notify)),
        )
        .route(
            &p("/projects/{projectUuid}/test-notify"),
            guard(state, ADMIN_READ, post(h::test_notify)),
        )
        .route(
            &p("/settings/default-task-channel"),
            guard(state, ADMIN, get(h::get_default_task_channel)),
        )
        .route(
            &p("/settings/default-task-channel"),
            guard(state, ADMIN, put(h::set_default_task_channel)),
        )
        .route(
            &p("/post-tasks"),
            guard(state, POST_TASKS, post(h::post_tasks)),
        )
        .route(
            &p("/linked-users"),
            guard(state, ADMIN, get(h::linked_users)),
        )
        .route(&p("/members"), guard(state, ADMIN, get(h::members)))
        .route(&p("/users/link"), guard(state, ADMIN, post(h::link_user)))
        .route(
            &p("/users/{userUuid}/link"),
            guard(state, ADMIN, delete(h::unlink_user)),
        )
}
