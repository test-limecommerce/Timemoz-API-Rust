//! Port `src/divisions` dari Mudah-API: 11 route `/v1/divisions*` (Employee Divisions, specs/006).
//!
//! Controller: `@RequireFeature('divisions')`, `@RequireModule(TIME_TRACKING)`,
//! `@RequirePlanFeature(DIVISIONS)`, `@SkipThrottle()`, `@UseGuards(JwtAuthGuard, RolesGuard)`;
//! `@Roles` dan `@RequirePermission` per handler. Route statis (`employees`, `my-scope`) menang
//! atas `:uuid`; method lain pada path statis itu jatuh ke `:uuid` (`ParseUUIDPipe` → 400), dan
//! method yang tidak terdaftar sama sekali = 404 `Cannot X /path` (fallback `mz-server`).

#![cfg_attr(test, allow(clippy::indexing_slicing))]

mod consts;
mod division_time;
mod dto;
mod entities;
mod errors;
mod extract;
mod queries;
mod read;
mod scope;
mod write;

pub use division_time::{MemberStatus, company_day_and_week, member_status, round_average};
pub use errors::{DivError, map_unique_violation};
pub use scope::{Scope, ScopeDivision, can_lead_division, resolve_viewer_scope};

use axum::Router;
use axum::http::Uri;
use axum::routing::{delete, get, patch, post};
use mz_core::enums::{FeatureModule, PermissionAction};
use mz_core::gate::guard;
use mz_core::plan::PlanFeature;
use mz_core::uuid_rules::uuid_pipe;
use mz_core::{AppState, RequestCtx, RouteSpec};

use consts::{READ_ROLES, VIEW_ROLES, WRITE_ROLES};
use errors::DivResult;

/// Kelas controller: fitur `divisions`, modul TIME_TRACKING, paket DIVISIONS.
const BASE: RouteSpec = RouteSpec::jwt()
    .feature("divisions")
    .modules(&[FeatureModule::TIME_TRACKING])
    .plan_features(&[PlanFeature::Divisions]);

const READ: RouteSpec = BASE.roles(READ_ROLES).action(PermissionAction::READ);
const VIEW: RouteSpec = BASE.roles(VIEW_ROLES).action(PermissionAction::READ);
const WRITE: RouteSpec = BASE.roles(WRITE_ROLES).action(PermissionAction::WRITE);
const DELETE: RouteSpec = BASE.roles(WRITE_ROLES).action(PermissionAction::DELETE);

/// Method tulis pada path statis (`employees`, `my-scope`) yang di Node ditelan `:uuid`:
/// guard `@Roles`/`@RequirePermission` milik handler `:uuid`, lalu `ParseUUIDPipe` → 400.
async fn static_as_uuid(_ctx: RequestCtx, uri: Uri) -> DivResult<()> {
    let segment = uri.path().rsplit('/').next().unwrap_or_default().to_owned();
    uuid_pipe(segment)?;
    Err(errors::internal("static segment accepted as uuid"))
}

pub fn router(state: &AppState) -> Router<AppState> {
    Router::new()
        .route("/v1/divisions", guard(state, READ, get(read::h_list)))
        .route("/v1/divisions", guard(state, WRITE, post(write::h_create)))
        .route(
            "/v1/divisions/employees",
            guard(state, VIEW, get(read::h_employees)),
        )
        .route(
            "/v1/divisions/employees",
            guard(state, WRITE, patch(static_as_uuid)),
        )
        .route(
            "/v1/divisions/employees",
            guard(state, DELETE, delete(static_as_uuid)),
        )
        .route(
            "/v1/divisions/my-scope",
            guard(state, READ, get(read::h_my_scope)),
        )
        .route(
            "/v1/divisions/my-scope",
            guard(state, WRITE, patch(static_as_uuid)),
        )
        .route(
            "/v1/divisions/my-scope",
            guard(state, DELETE, delete(static_as_uuid)),
        )
        .route(
            "/v1/divisions/{uuid}",
            guard(state, READ, get(read::h_detail)),
        )
        .route(
            "/v1/divisions/{uuid}",
            guard(state, WRITE, patch(write::h_update)),
        )
        .route(
            "/v1/divisions/{uuid}",
            guard(state, DELETE, delete(write::h_remove)),
        )
        .route(
            "/v1/divisions/{uuid}/tracking",
            guard(state, READ, get(read::h_tracking)),
        )
        .route(
            "/v1/divisions/{uuid}/report",
            guard(state, READ, get(read::h_report)),
        )
        .route(
            "/v1/divisions/{uuid}/members/{user_uuid}",
            guard(state, WRITE, patch(write::h_set_member_role)),
        )
        .route(
            "/v1/divisions/{uuid}/members/{user_uuid}",
            guard(state, DELETE, delete(write::h_remove_member)),
        )
}
