//! Handler HTTP `tracking-timelines.controller.ts`. Semua route mengembalikan 200
//! (GET/PATCH/DELETE bawaan, POST ber-`@HttpCode(200)`).

use axum::Json;
use axum::extract::State;
use mz_core::extract::ValidatedJson;
use mz_core::{ApiError, ApiResult, AppState, RequestCtx};
use serde_json::{Value, json};

use crate::access::{caller_setting, controller_allows};
use crate::dto::{
    BulkBillableDto, CreateManualTimeDto, CreateTimelineDto, SplitEntryDto, SplitTimeDto,
    UpdateNoteDto, UpdateTimelineDto, UuidParam, bool_of, str_of,
};
use crate::{manual, service};

/// Node: src/tracking-timelines/tracking-timelines.controller.ts:54 `findOne`
pub async fn find_one(
    State(state): State<AppState>,
    ctx: RequestCtx,
    UuidParam(uuid, raw): UuidParam,
) -> ApiResult<Json<Value>> {
    let caller = ctx.require_user_id()?;
    Ok(Json(service::find_one(&state, uuid, &raw, caller).await?))
}

/// Node: src/tracking-timelines/tracking-timelines.controller.ts:70 `create` (`@HttpCode(200)`)
pub async fn create(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedJson(dto): ValidatedJson<CreateTimelineDto>,
) -> ApiResult<Json<Value>> {
    let caller = ctx.require_user_id()?;
    Ok(Json(service::create_offline(&state, &dto.0, caller).await))
}

/// Node: src/tracking-timelines/tracking-timelines.controller.ts:96 `createManualTime` (`@HttpCode(200)`)
pub async fn create_manual_time(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedJson(dto): ValidatedJson<CreateManualTimeDto>,
) -> ApiResult<Json<Value>> {
    let caller = ctx.require_user_id()?;
    Ok(Json(manual::create_manual_time(&state, &dto, caller).await))
}

/// `userSetting.user.userAccessLevel[0].accessLevel` yang `undefined` → TypeError → 500.
fn type_error() -> ApiError {
    ApiError::internal(anyhow::anyhow!(
        "TypeError: getCreateOfflineSetting returned no usable userAccessLevel"
    ))
}

/// Node: src/tracking-timelines/tracking-timelines.controller.ts:113 `createOffline` (`@HttpCode(200)`)
pub async fn create_offline(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedJson(dto): ValidatedJson<CreateTimelineDto>,
) -> ApiResult<Json<Value>> {
    let caller = ctx.require_user_id()?;
    if bool_of(dto.0.is_offline.as_ref()) == Some(true) {
        let setting = caller_setting(&state.db, caller, caller).await?;
        let allowed = controller_allows(setting.as_ref(), |s| s.is_allow_manual_time)
            .map_err(|()| type_error())?;
        if !allowed {
            return Ok(Json(json!({
                "success": false,
                "message": "You are not allowed to create offline tracking timeline",
            })));
        }
    }
    Ok(Json(service::create_offline(&state, &dto.0, caller).await))
}

/// Node: src/tracking-timelines/tracking-timelines.controller.ts:163 `bulkUpdateBillable` (`@HttpCode(200)`)
pub async fn bulk_update_billable(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedJson(dto): ValidatedJson<BulkBillableDto>,
) -> ApiResult<Json<Value>> {
    let caller = ctx.require_user_id()?;
    let billable = bool_of(dto.billable.as_ref()).unwrap_or(false);
    Ok(Json(
        service::bulk_update_billable(&state, &dto.uuids(), billable, caller).await?,
    ))
}

/// Node: src/tracking-timelines/tracking-timelines.controller.ts:181 `update`
/// (juga `PATCH update-offline/:uuid`, controller.ts:225 `updateOffline`).
pub async fn update(
    State(state): State<AppState>,
    ctx: RequestCtx,
    UuidParam(uuid, _): UuidParam,
    ValidatedJson(dto): ValidatedJson<UpdateTimelineDto>,
) -> ApiResult<Json<Value>> {
    let caller = ctx.require_user_id()?;
    Ok(Json(
        service::update_offline(&state, uuid, &dto.0, caller).await,
    ))
}

/// Node: src/tracking-timelines/tracking-timelines.controller.ts:205 `updateNote`
pub async fn update_note(
    State(state): State<AppState>,
    ctx: RequestCtx,
    UuidParam(uuid, raw): UuidParam,
    ValidatedJson(dto): ValidatedJson<UpdateNoteDto>,
) -> ApiResult<Json<Value>> {
    let caller = ctx.require_user_id()?;
    let note = str_of(dto.note.as_ref()).unwrap_or_default();
    Ok(Json(
        service::update_note(&state, uuid, &raw, note, caller).await?,
    ))
}

/// Node: src/tracking-timelines/tracking-timelines.controller.ts:249 `updateDashboardOffline`
pub async fn update_dashboard_offline(
    State(state): State<AppState>,
    ctx: RequestCtx,
    UuidParam(uuid, _): UuidParam,
    ValidatedJson(dto): ValidatedJson<UpdateTimelineDto>,
) -> ApiResult<Json<Value>> {
    let caller = ctx.require_user_id()?;
    let setting = caller_setting(&state.db, caller, caller).await?;
    let allowed =
        controller_allows(setting.as_ref(), |s| s.is_allow_edit_time).map_err(|()| type_error())?;
    if !allowed {
        return Ok(Json(json!({
            "success": false,
            "message": "You are not allowed to update offline tracking timeline",
        })));
    }
    Ok(Json(
        service::update_offline(&state, uuid, &dto.0, caller).await,
    ))
}

/// Node: src/tracking-timelines/tracking-timelines.controller.ts:288 `remove`
/// (juga `DELETE delete-offline/:uuid`, controller.ts:307 `removeOffline`).
pub async fn remove(
    State(state): State<AppState>,
    ctx: RequestCtx,
    UuidParam(uuid, _): UuidParam,
) -> ApiResult<Json<Value>> {
    let caller = ctx.require_user_id()?;
    Ok(Json(service::remove_offline(&state, uuid, caller).await))
}

/// Node: src/tracking-timelines/tracking-timelines.controller.ts:329 `splitEntry` (`@HttpCode(200)`)
pub async fn split_entry(
    State(state): State<AppState>,
    ctx: RequestCtx,
    UuidParam(uuid, _): UuidParam,
    ValidatedJson(dto): ValidatedJson<SplitEntryDto>,
) -> ApiResult<Json<Value>> {
    let caller = ctx.require_user_id()?;
    Ok(Json(service::split_entry(&state, uuid, &dto, caller).await))
}

/// Node: src/tracking-timelines/tracking-timelines.controller.ts:353 `splitTime` (`@HttpCode(200)`)
pub async fn split_time(
    State(state): State<AppState>,
    ctx: RequestCtx,
    UuidParam(uuid, _): UuidParam,
    ValidatedJson(dto): ValidatedJson<SplitTimeDto>,
) -> ApiResult<Json<Value>> {
    let caller = ctx.require_user_id()?;
    Ok(Json(service::split_time(&state, uuid, &dto, caller).await))
}

/// Path statis yang di Express tertelan rute `:uuid` (GET/PATCH/DELETE) →
/// `ParseUUIDPipe` 400 sesudah gerbang.
pub async fn uuid_pipe_rejection(ctx: RequestCtx) -> ApiResult<Json<Value>> {
    ctx.require_user_id()?;
    Err(ApiError::bad_request(
        "Validation failed (uuid is expected)",
    ))
}
