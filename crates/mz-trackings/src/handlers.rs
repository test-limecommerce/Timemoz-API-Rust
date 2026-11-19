//! Handler HTTP `trackings.controller.ts`.

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use chrono::Utc;
use mz_core::extract::{PathParams, ValidatedJson};
use mz_core::time::to_js_iso;
use mz_core::{ApiError, ApiResult, AppState, RequestCtx};
use serde_json::Value;

use crate::dto::{
    CreateSyncLocalDto, CreateTrackingDto, QueryPairs, UpdateTrackingDto, parse_count,
    parse_paginator, parse_uuid_param,
};
use crate::service::{self, FindAllOptions};

/// Node: src/trackings/trackings.controller.ts:52 `findAll`
/// Node: method selain milik route statis (`date-time`, `sync-local-db`) jatuh ke
/// `:uuid` (`ParseUUIDPipe`) -> 400 `Validation failed (uuid is expected)`.
pub async fn static_as_uuid(ctx: RequestCtx, uri: Uri) -> ApiResult<Json<Value>> {
    ctx.require_user_id()?;
    let segment = uri.path().rsplit('/').next().unwrap_or_default();
    parse_uuid_param(segment)?;
    Err(ApiError::not_found("Cannot handle request"))
}

pub async fn find_all(
    State(state): State<AppState>,
    ctx: RequestCtx,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    let mut query = parse_paginator(&QueryPairs::from_uri(&uri))?;
    if query.taken_date.as_deref().is_none_or(str::is_empty) {
        query.taken_date = Some(to_js_iso(&Utc::now()));
    }

    // Poll desktop/mobile: token membawa `sid` string dan request tanpa `Origin`.
    let has_sid = ctx
        .user()
        .is_some_and(|u| u.claims.get("sid").is_some_and(Value::is_string));
    let has_origin = headers
        .get(axum::http::header::ORIGIN)
        .is_some_and(|v| !v.as_bytes().is_empty());
    let is_device_poll = has_sid && !has_origin;
    let opts = FindAllOptions {
        skip_screenshots: is_device_poll,
        skip_user: is_device_poll,
        skip_work_limit: is_device_poll,
        skip_integration: is_device_poll,
    };
    Ok(Json(
        service::find_all(&state, &query, user_id, opts).await?,
    ))
}

/// Node: src/trackings/trackings.controller.ts:84 `getServerDateTime`
pub async fn server_date_time(
    State(state): State<AppState>,
    ctx: RequestCtx,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    let query = parse_paginator(&QueryPairs::from_uri(&uri))?;
    service::fix(
        &state,
        query.taken_date.as_deref(),
        query.user_uuid.as_deref(),
        user_id,
    )
    .await;
    Ok(Json(service::server_date_time()))
}

/// Node: src/trackings/trackings.controller.ts:103 `findOne`
pub async fn find_one(
    State(state): State<AppState>,
    ctx: RequestCtx,
    PathParams(uuid): PathParams<String>,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    let parsed = parse_uuid_param(&uuid)?;
    Ok(Json(
        service::find_one(&state, &uuid, parsed, user_id).await?,
    ))
}

/// Node: src/trackings/trackings.controller.ts:117 `countTracking`
pub async fn count_tracking(
    State(state): State<AppState>,
    ctx: RequestCtx,
    PathParams(uuid): PathParams<String>,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    let parsed = parse_uuid_param(&uuid)?;
    let query = parse_count(&QueryPairs::from_uri(&uri))?;
    Ok(Json(
        service::count_total(&state, parsed, &query, user_id).await?,
    ))
}

/// Node: src/trackings/trackings.controller.ts:139 `create` (`@HttpCode(200)`)
pub async fn create(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedJson(dto): ValidatedJson<CreateTrackingDto>,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    // TODO(audit): tidak ada model ber-audit yang ditulis (tracking tidak diaudit).
    Ok(Json(service::create(&state, &dto, user_id).await?))
}

/// Node: src/trackings/trackings.controller.ts:158 `update`
pub async fn update(
    State(state): State<AppState>,
    ctx: RequestCtx,
    PathParams(uuid): PathParams<String>,
    ValidatedJson(dto): ValidatedJson<UpdateTrackingDto>,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    let parsed = parse_uuid_param(&uuid)?;
    Ok(Json(service::update(&state, parsed, &dto, user_id).await?))
}

/// Node: src/trackings/trackings.controller.ts:178 `remove`
pub async fn remove(
    State(state): State<AppState>,
    ctx: RequestCtx,
    PathParams(uuid): PathParams<String>,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    let parsed = parse_uuid_param(&uuid)?;
    Ok(Json(service::remove(&state, parsed, &uuid, user_id).await?))
}

/// Node: src/trackings/trackings.controller.ts:190 `syncLocalDB` (201 bawaan POST)
pub async fn sync_local_db(
    State(state): State<AppState>,
    ctx: RequestCtx,
    uri: Uri,
    ValidatedJson(body): ValidatedJson<CreateSyncLocalDto>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let user_id = ctx.require_user_id()?;
    let pairs = QueryPairs::from_uri(&uri);
    let data = match &body.data {
        Some(Value::String(s)) => s.as_str(),
        _ => "",
    };
    let out = service::create_sync_local(&state, pairs.first("takenDate"), user_id, data).await;
    Ok((StatusCode::CREATED, Json(out)))
}
