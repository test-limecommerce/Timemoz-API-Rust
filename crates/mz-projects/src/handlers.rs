//! Handler HTTP `src/projects/projects.controller.ts`.

use axum::Json;
use axum::extract::State;
use axum::http::HeaderMap;
use mz_core::extract::{PathParams, ValidatedJson, ValidatedQuery};
use mz_core::uuid_guard::{GuardTable, ensure_uuid_access};
use mz_core::uuid_rules::uuid_pipe as parse_uuid;
use mz_core::{ApiResult, AppState, RequestCtx};
use serde_json::Value;

use crate::dto::{
    CreateProjectBody, PaginatorQuery, ProjectHourBody, ProjectInput, UpdateProjectBody,
};
use crate::service;

/// Request dari klien desktop: token ber-`sid` dan tanpa header `Origin`
/// (`typeof req.user.sid === 'string' && !req.headers.origin`).
fn is_device_request(ctx: &RequestCtx, headers: &HeaderMap) -> bool {
    let has_origin = headers
        .get(http::header::ORIGIN)
        .is_some_and(|v| !v.is_empty());
    ctx.user().is_some_and(|u| u.sid.is_some()) && !has_origin
}

/// Node: src/projects/projects.controller.ts:55 (`findAll`)
pub async fn find_all(
    State(state): State<AppState>,
    ctx: RequestCtx,
    headers: HeaderMap,
    ValidatedQuery(query): ValidatedQuery<PaginatorQuery>,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    // Sudah dihitung RolesGuard untuk request ini (memo), tanpa kueri ulang.
    let company_id = ctx.company_id(&state).await?;
    let device = is_device_request(&ctx, &headers);
    Ok(Json(
        service::find_all(&state, user_id, company_id, &query, device).await?,
    ))
}

/// Node: src/projects/projects.controller.ts:64 (`findOne`)
pub async fn find_one(
    State(state): State<AppState>,
    ctx: RequestCtx,
    headers: HeaderMap,
    PathParams(uuid): PathParams<String>,
) -> ApiResult<Json<Value>> {
    let uuid = parse_uuid(uuid)?;
    ctx.require_user()?;
    let user_id = ctx.require_user_id()?;
    let device_poll = is_device_request(&ctx, &headers);
    Ok(Json(
        service::find_one(&state, &uuid, user_id, device_poll).await?,
    ))
}

/// Node: src/projects/projects.controller.ts:85 (`create`, `@HttpCode(200)`)
pub async fn create(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedJson(body): ValidatedJson<CreateProjectBody>,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    let input = ProjectInput::from_map(&body.0);
    Ok(Json(service::create(&state, user_id, &input).await))
}

/// Node: src/projects/projects.controller.ts:95 (`update`, `@HttpCode(200)`)
pub async fn update(
    State(state): State<AppState>,
    ctx: RequestCtx,
    PathParams(uuid): PathParams<String>,
    ValidatedJson(body): ValidatedJson<UpdateProjectBody>,
) -> ApiResult<Json<Value>> {
    let uuid = parse_uuid(uuid)?;
    let user_id = ctx.require_user_id()?;
    ensure_uuid_access(&state, &ctx, GuardTable::Project, &uuid).await?;
    let input = ProjectInput::from_map(&body.0);
    Ok(Json(service::update(&state, &uuid, user_id, &input).await?))
}

/// Node: src/projects/projects.controller.ts:119 (`delete`, `@HttpCode(200)`)
pub async fn delete(
    State(state): State<AppState>,
    ctx: RequestCtx,
    PathParams(uuid): PathParams<String>,
) -> ApiResult<Json<Value>> {
    ensure_uuid_access(&state, &ctx, GuardTable::Project, &uuid).await?;
    Ok(Json(service::delete(&state, &uuid).await?))
}

/// Node: src/projects/projects.controller.ts:139 (`createHour`, `@HttpCode(200)`)
pub async fn create_hour(
    State(state): State<AppState>,
    ctx: RequestCtx,
    PathParams(uuid): PathParams<String>,
    ValidatedJson(body): ValidatedJson<ProjectHourBody>,
) -> ApiResult<Json<Value>> {
    let uuid = parse_uuid(uuid)?;
    ensure_uuid_access(&state, &ctx, GuardTable::Project, &uuid).await?;
    Ok(Json(
        service::create_hour(&state, &uuid, body.total_hour()).await,
    ))
}

/// Node: src/projects/projects.controller.ts:162 (`updateHour`, `@HttpCode(200)`; guard `@UUID` projectHour — baru 2026-11-17)
pub async fn update_hour(
    State(state): State<AppState>,
    ctx: RequestCtx,
    PathParams(uuid): PathParams<String>,
    ValidatedJson(body): ValidatedJson<ProjectHourBody>,
) -> ApiResult<Json<Value>> {
    let uuid = parse_uuid(uuid)?;
    ensure_uuid_access(&state, &ctx, GuardTable::ProjectHour, &uuid).await?;
    Ok(Json(
        service::update_hour(&state, &uuid, body.total_hour()).await,
    ))
}

/// Node: src/projects/projects.controller.ts:178 (`deleteHour`, `@HttpCode(200)`)
pub async fn delete_hour(
    State(state): State<AppState>,
    ctx: RequestCtx,
    PathParams(uuid): PathParams<String>,
) -> ApiResult<Json<Value>> {
    ensure_uuid_access(&state, &ctx, GuardTable::ProjectHour, &uuid).await?;
    Ok(Json(service::delete_hour(&state, &uuid).await))
}
