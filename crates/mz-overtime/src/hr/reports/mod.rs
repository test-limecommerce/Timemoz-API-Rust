//! Route HR lembur: laporan (3). Node: `src/overtime/overtime-reports.controller.ts`.
//!
//! `GET /v1/overtime/reports`, `GET /v1/overtime/reports/weekly-excess`,
//! `GET /v1/overtime/reports/export` (PDF & XLSX). Tanpa STAFF dan GUEST. Laporan tetap
//! tersedia saat fitur lembur dinonaktifkan (FR-036), jadi tidak ada cek "enabled".

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::header::{CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE, HeaderValue};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use mz_core::enums::{FeatureModule, Role};
use mz_core::gate::guard;
use mz_core::{ApiError, ApiResult, AppState, RequestCtx, RouteSpec};
use serde_json::{Value, json};

use self::dto::{RawQuery, parse_query};
use self::pdf::PdfInput;

pub mod dto;
pub mod entity;
pub mod export;
#[cfg(test)]
mod fixtures;
pub mod pdf;
pub mod service;

const XLSX_MIME: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
const PDF_MIME: &str = "application/pdf";

/// `@RequireFeature('overtime-report')` + `@RequireModule(TIME_TRACKING)` +
/// `@Roles(SYSTEM_ADMIN, SUPER_ADMIN, ADMIN, MANAGER)` + `@UseGuards(JwtAuthGuard, RolesGuard)`
/// (semuanya pada kelas controller, berlaku untuk ketiga route).
const SPEC: RouteSpec = RouteSpec::jwt()
    .roles(&[
        Role::SYSTEM_ADMIN,
        Role::SUPER_ADMIN,
        Role::ADMIN,
        Role::MANAGER,
    ])
    .feature("overtime-report")
    .modules(&[FeatureModule::TIME_TRACKING]);

/// Semua route `/v1/overtime/reports*`.
pub fn router(state: &AppState) -> Router<AppState> {
    Router::new()
        .route("/v1/overtime/reports", guard(state, SPEC, get(report)))
        .route(
            "/v1/overtime/reports/weekly-excess",
            guard(state, SPEC, get(weekly)),
        )
        .route(
            "/v1/overtime/reports/export",
            guard(state, SPEC, get(export)),
        )
}

/// `GET /v1/overtime/reports` (200) — Node: src/overtime/overtime-reports.controller.ts:64
async fn report(
    State(state): State<AppState>,
    ctx: RequestCtx,
    RawQuery(pairs): RawQuery,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    let query = parse_query(&pairs, false)?;
    let data = service::report(&state, user_id, &query).await?;
    Ok(Json(json!({ "data": data })))
}

/// `GET /v1/overtime/reports/weekly-excess` (200) — Node: src/overtime/overtime-reports.controller.ts:77
async fn weekly(
    State(state): State<AppState>,
    ctx: RequestCtx,
    RawQuery(pairs): RawQuery,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    let query = parse_query(&pairs, false)?;
    let data = service::weekly(&state, user_id, &query).await?;
    Ok(Json(json!({ "data": data })))
}

/// `GET /v1/overtime/reports/export?format=xlsx|pdf` (200) — Node: src/overtime/overtime-reports.controller.ts:92
async fn export(
    State(state): State<AppState>,
    ctx: RequestCtx,
    RawQuery(pairs): RawQuery,
) -> ApiResult<Response> {
    let user_id = ctx.require_user_id()?;
    let query = parse_query(&pairs, true)?;
    let (daily, weekly, meta) = service::export_data(&state, user_id, &query).await?;
    let is_xlsx = query.format.as_deref() == Some("xlsx");
    let (ext, mime) = if is_xlsx {
        ("xlsx", XLSX_MIME)
    } else {
        ("pdf", PDF_MIME)
    };
    // Penulisan berkas CPU-bound (laporan 200 x 92 baris): jangan blokir reactor.
    let bytes = tokio::task::spawn_blocking(move || -> ApiResult<Vec<u8>> {
        if is_xlsx {
            export::to_xlsx(&daily, &weekly, &meta).map_err(ApiError::internal)
        } else {
            Ok(pdf::generate(&PdfInput {
                meta: &meta,
                daily: &daily,
                weekly: &weekly,
            }))
        }
    })
    .await
    .map_err(ApiError::internal)??;

    let filename = export::report_filename(&query.from, &query.to, ext);
    let disposition = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
        .map_err(ApiError::internal)?;
    let length = HeaderValue::from(bytes.len());
    let mut res = bytes.into_response();
    let headers = res.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(mime));
    headers.insert(CONTENT_DISPOSITION, disposition);
    headers.insert(CONTENT_LENGTH, length);
    Ok(res)
}
