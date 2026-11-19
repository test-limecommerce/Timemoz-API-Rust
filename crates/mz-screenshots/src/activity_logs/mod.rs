//! Port `src/activity-logs` (3 route): `GET /activity-logs`, `GET /activity-logs/export/csv`,
//! `POST /activity-logs`. `@RequireFeature('activity-logs')` (legacyOpen, ditangani core) +
//! `@RequireModule(TIME_TRACKING)` + `@UseGuards(JwtAuthGuard)`.

pub mod dto;
pub mod service;
pub mod window_title;

use axum::Json;
use axum::Router;
use axum::extract::{DefaultBodyLimit, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use http::StatusCode;
use http::header::{CONTENT_DISPOSITION, CONTENT_TYPE, HeaderValue};
use mz_core::enums::FeatureModule;
use mz_core::extract::{ValidatedJson, ValidatedQuery};
use mz_core::gate::guard;
use mz_core::{ApiError, ApiResult, AppState, RequestCtx, RouteSpec};
use serde_json::Value;

use self::dto::{CreateActivityLogDto, FindActivityLogsByDateDto};

const SPEC: RouteSpec = RouteSpec::jwt()
    .feature("activity-logs")
    .modules(&[FeatureModule::TIME_TRACKING]);

/// Parser JSON bawaan Express: 100kb.
const JSON_LIMIT: usize = 100 * 1024;

/// Node: src/activity-logs/activity-logs.controller.ts:40 `findByDate`
async fn find_by_date(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedQuery(query): ValidatedQuery<FindActivityLogsByDateDto>,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    Ok(Json(
        service::find_by_date(&state, &query, user_id).await?.body,
    ))
}

/// Node: src/activity-logs/activity-logs.controller.ts:54 `exportCsv`
async fn export_csv(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedQuery(query): ValidatedQuery<FindActivityLogsByDateDto>,
) -> ApiResult<Response> {
    let user_id = ctx.require_user_id()?;
    let result = service::find_by_date(&state, &query, user_id).await?;
    let csv = service::to_csv(&result.data);
    let mut resp = (StatusCode::OK, csv).into_response();
    // Express `res.send(string)` menambahkan charset ke Content-Type yang sudah di-set.
    resp.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/csv; charset=utf-8"),
    );
    let disposition = format!(
        "attachment; filename=\"activity-logs-{}.csv\"",
        result.taken_date
    );
    resp.headers_mut().insert(
        CONTENT_DISPOSITION,
        HeaderValue::from_str(&disposition)
            .map_err(|e| ApiError::internal(anyhow::anyhow!("{e}")))?,
    );
    Ok(resp)
}

/// Node: src/activity-logs/activity-logs.controller.ts:96 `logActivity` (`@HttpCode(200)`)
async fn log_activity(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedJson(dto): ValidatedJson<CreateActivityLogDto>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    ctx.require_user_id()?;
    Ok((
        StatusCode::OK,
        Json(service::log_activity(&state, &dto).await?),
    ))
}

pub fn router(state: &AppState) -> Router<AppState> {
    Router::new()
        .route("/activity-logs", guard(state, SPEC, get(find_by_date)))
        .route(
            "/activity-logs/export/csv",
            guard(state, SPEC, get(export_csv)),
        )
        .route(
            "/activity-logs",
            guard(
                state,
                SPEC,
                post(log_activity).layer(DefaultBodyLimit::max(JSON_LIMIT)),
            ),
        )
}
