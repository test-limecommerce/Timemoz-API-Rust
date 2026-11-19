//! Route HR lembur: approvals (7). Node: `src/overtime/overtime-approvals.controller.ts`.
//!
//! Gerbang kelas: `@RequireFeature('overtime')`, `@RequireModule(TIME_TRACKING)`,
//! `@Roles(SYSTEM_ADMIN, SUPER_ADMIN, ADMIN, MANAGER)` + `JwtAuthGuard, RolesGuard`,
//! `@SkipThrottle()` (tanpa limiter). `GET quota` dan `GET :id` mengulang `@Roles` +
//! `@UseGuards` yang SAMA di method → gerbang identik untuk ketujuh route. Verb izin dari
//! method (GET → READ, POST → WRITE).
//!
//! `:id` memakai `ParseUUIDPipe({ version: '4' })` → 400 `Validation failed (uuid v 4 is
//! expected)` setelah gerbang, sebelum body divalidasi. Di Nest, `GET /approvals/orders`
//! jatuh ke `GET :id` (400 pipe); di sini didaftarkan eksplisit agar sama (bukan 404).

use axum::extract::{RawQuery, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use mz_core::enums::{FeatureModule, Role};
use mz_core::extract::ValidatedJson;
use mz_core::gate::guard;
use mz_core::{ApiError, ApiResult, AppState, RequestCtx, RouteSpec};

use self::dto::{CreateOrderBody, UUID_V4_MESSAGE, UuidV4Id, parse_list_query, parse_quota_query};
use self::entity::{ApprovalDetail, ApprovalList, Data, OrderResult, QuotaEntry};
use self::rules::ListMode;
use super::common::dto::{ConfirmOverQuotaDto, RejectOvertimeDto};
use super::common::mapper::OvertimeRequestEntity;

pub mod dto;
pub mod entity;
pub mod rules;
pub mod service;

/// `@Roles(SYSTEM_ADMIN, SUPER_ADMIN, ADMIN, MANAGER)` (tanpa STAFF dan GUEST).
pub const APPROVAL_ROLES: &[Role] = &[
    Role::SYSTEM_ADMIN,
    Role::SUPER_ADMIN,
    Role::ADMIN,
    Role::MANAGER,
];

const SPEC: RouteSpec = RouteSpec::jwt()
    .roles(APPROVAL_ROLES)
    .feature("overtime")
    .modules(&[FeatureModule::TIME_TRACKING]);

const BASE: &str = "/v1/overtime/approvals";

/// Route `/v1/overtime/approvals*`.
pub fn router(state: &AppState) -> Router<AppState> {
    Router::new()
        .route(BASE, guard(state, SPEC, get(h_list)))
        .route(
            "/v1/overtime/approvals/history",
            guard(state, SPEC, get(h_history)),
        )
        .route(
            "/v1/overtime/approvals/quota",
            guard(state, SPEC, get(h_quota)),
        )
        .route(
            "/v1/overtime/approvals/orders",
            guard(state, SPEC, post(h_create_orders)),
        )
        .route(
            "/v1/overtime/approvals/orders",
            guard(state, SPEC, get(h_get_one_orders)),
        )
        .route(
            "/v1/overtime/approvals/{id}",
            guard(state, SPEC, get(h_get_one)),
        )
        .route(
            "/v1/overtime/approvals/{id}/approve",
            guard(state, SPEC, post(h_approve)),
        )
        .route(
            "/v1/overtime/approvals/{id}/reject",
            guard(state, SPEC, post(h_reject)),
        )
}

/// Node: src/overtime/overtime-approvals.controller.ts:68 (`list`)
pub async fn h_list(
    State(state): State<AppState>,
    ctx: RequestCtx,
    RawQuery(raw): RawQuery,
) -> ApiResult<Json<ApprovalList>> {
    let q = parse_list_query(raw.as_deref()).map_err(ApiError::Validation)?;
    let user_id = ctx.require_user_id()?;
    Ok(Json(
        service::list_rows(&state, user_id, &q, ListMode::Pending).await?,
    ))
}

/// Node: src/overtime/overtime-approvals.controller.ts:76 (`history`)
pub async fn h_history(
    State(state): State<AppState>,
    ctx: RequestCtx,
    RawQuery(raw): RawQuery,
) -> ApiResult<Json<ApprovalList>> {
    let q = parse_list_query(raw.as_deref()).map_err(ApiError::Validation)?;
    let user_id = ctx.require_user_id()?;
    Ok(Json(
        service::list_rows(&state, user_id, &q, ListMode::History).await?,
    ))
}

/// Node: src/overtime/overtime-approvals.controller.ts:93 (`quota`)
pub async fn h_quota(
    State(state): State<AppState>,
    ctx: RequestCtx,
    RawQuery(raw): RawQuery,
) -> ApiResult<Json<Data<Vec<QuotaEntry>>>> {
    let q = parse_quota_query(raw.as_deref()).map_err(ApiError::Validation)?;
    let user_id = ctx.require_user_id()?;
    Ok(Json(Data {
        data: service::order_quota(&state, user_id, &q).await?,
    }))
}

/// Node: src/overtime/overtime-approvals.controller.ts:112 (`createOrders`). 201.
pub async fn h_create_orders(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedJson(body): ValidatedJson<CreateOrderBody>,
) -> ApiResult<(axum::http::StatusCode, Json<Data<OrderResult>>)> {
    let user_id = ctx.require_user_id()?;
    let result = service::create_orders(&state, user_id, &body.to_input()).await?;
    Ok((axum::http::StatusCode::CREATED, Json(Data { data: result })))
}

/// Node: src/overtime/overtime-approvals.controller.ts:128 (`getOne`) untuk `id = "orders"`:
/// `ParseUUIDPipe` menolak setelah gerbang.
pub async fn h_get_one_orders() -> ApiResult<Json<Data<ApprovalDetail>>> {
    Err(ApiError::bad_request(UUID_V4_MESSAGE))
}

/// Node: src/overtime/overtime-approvals.controller.ts:128 (`getOne`)
pub async fn h_get_one(
    State(state): State<AppState>,
    ctx: RequestCtx,
    UuidV4Id(id): UuidV4Id,
) -> ApiResult<Json<Data<ApprovalDetail>>> {
    let user_id = ctx.require_user_id()?;
    Ok(Json(Data {
        data: service::get_one(&state, user_id, &id).await?,
    }))
}

/// Node: src/overtime/overtime-approvals.controller.ts:147 (`approve`). 200.
pub async fn h_approve(
    State(state): State<AppState>,
    ctx: RequestCtx,
    UuidV4Id(id): UuidV4Id,
    ValidatedJson(dto): ValidatedJson<ConfirmOverQuotaDto>,
) -> ApiResult<Json<Data<OvertimeRequestEntity>>> {
    let user_id = ctx.require_user_id()?;
    Ok(Json(Data {
        data: service::approve(&state, user_id, &id, dto.confirm_over_quota()).await?,
    }))
}

/// Node: src/overtime/overtime-approvals.controller.ts:168 (`reject`). 200.
pub async fn h_reject(
    State(state): State<AppState>,
    ctx: RequestCtx,
    UuidV4Id(id): UuidV4Id,
    ValidatedJson(dto): ValidatedJson<RejectOvertimeDto>,
) -> ApiResult<Json<Data<OvertimeRequestEntity>>> {
    let user_id = ctx.require_user_id()?;
    Ok(Json(Data {
        data: service::reject(&state, user_id, &id, &dto.reason()).await?,
    }))
}
