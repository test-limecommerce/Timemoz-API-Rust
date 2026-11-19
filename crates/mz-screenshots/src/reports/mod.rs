//! Port `src/reports` (9 route): `GET /reports`, `GET /reports/export/excel`, `GET /reports/export/pdf`,
//! `POST /reports/create/share`, `GET /reports/share`, `GET /reports/share/:uuid` (PUBLIK),
//! `DELETE /reports/share/:uuid`, `POST /reports/send`,
//! `GET /reports/share/:shareUuid/screenshots/:screenshotUuid` (PUBLIK, proxy gambar share).

pub mod builder;
pub mod dto;
pub mod pdf;
pub mod service;
pub mod support;
mod xlsx;

use std::collections::HashMap;

use axum::Json;
use axum::Router;
use axum::extract::{DefaultBodyLimit, Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use http::header::{CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE, HeaderValue};
use http::{HeaderMap, StatusCode};
use mz_core::enums::{PermissionAction, Role};
use mz_core::extract::{PathParams, ValidatedJson, ValidatedQuery};
use mz_core::gate::guard;
use mz_core::rate_limit::{RateKey, RateRule};
use mz_core::{ApiError, ApiResult, AppState, RequestCtx, RouteSpec};
use serde_json::Value;

use self::dto::{CreateShareReportDto, QueryReportDto, SendReportDto};
use crate::util::parse_uuid_param;

/// `@RequireFeature('reports')` + `@UseGuards(JwtAuthGuard)`.
const SPEC: RouteSpec = RouteSpec::jwt().feature("reports");

const ALL_ROLES: &[Role] = &[
    Role::SYSTEM_ADMIN,
    Role::SUPER_ADMIN,
    Role::ADMIN,
    Role::MANAGER,
    Role::STAFF,
];

/// `export/pdf`, `DELETE share/:uuid`: `@Roles(SYSTEM_ADMIN..STAFF)` + `RolesGuard`.
const ROLES_SPEC: RouteSpec = SPEC.roles(ALL_ROLES);

/// `@Header(Content-Type xlsx)` + `@Header(Content-Disposition)` — Nest memasangnya
/// sesudah guard, jadi error interceptor/handler pun membawa header ini.
const EXCEL_SPEC: RouteSpec = SPEC.headers(&[
    (
        "content-type",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    ),
    ("content-disposition", "attachment; filename=report.xlsx"),
]);

/// `POST send`: `@Roles(SUPER_ADMIN, SYSTEM_ADMIN, ADMIN, MANAGER)` + `@RequirePermission(READ)`.
const SEND_SPEC: RouteSpec = SPEC
    .roles(&[
        Role::SUPER_ADMIN,
        Role::SYSTEM_ADMIN,
        Role::ADMIN,
        Role::MANAGER,
    ])
    .action(PermissionAction::READ);

/// `GET /reports/share/:uuid` — publik; limiter `main.ts` `rl:share-report` 30/menit per IP.
const SHARE_REPORT_LIMIT: RateRule = RateRule {
    key_prefix: "rl:share-report",
    max: Some(30),
    window_ms: 60_000,
    key: RateKey::Ip,
    pass_on_store_error: false,
};

/// Proxy gambar share — publik; limiter `main.ts` `rl:share-report-screenshot` 300/menit per IP.
const SHARE_SCREENSHOT_LIMIT: RateRule = RateRule {
    key_prefix: "rl:share-report-screenshot",
    max: Some(300),
    window_ms: 60_000,
    key: RateKey::Ip,
    pass_on_store_error: false,
};

const SHARE_PUBLIC_SPEC: RouteSpec = RouteSpec::public().rate_limits(&[SHARE_REPORT_LIMIT]);
const SHARE_SCREENSHOT_SPEC: RouteSpec = RouteSpec::public().rate_limits(&[SHARE_SCREENSHOT_LIMIT]);

/// Parser JSON bawaan Express: 100kb.
const JSON_LIMIT: usize = 100 * 1024;

const XLSX_MIME: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";

/// Node: src/reports/reports.controller.ts:55 `find`
async fn find(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedQuery(query): ValidatedQuery<QueryReportDto>,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    Ok(Json(service::find(&state, &query, user_id).await?))
}

fn file_response(mime: &'static str, filename: &str, bytes: Vec<u8>) -> ApiResult<Response> {
    let disposition = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
        .map_err(|e| ApiError::internal(anyhow::anyhow!("{e}")))?;
    let len = bytes.len();
    let mut resp = (StatusCode::OK, bytes).into_response();
    let h = resp.headers_mut();
    h.insert(CONTENT_TYPE, HeaderValue::from_static(mime));
    h.insert(CONTENT_DISPOSITION, disposition);
    h.insert(CONTENT_LENGTH, HeaderValue::from(len));
    Ok(resp)
}

/// Node: src/reports/reports.controller.ts:66 `exportExcel` (`@HttpCode(200)`)
async fn export_excel(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedQuery(query): ValidatedQuery<QueryReportDto>,
) -> ApiResult<Response> {
    let user_id = ctx.require_user_id()?;
    // Klaim kuota secara atomik; jangan hanguskan bila pembuatan file gagal.
    let claim = service::claim_export_quota(&state, user_id).await?;
    match service::generate_excel(&state, &query, user_id).await {
        Ok(bytes) => file_response(
            XLSX_MIME,
            &format!("Report-{}-to-{}.xlsx", query.start_at(), query.end_at()),
            bytes,
        ),
        Err(err) => {
            claim.release(&state.db).await;
            Err(err)
        }
    }
}

/// Node: src/reports/reports.controller.ts:106 `exportPdf` (`@HttpCode(200)`)
async fn export_pdf(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedQuery(query): ValidatedQuery<QueryReportDto>,
) -> ApiResult<Response> {
    let user_id = ctx.require_user_id()?;
    let claim = service::claim_export_quota(&state, user_id).await?;
    match service::generate_report_pdf(&state, &query, user_id).await {
        Ok(bytes) => file_response(
            "application/pdf",
            &format!("Report-{}-to-{}.pdf", query.start_at(), query.end_at()),
            bytes,
        ),
        Err(err) => {
            claim.release(&state.db).await;
            Err(err)
        }
    }
}

/// Node: src/reports/reports.controller.ts:193 `createShare` (`@HttpCode(200)`)
async fn create_share(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedJson(dto): ValidatedJson<CreateShareReportDto>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let user_id = ctx.require_user_id()?;
    Ok((
        StatusCode::OK,
        Json(service::create_share_report(&state, &dto, user_id).await?),
    ))
}

/// Node: src/reports/reports.controller.ts:207 `findAllShare` (query tanpa DTO)
async fn find_all_share(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Query(query): Query<HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    Ok(Json(
        service::find_all_share_report(&state, &query, user_id).await?,
    ))
}

/// Node: src/reports/reports.controller.ts:215 `findShareReport` (PUBLIK, `@HttpCode(200)`)
async fn find_share_report(
    State(state): State<AppState>,
    PathParams(uuid): PathParams<String>,
) -> ApiResult<Json<Value>> {
    parse_uuid_param(&uuid)?;
    Ok(Json(service::find_share_report(&state, &uuid).await))
}

/// Node: src/reports/reports.controller.ts:237 `revokeShare` (`@HttpCode(200)`)
async fn revoke_share(
    State(state): State<AppState>,
    ctx: RequestCtx,
    PathParams(uuid): PathParams<String>,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    parse_uuid_param(&uuid)?;
    Ok(Json(
        service::delete_share_report(&state, &uuid, user_id).await?,
    ))
}

/// Node: src/reports/reports.controller.ts:255 `sendReport` (`@HttpCode(200)`)
async fn send_report(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedJson(dto): ValidatedJson<SendReportDto>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let user_id = ctx.require_user_id()?;
    if !service::client_proof_export_allowed(&state, user_id).await? {
        return Err(ApiError::Plain {
            status: StatusCode::PAYMENT_REQUIRED,
            message: "Client proof report export is not available on your current plan. Upgrade to Pro to enable PDF exports."
                .into(),
        });
    }
    Ok((
        StatusCode::OK,
        Json(service::send_report(&state, user_id, &dto).await?),
    ))
}

/// Node: src/reports/share-report-screenshot.controller.ts:48 `screenshot` (PUBLIK; link share = kunci)
async fn share_screenshot(
    State(state): State<AppState>,
    PathParams((share_uuid, screenshot_uuid)): PathParams<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    parse_uuid_param(&share_uuid)?;
    parse_uuid_param(&screenshot_uuid)?;
    let key = service::resolve_share_screenshot_key(&state, &share_uuid, &screenshot_uuid).await?;
    let mut resp = mz_files::stream_object(
        &state,
        &key,
        Some(&headers),
        mz_files::StreamObjectOptions::default(),
    )
    .await?;
    // URL ber-`sig`/link share tidak bocor lewat Referer dan tidak diindeks.
    mz_files::set_unauthenticated_file_headers(resp.headers_mut());
    Ok(resp)
}

pub fn router(state: &AppState) -> Router<AppState> {
    Router::new()
        .route("/reports", guard(state, SPEC, get(find)))
        .route(
            "/reports/export/excel",
            guard(state, EXCEL_SPEC, get(export_excel)),
        )
        .route(
            "/reports/export/pdf",
            guard(state, ROLES_SPEC, get(export_pdf)),
        )
        .route(
            "/reports/create/share",
            guard(
                state,
                SPEC,
                post(create_share).layer(DefaultBodyLimit::max(JSON_LIMIT)),
            ),
        )
        .route("/reports/share", guard(state, SPEC, get(find_all_share)))
        .route(
            "/reports/share/{uuid}",
            guard(state, SHARE_PUBLIC_SPEC, get(find_share_report)),
        )
        .route(
            "/reports/share/{uuid}",
            guard(state, ROLES_SPEC, delete(revoke_share)),
        )
        .route(
            "/reports/send",
            guard(
                state,
                SEND_SPEC,
                post(send_report).layer(DefaultBodyLimit::max(JSON_LIMIT)),
            ),
        )
        .route(
            "/reports/share/{uuid}/screenshots/{screenshot_uuid}",
            guard(state, SHARE_SCREENSHOT_SPEC, get(share_screenshot)),
        )
}
