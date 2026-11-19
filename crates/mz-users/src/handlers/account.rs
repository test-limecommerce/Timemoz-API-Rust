//! Akun & sesi: delete-account, enable/disable, list, find, update, remove, product tour,
//! check subscription.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use mz_account::user_ops;
use mz_audit::AuditHandle;
use mz_core::extract::{ValidatedJson, ValidatedQuery};
use mz_core::{ApiError, ApiResult, AppState, RequestCtx};
use serde_json::Value;

use super::UuidParam;
use crate::dto::{DeleteAccountBody, PaginatorQuery, ProductTourBody, UpdateUserBody};
use crate::service::{accounts, product_tour, subscription};

/// Node: src/users/users.controller.ts:104 (`deleteAccount`, `@HttpCode(200)`)
pub async fn delete_account(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    ValidatedJson(body): ValidatedJson<DeleteAccountBody>,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    Ok(Json(
        accounts::delete_account(&state, &audit, user_id, &body.password()).await?,
    ))
}

/// Node: src/users/users.controller.ts:119 (`disable`, tanpa `@HttpCode` -> 201)
pub async fn disable(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    UuidParam(uuid): UuidParam,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let user_id = ctx.require_user_id()?;
    Ok((
        StatusCode::CREATED,
        Json(user_ops::disable(&state, &audit, &uuid, user_id).await?),
    ))
}

/// Node: src/users/users.controller.ts:134 (`enable`, tanpa `@HttpCode` -> 201)
pub async fn enable(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    UuidParam(uuid): UuidParam,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let user_id = ctx.require_user_id()?;
    Ok((
        StatusCode::CREATED,
        Json(user_ops::enable(&state, &audit, &uuid, user_id).await?),
    ))
}

/// Node: src/users/users.controller.ts:162 (`findAll`)
pub async fn find_all(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedQuery(query): ValidatedQuery<PaginatorQuery>,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    Ok(Json(accounts::find_all(&state, user_id, &query).await?))
}

/// Node: src/users/users.controller.ts:172 (`getProductTour`)
pub async fn get_product_tour(
    State(state): State<AppState>,
    ctx: RequestCtx,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    Ok(Json(product_tour::get_product_tour(&state, user_id).await?))
}

/// Node: src/users/users.controller.ts:183 (`updateProductTour`)
pub async fn update_product_tour(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    ValidatedJson(body): ValidatedJson<ProductTourBody>,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    Ok(Json(
        product_tour::update_product_tour(
            &state,
            &audit,
            user_id,
            &body.status(),
            body.step().as_deref(),
        )
        .await?,
    ))
}

/// Node: src/users/users.controller.ts:257 (`findOne`)
pub async fn find_one(
    State(state): State<AppState>,
    ctx: RequestCtx,
    UuidParam(uuid): UuidParam,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    Ok(Json(
        accounts::find_one_by_uuid(&state, &uuid, user_id).await?,
    ))
}

/// Node: src/users/users.controller.ts:278 (`update`, `@HttpCode(200)`)
pub async fn update(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    UuidParam(uuid): UuidParam,
    ValidatedJson(body): ValidatedJson<UpdateUserBody>,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    // Angka non-integer lolos class-validator tapi ditolak Prisma (`Int`) -> 500.
    let input = body.input().ok_or_else(|| {
        ApiError::internal(anyhow::anyhow!("non-integer value for an Int column"))
    })?;
    Ok(Json(
        accounts::update(&state, &audit, &uuid, &input, user_id).await?,
    ))
}

/// Node: src/users/users.controller.ts:297 (`remove`)
pub async fn remove(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    UuidParam(uuid): UuidParam,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    Ok(Json(
        accounts::remove(&state, &audit, &uuid, user_id).await?,
    ))
}

/// Node: src/users/users.controller.ts:435 (`checkSubscription`)
pub async fn check_subscription(
    State(state): State<AppState>,
    ctx: RequestCtx,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    Ok(Json(
        subscription::check_subscription(&state, user_id).await?,
    ))
}
