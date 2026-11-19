//! Pay rate (4), project rate (2), work time limit (3). Semua `@Roles(SUPER_ADMIN, SYSTEM_ADMIN, ADMIN)`.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use mz_core::extract::ValidatedJson;
use mz_core::{ApiResult, AppState, RequestCtx};
use serde_json::Value;

use super::{UuidPair, UuidParam};
use crate::dto::{CreatePayRateBody, SetProjectRateBody, UpdatePayRateBody, WorkTimeLimitBody};
use crate::service::rates;

/// Node: src/users/users.controller.ts:449 (`getPayRates`)
pub async fn get_pay_rates(
    State(state): State<AppState>,
    ctx: RequestCtx,
    UuidParam(uuid): UuidParam,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    Ok(Json(rates::get_pay_rates(&state, &uuid, user_id).await?))
}

/// Node: src/users/users.controller.ts:461 (`createPayRate`, `@HttpCode(201)`)
pub async fn create_pay_rate(
    State(state): State<AppState>,
    ctx: RequestCtx,
    UuidParam(uuid): UuidParam,
    ValidatedJson(body): ValidatedJson<CreatePayRateBody>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let user_id = ctx.require_user_id()?;
    Ok((
        StatusCode::CREATED,
        Json(rates::create_pay_rate(&state, &uuid, &body.input(), user_id).await?),
    ))
}

/// Node: src/users/users.controller.ts:478 (`updatePayRate`)
pub async fn update_pay_rate(
    State(state): State<AppState>,
    ctx: RequestCtx,
    UuidPair(uuid, rate_uuid): UuidPair,
    ValidatedJson(body): ValidatedJson<UpdatePayRateBody>,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    Ok(Json(
        rates::update_pay_rate(&state, &uuid, &rate_uuid, &body.input(), user_id).await?,
    ))
}

/// Node: src/users/users.controller.ts:503 (`deletePayRate`)
pub async fn delete_pay_rate(
    State(state): State<AppState>,
    ctx: RequestCtx,
    UuidPair(uuid, rate_uuid): UuidPair,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    Ok(Json(
        rates::delete_pay_rate(&state, &uuid, &rate_uuid, user_id).await?,
    ))
}

/// Node: src/users/users.controller.ts:521 (`getProjectRates`)
pub async fn get_project_rates(
    State(state): State<AppState>,
    ctx: RequestCtx,
    UuidParam(uuid): UuidParam,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    Ok(Json(
        rates::get_project_rates(&state, &uuid, user_id).await?,
    ))
}

/// Node: src/users/users.controller.ts:539 (`setProjectRate`)
pub async fn set_project_rate(
    State(state): State<AppState>,
    ctx: RequestCtx,
    UuidPair(uuid, project_uuid): UuidPair,
    ValidatedJson(body): ValidatedJson<SetProjectRateBody>,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    Ok(Json(
        rates::set_project_rate(
            &state,
            &uuid,
            &project_uuid,
            body.pay_rate(),
            body.bill_rate(),
            user_id,
        )
        .await?,
    ))
}

/// Node: src/users/users.controller.ts:563 (`getWorkTimeLimit`)
pub async fn get_work_time_limit(
    State(state): State<AppState>,
    ctx: RequestCtx,
    UuidParam(uuid): UuidParam,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    Ok(Json(
        rates::get_work_time_limit(&state, &uuid, user_id).await?,
    ))
}

/// Node: src/users/users.controller.ts:578 (`upsertWorkTimeLimit`)
pub async fn upsert_work_time_limit(
    State(state): State<AppState>,
    ctx: RequestCtx,
    UuidParam(uuid): UuidParam,
    ValidatedJson(body): ValidatedJson<WorkTimeLimitBody>,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    Ok(Json(
        rates::upsert_work_time_limit(&state, &uuid, body.daily(), body.weekly(), user_id).await?,
    ))
}

/// Node: src/users/users.controller.ts:597 (`removeWorkTimeLimit`)
pub async fn remove_work_time_limit(
    State(state): State<AppState>,
    ctx: RequestCtx,
    UuidParam(uuid): UuidParam,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    Ok(Json(
        rates::remove_work_time_limit(&state, &uuid, user_id).await?,
    ))
}
