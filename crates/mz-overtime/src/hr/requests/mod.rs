//! Route HR lembur: pengajuan karyawan (7). Node: `src/overtime/overtime-requests.controller.ts`.
//!
//! Dekorator kelas (berlaku untuk ketujuh route, tanpa override per method):
//! `@RequireFeature('overtime')`, `@RequireModule(TIME_TRACKING)`,
//! `@Roles(SYSTEM_ADMIN, SUPER_ADMIN, ADMIN, MANAGER, STAFF)` (tanpa GUEST),
//! `@UseGuards(JwtAuthGuard, RolesGuard)`, `@SkipThrottle()` (tak ada limiter).
//! Status: `POST /` 201, `POST :id/{cancel,accept,reject}` 200 (`@HttpCode(200)`).
//!
//! `me`/`quota` didaftarkan sebelum `:id` di Node; di axum path statis selalu menang atas
//! `{id}`. `:id` memakai `ParseUUIDPipe({ version: '4' })` → 400
//! `Validation failed (uuid v 4 is expected)` (lihat [`dto::UuidV4Id`]) — dijalankan
//! sesudah gerbang dan sebelum validasi body.
//!
//! Logika ada di [`service`].

use axum::extract::{RawQuery, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use mz_core::enums::{FeatureModule, Role};
use mz_core::extract::ValidatedJson;
use mz_core::gate::guard;
use mz_core::{ApiError, ApiResult, AppState, RequestCtx, RouteSpec};

use self::dto::{CreateOvertimeRequestBody, UuidV4Id, parse_list_mine_query, parse_quota_query};
use self::service::{DataResponse, ListResponse};
use crate::hr::common::dto::{ConfirmOverQuotaDto, RejectOvertimeDto};
use crate::hr::common::mapper::OvertimeRequestEntity;
use crate::math::QuotaEvaluation;

pub mod dto;
pub mod service;

/// `@Roles(SYSTEM_ADMIN, SUPER_ADMIN, ADMIN, MANAGER, STAFF)`.
const ROLES: &[Role] = &[
    Role::SYSTEM_ADMIN,
    Role::SUPER_ADMIN,
    Role::ADMIN,
    Role::MANAGER,
    Role::STAFF,
];

const SPEC: RouteSpec = RouteSpec::jwt()
    .roles(ROLES)
    .feature("overtime")
    .modules(&[FeatureModule::TIME_TRACKING]);

const BASE: &str = "/v1/overtime/requests";

/// Ketujuh route `/v1/overtime/requests*`.
pub fn router(state: &AppState) -> Router<AppState> {
    Router::new()
        .route(&format!("{BASE}/me"), guard(state, SPEC, get(h_list_mine)))
        .route(&format!("{BASE}/quota"), guard(state, SPEC, get(h_quota)))
        .route(BASE, guard(state, SPEC, post(h_submit)))
        .route(
            &format!("{BASE}/{{id}}"),
            guard(state, SPEC, get(h_get_one)),
        )
        .route(
            &format!("{BASE}/{{id}}/cancel"),
            guard(state, SPEC, post(h_cancel)),
        )
        .route(
            &format!("{BASE}/{{id}}/accept"),
            guard(state, SPEC, post(h_accept)),
        )
        .route(
            &format!("{BASE}/{{id}}/reject"),
            guard(state, SPEC, post(h_reject)),
        )
}

type DataJson = Json<DataResponse<OvertimeRequestEntity>>;

/// Node: src/overtime/overtime-requests.controller.ts:63 (`listMine`)
pub async fn h_list_mine(
    State(state): State<AppState>,
    ctx: RequestCtx,
    RawQuery(raw): RawQuery,
) -> ApiResult<Json<ListResponse>> {
    let query = parse_list_mine_query(raw.as_deref()).map_err(ApiError::Validation)?;
    Ok(Json(
        service::list_mine(&state, ctx.require_user_id()?, query).await?,
    ))
}

/// Node: src/overtime/overtime-requests.controller.ts:71 (`quota`)
pub async fn h_quota(
    State(state): State<AppState>,
    ctx: RequestCtx,
    RawQuery(raw): RawQuery,
) -> ApiResult<Json<DataResponse<QuotaEvaluation>>> {
    let query = parse_quota_query(raw.as_deref()).map_err(ApiError::Validation)?;
    Ok(Json(
        service::quota(&state, ctx.require_user_id()?, query).await?,
    ))
}

/// Node: src/overtime/overtime-requests.controller.ts:79 (`submit`). 201.
pub async fn h_submit(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedJson(body): ValidatedJson<CreateOvertimeRequestBody>,
) -> ApiResult<(StatusCode, DataJson)> {
    let input = body
        .to_input()
        .ok_or_else(|| ApiError::Validation(vec!["Invalid body".to_owned()]))?;
    let res = service::submit(&state, ctx.require_user_id()?, input).await?;
    Ok((StatusCode::CREATED, Json(res)))
}

/// Node: src/overtime/overtime-requests.controller.ts:101 (`getOne`)
pub async fn h_get_one(
    State(state): State<AppState>,
    ctx: RequestCtx,
    UuidV4Id(id): UuidV4Id,
) -> ApiResult<DataJson> {
    Ok(Json(
        service::get_one(&state, ctx.require_user_id()?, &id).await?,
    ))
}

/// Node: src/overtime/overtime-requests.controller.ts:110 (`cancel`). 200.
pub async fn h_cancel(
    State(state): State<AppState>,
    ctx: RequestCtx,
    UuidV4Id(id): UuidV4Id,
) -> ApiResult<DataJson> {
    Ok(Json(
        service::cancel(&state, ctx.require_user_id()?, &id).await?,
    ))
}

/// Node: src/overtime/overtime-requests.controller.ts:126 (`accept`). 200.
pub async fn h_accept(
    State(state): State<AppState>,
    ctx: RequestCtx,
    UuidV4Id(id): UuidV4Id,
    ValidatedJson(dto): ValidatedJson<ConfirmOverQuotaDto>,
) -> ApiResult<DataJson> {
    Ok(Json(
        service::accept(
            &state,
            ctx.require_user_id()?,
            &id,
            dto.confirm_over_quota(),
        )
        .await?,
    ))
}

/// Node: src/overtime/overtime-requests.controller.ts:145 (`reject`). 200.
pub async fn h_reject(
    State(state): State<AppState>,
    ctx: RequestCtx,
    UuidV4Id(id): UuidV4Id,
    ValidatedJson(dto): ValidatedJson<RejectOvertimeDto>,
) -> ApiResult<DataJson> {
    Ok(Json(
        service::reject(&state, ctx.require_user_id()?, &id, dto.reason()).await?,
    ))
}
