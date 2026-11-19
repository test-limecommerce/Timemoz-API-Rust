//! Port jadwal kunjungan: `admin-store-visit-plans.controller.ts` (8 route,
//! `/v1/admin/store-visit-plans`) dan `mobile-store-visit-plans.controller.ts` (2 route,
//! `/v1/mobile/store-visit-plans`). `@TableFields('stores', { path: 'data', within: 'store' })`
//! pada daftar/detail/ubah/ganti-hari admin dan daftar mobile → `mz_table_fields`.

pub mod dto;
pub mod logic;
pub mod mobile;
pub mod service;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, patch, post, put};
use axum::{Json, Router};
use mz_audit::AuditHandle;
use mz_core::enums::FeatureModule;
use mz_core::extract::{ValidatedJson, ValidatedQuery};
use mz_core::gate::guard;
use mz_core::{ApiError, ApiResult, AppState, RequestCtx, RouteSpec};
use mz_sales_geo::dto::Raw;
use mz_table_fields::apply::censor_response_body;
use mz_table_fields::policy::policy_for_request;
use serde_json::{Value, json};

/// `AdminStoreVisitPlansController`.
const ADMIN: RouteSpec = RouteSpec::jwt()
    .feature("store-visit-plans")
    .modules(&[FeatureModule::SALES_TRACKER])
    .roles(crate::ADMIN_ROLES);
/// `MobileStoreVisitPlansController`.
const MOBILE: RouteSpec = RouteSpec::jwt()
    .feature("store-visit-plans")
    .modules(&[FeatureModule::SALES_TRACKER])
    .roles(crate::MOBILE_ROLES);

pub(crate) fn routes(state: &AppState) -> Router<AppState> {
    const A: &str = "/v1/admin/store-visit-plans";
    const M: &str = "/v1/mobile/store-visit-plans";
    Router::new()
        // Path statis (`calendar`, `compliance`, `day`) menang atas `{uuid}` (urutan Nest).
        .route(A, guard(state, ADMIN, get(h_find_all)))
        .route(A, guard(state, ADMIN, post(h_create_bulk)))
        .route(
            &format!("{A}/calendar"),
            guard(state, ADMIN, get(h_calendar)),
        )
        .route(
            &format!("{A}/compliance"),
            guard(state, ADMIN, get(h_compliance)),
        )
        .route(&format!("{A}/day"), guard(state, ADMIN, put(h_replace_day)))
        .route(
            &format!("{A}/{{uuid}}"),
            guard(state, ADMIN, patch(h_update)),
        )
        .route(
            &format!("{A}/{{uuid}}"),
            guard(state, ADMIN, delete(h_remove)),
        )
        .route(
            &format!("{A}/{{uuid}}"),
            guard(state, ADMIN, get(h_find_one)),
        )
        .route(M, guard(state, MOBILE, get(h_mobile_find_all)))
        .route(
            &format!("{M}/summary"),
            guard(state, MOBILE, get(h_mobile_summary)),
        )
}

/// `@TableFields('stores', { path: 'data', within: 'store' })`.
async fn censor_stores(
    state: &AppState,
    ctx: &RequestCtx,
    mut body: Value,
) -> ApiResult<Json<Value>> {
    let censored = policy_for_request(state, ctx, false)
        .await?
        .for_table("stores");
    censor_response_body(&mut body, "stores", "data", &censored, Some("store"));
    Ok(Json(body))
}

/// Node: src/store-visits/admin-store-visit-plans.controller.ts:168 `findAll`
async fn h_find_all(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedQuery(q): ValidatedQuery<Raw>,
) -> ApiResult<Json<Value>> {
    let q = dto::parse_list(&q.0).map_err(ApiError::Validation)?;
    let body = service::find_all(&state, ctx.require_user_id()?, &q).await?;
    censor_stores(&state, &ctx, body).await
}

/// Node: src/store-visits/admin-store-visit-plans.controller.ts:203 `calendar`
async fn h_calendar(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedQuery(q): ValidatedQuery<Raw>,
) -> ApiResult<Json<Value>> {
    let f = dto::parse_calendar(&q.0).map_err(ApiError::Validation)?;
    Ok(Json(
        service::calendar(&state, ctx.require_user_id()?, &f).await?,
    ))
}

/// Node: src/store-visits/admin-store-visit-plans.controller.ts:241 `createBulk` (201)
async fn h_create_bulk(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    ValidatedJson(body): ValidatedJson<Raw>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let dto = dto::parse_create(&body.0).map_err(ApiError::Validation)?;
    let v = service::create_bulk(&state, &audit, ctx.require_user_id()?, &dto).await?;
    Ok((StatusCode::CREATED, Json(v)))
}

/// Node: src/store-visits/admin-store-visit-plans.controller.ts:323 `replaceDay` (200)
async fn h_replace_day(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    ValidatedJson(body): ValidatedJson<Raw>,
) -> ApiResult<Json<Value>> {
    let dto = dto::parse_replace_day(&body.0).map_err(ApiError::Validation)?;
    let v = service::replace_day(&state, &audit, ctx.require_user_id()?, &dto).await?;
    censor_stores(&state, &ctx, v).await
}

/// Node: src/store-visits/admin-store-visit-plans.controller.ts:402 `update`
async fn h_update(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    Path(uuid): Path<String>,
    ValidatedJson(body): ValidatedJson<Raw>,
) -> ApiResult<Json<Value>> {
    let dto = dto::parse_update(&body.0).map_err(ApiError::Validation)?;
    let data = service::update(&state, &audit, ctx.require_user_id()?, &uuid, &dto).await?;
    censor_stores(&state, &ctx, json!({ "data": data })).await
}

/// Node: src/store-visits/admin-store-visit-plans.controller.ts:463 `remove` (204)
async fn h_remove(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    Path(uuid): Path<String>,
) -> ApiResult<StatusCode> {
    service::remove(&state, &audit, ctx.require_user_id()?, &uuid).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Node: src/store-visits/admin-store-visit-plans.controller.ts:485 `compliance`
async fn h_compliance(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedQuery(q): ValidatedQuery<Raw>,
) -> ApiResult<Json<Value>> {
    let (f, g) = dto::parse_compliance(&q.0).map_err(ApiError::Validation)?;
    Ok(Json(
        service::compliance(&state, ctx.require_user_id()?, &f, g).await?,
    ))
}

/// Node: src/store-visits/admin-store-visit-plans.controller.ts:537 `findOne`
async fn h_find_one(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(uuid): Path<String>,
) -> ApiResult<Json<Value>> {
    let data = service::get_plan(&state, ctx.require_user_id()?, &uuid).await?;
    censor_stores(&state, &ctx, json!({ "data": data })).await
}

/// Node: src/store-visits/mobile-store-visit-plans.controller.ts:101 `findAll`
async fn h_mobile_find_all(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedQuery(q): ValidatedQuery<Raw>,
) -> ApiResult<Json<Value>> {
    let q = dto::parse_mobile_list(&q.0).map_err(ApiError::Validation)?;
    let body = mobile::find_all(&state, ctx.require_user_id()?, &q).await?;
    censor_stores(&state, &ctx, body).await
}

/// Node: src/store-visits/mobile-store-visit-plans.controller.ts:134 `summary`
async fn h_mobile_summary(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedQuery(q): ValidatedQuery<Raw>,
) -> ApiResult<Json<Value>> {
    let (s, e) = dto::parse_mobile_summary(&q.0).map_err(ApiError::Validation)?;
    Ok(Json(
        mobile::summary(&state, ctx.require_user_id()?, &s, &e).await?,
    ))
}
