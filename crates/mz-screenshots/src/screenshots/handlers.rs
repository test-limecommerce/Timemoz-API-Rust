//! Handler HTTP `screenshots.controller.ts`.

use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use http::header::{CACHE_CONTROL, HeaderValue};
use http::{HeaderMap, StatusCode};
use mz_core::extract::{PathParams, ValidatedJson, ValidatedQuery};
use mz_core::{ApiResult, AppState, RequestCtx};
use mz_files::SIGNED_URL_RESPONSE_CACHE_CONTROL;
use serde_json::Value;

use super::dto::{CreateScreenshotDto, ScreenshotImageQueryDto, UpdateScreenshotDto};
use super::service;
use crate::util::parse_uuid_param;

/// Node: src/screenshots/screenshots.controller.ts:57 `findOne`
pub async fn find_one(
    State(state): State<AppState>,
    ctx: RequestCtx,
    PathParams(uuid): PathParams<String>,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    parse_uuid_param(&uuid)?;
    Ok(Json(service::find_one(&state, &uuid, user_id).await?))
}

/// Node: src/screenshots/screenshots.controller.ts:77 `create` (`@HttpCode(200)`)
pub async fn create(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedJson(dto): ValidatedJson<CreateScreenshotDto>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let user_id = ctx.require_user_id()?;
    Ok((
        StatusCode::OK,
        Json(service::create(&state, &dto, user_id).await),
    ))
}

/// Node: src/screenshots/screenshots.controller.ts:114 `update` (PATCH, 200)
pub async fn update(
    State(state): State<AppState>,
    ctx: RequestCtx,
    PathParams(uuid): PathParams<String>,
    ValidatedJson(dto): ValidatedJson<UpdateScreenshotDto>,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    parse_uuid_param(&uuid)?;
    Ok(Json(service::update(&state, &uuid, &dto, user_id).await?))
}

/// Node: src/screenshots/screenshots.controller.ts:147 `remove`
pub async fn remove(
    State(state): State<AppState>,
    ctx: RequestCtx,
    PathParams(uuid): PathParams<String>,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    parse_uuid_param(&uuid)?;
    Ok(Json(service::remove(&state, &uuid, user_id).await?))
}

/// Node: src/screenshots/screenshots.controller.ts:170 `blurImage` (PUT blur/:uuid)
pub async fn blur_image(
    State(state): State<AppState>,
    ctx: RequestCtx,
    PathParams(uuid): PathParams<String>,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    parse_uuid_param(&uuid)?;
    Ok(Json(service::blur(&state, &uuid, user_id).await?))
}

/// Node: src/screenshots/screenshots.controller.ts:190 `getOpenUrl`
pub async fn get_open_url(
    State(state): State<AppState>,
    ctx: RequestCtx,
    PathParams(uuid): PathParams<String>,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    parse_uuid_param(&uuid)?;
    Ok(Json(service::get_open_url(&state, &uuid, user_id).await?))
}

/// Node: src/screenshots/screenshots.controller.ts:229 `image` (proxy ber-JWT; sengaja TANPA limiter)
pub async fn image(
    State(state): State<AppState>,
    ctx: RequestCtx,
    PathParams(uuid): PathParams<String>,
    ValidatedQuery(query): ValidatedQuery<ScreenshotImageQueryDto>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let user_id = ctx.require_user_id()?;
    parse_uuid_param(&uuid)?;
    service::stream_image(&state, &uuid, query.variant(), user_id, &headers).await
}

/// Node: src/screenshots/screenshots.controller.ts:268 `imageUrl`
/// (`@Header('Cache-Control', SIGNED_URL_RESPONSE_CACHE_CONTROL)`, hanya pada respons sukses)
pub async fn image_url(
    State(state): State<AppState>,
    ctx: RequestCtx,
    PathParams(uuid): PathParams<String>,
    ValidatedQuery(query): ValidatedQuery<ScreenshotImageQueryDto>,
) -> ApiResult<Response> {
    let user_id = ctx.require_user_id()?;
    parse_uuid_param(&uuid)?;
    let body = service::image_url(&state, &uuid, query.variant(), user_id).await?;
    let mut resp = Json(body).into_response();
    resp.headers_mut().insert(
        CACHE_CONTROL,
        HeaderValue::from_static(SIGNED_URL_RESPONSE_CACHE_CONTROL),
    );
    Ok(resp)
}
