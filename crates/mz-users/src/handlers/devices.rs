//! Device terhubung & logout desktop.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use mz_core::extract::PathParams;
use mz_core::{ApiError, ApiResult, AppState, RequestCtx};
use serde_json::Value;

use crate::service::devices;

/// Node: src/users/users.controller.ts:311 (`removeConnected`, `@HttpCode(200)`).
/// Jalur logout desktop: `@PermissionExempt('self-service')` + `@AllowWhenSubscriptionLocked()`.
pub async fn remove_connected(
    State(state): State<AppState>,
    ctx: RequestCtx,
    PathParams(mac_address): PathParams<String>,
) -> ApiResult<Json<Value>> {
    let user = ctx.require_user()?;
    // `extractJwtFromRequest(req)` kosong -> `new UnauthorizedException()`.
    if user.raw_token.is_empty() {
        return Err(ApiError::Bare(StatusCode::UNAUTHORIZED));
    }
    let user_id = ctx.require_user_id()?;
    Ok(Json(
        devices::revoke_user_connected(
            &state,
            user_id,
            &mac_address,
            user.sid.as_deref(),
            &user.raw_token,
        )
        .await?,
    ))
}

/// Node: src/users/users.controller.ts:412 (`findConnectedDevice`)
pub async fn find_connected_device(
    State(state): State<AppState>,
    ctx: RequestCtx,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    Ok(Json(devices::find_connected_device(&state, user_id).await?))
}

/// Node: src/users/users.controller.ts:423 (`revokeConnectedDevice`, `@HttpCode(200)`).
/// Path `:id` tidak dipakai Node: semua device aktif diputus.
pub async fn revoke_connected_device(
    State(state): State<AppState>,
    ctx: RequestCtx,
    PathParams(_id): PathParams<String>,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    Ok(Json(
        devices::revoke_connected_device(&state, user_id).await?,
    ))
}
