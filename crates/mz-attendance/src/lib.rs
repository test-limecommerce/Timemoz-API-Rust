//! Port modul absensi Mudah-API:
//! - `src/mobile-clock/**` — 11 route `/mobile/clock/**` + 7 route `/v1/admin/clock/**`
//!   (generator Excel & PDF absensi, `previous-day-shift.ts`, `selfie-punch-type.ts`);
//! - `src/mobile-kyc/**` — 2 route `/mobile/kyc/face`;
//! - `src/mobile-company-config/**` — `GET /mobile/company/config`,
//!   `GET /v1/mobile/companies/:companyUuid/logo` (tanpa JWT, URL bertanda tangan);
//! - `src/mobile-dashboard/**` — `GET /mobile/dashboard/summary`.
//!
//! Semua controller ber-`@SkipThrottle()`. Deviasi: `plan/deviations/mz-attendance.md`.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic
    )
)]

pub mod attendance;
pub mod body;
pub mod clock;
pub mod company_config;
pub mod dashboard;
pub mod geo;
pub mod kyc;
mod pdf;
pub mod report_pdf;
pub mod schedule;
pub mod selfie;
pub mod xlsx;

use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use mz_core::enums::Role;
use mz_core::permissions::Exempt;
use mz_core::rate_limit::{RateKey, RateRule};
use mz_core::uuid_rules::uuid_pipe;
use mz_core::{ApiError, ApiResult, AppState, RequestCtx, RouteSpec};
use serde_json::{Map, Value, json};

use body::{
    AttendanceQuery, BreakStartDto, ClockForm, ClockInDto, ClockOutDto, GpsStatusDto, QueryKind,
};

const MOBILE_ROLES: &[Role] = &[
    Role::SYSTEM_ADMIN,
    Role::SUPER_ADMIN,
    Role::ADMIN,
    Role::MANAGER,
    Role::STAFF,
];
const ADMIN_ROLES: &[Role] = &[Role::SYSTEM_ADMIN, Role::SUPER_ADMIN, Role::ADMIN];
const ADMIN_CLOCK_ROLES: &[Role] = &[
    Role::SYSTEM_ADMIN,
    Role::SUPER_ADMIN,
    Role::ADMIN,
    Role::MANAGER,
];

/// `MobileClockController` (`@RequireFeature('clock')`).
const CLOCK: RouteSpec = RouteSpec::jwt().feature("clock").roles(MOBILE_ROLES);
/// `POST /mobile/clock/auto-close`.
const CLOCK_AUTO_CLOSE: RouteSpec = RouteSpec::jwt().feature("clock").roles(ADMIN_ROLES);
/// `AdminClockController` (role kelas: SYSTEM_ADMIN, SUPER_ADMIN, ADMIN, MANAGER).
const ADMIN_CLOCK: RouteSpec = RouteSpec::jwt().feature("clock").roles(ADMIN_CLOCK_ROLES);
/// `GET shifts/:shiftId/selfie/url` — `@Header('Cache-Control', SIGNED_URL_RESPONSE_CACHE_CONTROL)`.
const ADMIN_SELFIE_URL: RouteSpec =
    ADMIN_CLOCK.headers(&[("cache-control", mz_files::SIGNED_URL_RESPONSE_CACHE_CONTROL)]);
/// `MobileKycController` (`@RequireFeature('kyc')`).
const KYC: RouteSpec = RouteSpec::jwt().feature("kyc").roles(MOBILE_ROLES);
/// `MobileCompanyConfigController.getConfig` (`@AllowWhenSubscriptionLocked()`).
const CONFIG: RouteSpec = RouteSpec::jwt()
    .feature("company-settings")
    .roles(MOBILE_ROLES)
    .allow_locked();
/// `main.ts` `mobileFileLimiter`: 300/menit per IP, `rl:mobile-signed-file`.
const MOBILE_FILE_LIMIT: RateRule = RateRule {
    key_prefix: "rl:mobile-signed-file",
    max: Some(300),
    window_ms: 60_000,
    key: RateKey::Ip,
    pass_on_store_error: false,
};
/// `MobileCompanyLogoFileController` — tanpa JWT, `@PermissionExempt('no-user')`.
const LOGO: RouteSpec = RouteSpec::public()
    .exempt(Exempt::NoUser)
    .rate_limits(&[MOBILE_FILE_LIMIT]);
/// `MobileDashboardController` (`@RequireFeature('clock')`).
const DASHBOARD: RouteSpec = RouteSpec::jwt().feature("clock").roles(MOBILE_ROLES);

/// Multer tanpa `limits` (tak terbatas). Batas wajar untuk selfie; lihat deviasi.
const UPLOAD_LIMIT: usize = 50 * 1024 * 1024;

fn created(v: Value) -> Response {
    (StatusCode::CREATED, Json(json!({ "data": v }))).into_response()
}

fn ok_data(v: Value) -> Json<Value> {
    Json(json!({ "data": v }))
}

fn query_map(uri: &Uri) -> Map<String, Value> {
    body::qs_object(uri.query().unwrap_or_default())
}

/// Node: src/mobile-clock/mobile-clock.controller.ts:155 `clockIn` (201).
async fn h_clock_in(
    State(state): State<AppState>,
    ctx: RequestCtx,
    form: ClockForm,
) -> ApiResult<Response> {
    let user_id = ctx.require_user_id()?;
    let dto = ClockInDto::parse(&form.fields)?;
    let data = clock::clock_in(&state, user_id, &dto, form.photo.as_ref()).await?;
    Ok(created(data))
}

/// Node: src/mobile-clock/mobile-clock.controller.ts:228 `clockOut` (`@HttpCode(200)`).
async fn h_clock_out(
    State(state): State<AppState>,
    ctx: RequestCtx,
    form: ClockForm,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    let dto = ClockOutDto::parse(&form.fields)?;
    Ok(ok_data(
        clock::clock_out(&state, user_id, &dto, form.photo.as_ref()).await?,
    ))
}

/// Node: src/mobile-clock/mobile-clock.controller.ts:276 `getCurrent`.
async fn h_current(State(state): State<AppState>, ctx: RequestCtx) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    Ok(ok_data(clock::get_current(&state, user_id).await?))
}

/// Node: src/mobile-clock/mobile-clock.controller.ts:302 `autoClose` (`@HttpCode(200)`).
async fn h_auto_close(
    State(state): State<AppState>,
    ctx: RequestCtx,
    req: Request,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    let fields = body::read_plain_body(req).await?;
    let threshold = body::parse_auto_close(&fields)?;
    Ok(ok_data(
        clock::auto_close(&state, user_id, threshold).await?,
    ))
}

/// Node: src/mobile-clock/mobile-clock.controller.ts:299 `reportGpsStatus` (201).
async fn h_gps_status(
    State(state): State<AppState>,
    ctx: RequestCtx,
    req: Request,
) -> ApiResult<Response> {
    let user_id = ctx.require_user_id()?;
    let fields = body::read_plain_body(req).await?;
    let dto = GpsStatusDto::parse(&fields)?;
    Ok(created(
        clock::report_gps_status(&state, user_id, &dto).await?,
    ))
}

/// Node: src/mobile-clock/mobile-clock.controller.ts:349 `breakStart` (201).
async fn h_break_start(
    State(state): State<AppState>,
    ctx: RequestCtx,
    req: Request,
) -> ApiResult<Response> {
    let user_id = ctx.require_user_id()?;
    let fields = body::read_plain_body(req).await?;
    let dto = BreakStartDto::parse(&fields)?;
    Ok(created(clock::break_start(&state, user_id, &dto).await?))
}

/// Node: src/mobile-clock/mobile-clock.controller.ts:385 `breakEnd` (`@HttpCode(200)`).
async fn h_break_end(State(state): State<AppState>, ctx: RequestCtx) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    Ok(ok_data(clock::break_end(&state, user_id).await?))
}

/// Node: src/mobile-clock/mobile-clock.controller.ts:415 `getBreakCurrent`.
async fn h_break_current(State(state): State<AppState>, ctx: RequestCtx) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    Ok(ok_data(clock::break_current(&state, user_id).await?))
}

/// Node: src/mobile-clock/mobile-clock.controller.ts:446 `getBreakPolicy`.
async fn h_break_policy(State(state): State<AppState>, ctx: RequestCtx) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    Ok(ok_data(clock::break_policy(&state, user_id).await?))
}

/// Node: src/mobile-clock/mobile-clock.controller.ts:512 `getMyAttendance` (tanpa bungkus `data`).
async fn h_my_attendance(
    State(state): State<AppState>,
    ctx: RequestCtx,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    let q = AttendanceQuery::parse(uri.query(), QueryKind::Mobile)?;
    Ok(Json(attendance::my_attendance(&state, user_id, &q).await?))
}

/// Node: src/mobile-clock/mobile-clock.controller.ts:528 `streamSelfie` (milik sendiri).
async fn h_my_selfie(
    State(state): State<AppState>,
    ctx: RequestCtx,
    axum::extract::Path(shift_id): axum::extract::Path<String>,
    uri: Uri,
) -> ApiResult<Response> {
    let user_id = ctx.require_user_id()?;
    let punch = selfie::parse_mobile_punch(query_map(&uri).get("type"));
    let key = selfie::own_key(&state, user_id, &shift_id, punch).await?;
    selfie::send_jpeg(&state, &key).await
}

/// Node: src/mobile-clock/admin-clock.controller.ts:113 `getAttendance`.
async fn h_admin_attendance(
    State(state): State<AppState>,
    ctx: RequestCtx,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    let q = AttendanceQuery::parse(uri.query(), QueryKind::Admin)?;
    Ok(Json(
        attendance::admin_attendance(&state, user_id, &q).await?,
    ))
}

/// Node: src/mobile-clock/admin-clock.controller.ts:255 `getAttendanceSelfie`.
async fn h_admin_attendance_selfie(
    State(state): State<AppState>,
    ctx: RequestCtx,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    let q = AttendanceQuery::parse(uri.query(), QueryKind::Admin)?;
    Ok(Json(
        attendance::admin_attendance_selfie(&state, user_id, &q).await?,
    ))
}

fn file_response(mime: &'static str, filename: &str, bytes: Vec<u8>) -> ApiResult<Response> {
    let disposition = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
        .map_err(|e| ApiError::internal(anyhow::anyhow!("{e}")))?;
    let len = bytes.len();
    let mut resp = (StatusCode::OK, bytes).into_response();
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    h.insert(header::CONTENT_DISPOSITION, disposition);
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    Ok(resp)
}

/// Node: src/mobile-clock/admin-clock.controller.ts:267 `exportAttendanceExcel`.
async fn h_export_excel(
    State(state): State<AppState>,
    ctx: RequestCtx,
    uri: Uri,
) -> ApiResult<Response> {
    let user_id = ctx.require_user_id()?;
    let q = AttendanceQuery::parse(uri.query(), QueryKind::Export)?;
    let data = attendance::export_data(&state, user_id, &q).await?;
    let bytes = xlsx::build(&data).map_err(ApiError::internal)?;
    file_response(
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        &format!("attendance-{}-to-{}.xlsx", q.start_date, q.end_date),
        bytes,
    )
}

/// Node: src/mobile-clock/admin-clock.controller.ts:290 `exportAttendancePdf`.
async fn h_export_pdf(
    State(state): State<AppState>,
    ctx: RequestCtx,
    uri: Uri,
) -> ApiResult<Response> {
    let user_id = ctx.require_user_id()?;
    let q = AttendanceQuery::parse(uri.query(), QueryKind::Export)?;
    let data = attendance::export_data(&state, user_id, &q).await?;
    let bytes = tokio::task::spawn_blocking(move || report_pdf::build(&data))
        .await
        .map_err(ApiError::internal)?;
    file_response(
        "application/pdf",
        &format!("attendance-{}-to-{}.pdf", q.start_date, q.end_date),
        bytes,
    )
}

/// Node: src/mobile-clock/admin-clock.controller.ts:394 `getAttendanceDetail`.
async fn h_admin_detail(
    State(state): State<AppState>,
    ctx: RequestCtx,
    axum::extract::Path(shift_id): axum::extract::Path<String>,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    Ok(ok_data(
        attendance::admin_attendance_detail(&state, user_id, &shift_id).await?,
    ))
}

/// Node: src/mobile-clock/admin-clock.controller.ts:420 `streamSelfie` (`ParseUUIDPipe`).
async fn h_admin_selfie(
    State(state): State<AppState>,
    ctx: RequestCtx,
    axum::extract::Path(shift_id): axum::extract::Path<String>,
    uri: Uri,
) -> ApiResult<Response> {
    let user_id = ctx.require_user_id()?;
    let shift_id = uuid_pipe(shift_id)?;
    let punch = selfie::parse_admin_punch(query_map(&uri).get("type"))?;
    let key = selfie::admin_key(&state, user_id, &shift_id, punch).await?;
    selfie::send_jpeg(&state, &key).await
}

/// Node: src/mobile-clock/admin-clock.controller.ts:452 `selfieUrl`.
async fn h_admin_selfie_url(
    State(state): State<AppState>,
    ctx: RequestCtx,
    axum::extract::Path(shift_id): axum::extract::Path<String>,
    uri: Uri,
) -> ApiResult<Json<mz_files::SignedReadUrl>> {
    let user_id = ctx.require_user_id()?;
    let shift_id = uuid_pipe(shift_id)?;
    let punch = selfie::parse_admin_punch(query_map(&uri).get("type"))?;
    let key = selfie::admin_key(&state, user_id, &shift_id, punch).await?;
    Ok(Json(selfie::signed_url(&state, &key).await?))
}

/// Node: src/mobile-kyc/mobile-kyc.controller.ts:54 `saveFace` (`@HttpCode(200)`).
async fn h_kyc_save(
    State(state): State<AppState>,
    ctx: RequestCtx,
    req: Request,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    let fields = body::read_plain_body(req).await?;
    let faces = body::parse_save_face(&fields)?;
    Ok(ok_data(kyc::save_face(&state, user_id, &faces).await?))
}

/// Node: src/mobile-kyc/mobile-kyc.controller.ts:80 `getFace`.
async fn h_kyc_get(State(state): State<AppState>, ctx: RequestCtx) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    Ok(ok_data(kyc::get_face(&state, user_id).await?))
}

/// Node: src/mobile-company-config/mobile-company-config.controller.ts:78 `getConfig`.
async fn h_config(State(state): State<AppState>, ctx: RequestCtx) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    Ok(Json(company_config::get_config(&state, user_id).await?))
}

/// Node: src/mobile-company-config/mobile-company-logo-file.controller.ts:64 `logo`.
async fn h_logo(
    State(state): State<AppState>,
    axum::extract::Path(company_uuid): axum::extract::Path<String>,
    uri: Uri,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let company_uuid = uuid_pipe(company_uuid)?;
    let q = query_map(&uri);
    let as_str = |k: &str| match q.get(k) {
        Some(Value::String(s)) => Some(s.as_str()),
        _ => None,
    };
    Ok(company_config::stream_logo(
        &state,
        &company_uuid,
        as_str("exp"),
        as_str("sig"),
        &headers,
    )
    .await)
}

/// Node: src/mobile-dashboard/mobile-dashboard.controller.ts:152 `getSummary`.
async fn h_dashboard(State(state): State<AppState>, ctx: RequestCtx) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    Ok(ok_data(dashboard::summary(&state, user_id).await?))
}

pub fn router(state: &AppState) -> Router<AppState> {
    use mz_core::gate::guard;
    Router::new()
        // ── /mobile/clock ──
        .route(
            "/mobile/clock/in",
            guard(
                state,
                CLOCK,
                post(h_clock_in).layer(DefaultBodyLimit::max(UPLOAD_LIMIT)),
            ),
        )
        .route(
            "/mobile/clock/out",
            guard(
                state,
                CLOCK,
                post(h_clock_out).layer(DefaultBodyLimit::max(UPLOAD_LIMIT)),
            ),
        )
        .route("/mobile/clock/current", guard(state, CLOCK, get(h_current)))
        .route(
            "/mobile/clock/auto-close",
            guard(state, CLOCK_AUTO_CLOSE, post(h_auto_close)),
        )
        .route(
            "/mobile/clock/gps-status",
            guard(state, CLOCK, post(h_gps_status)),
        )
        .route(
            "/mobile/clock/break/start",
            guard(state, CLOCK, post(h_break_start)),
        )
        .route(
            "/mobile/clock/break/end",
            guard(state, CLOCK, post(h_break_end)),
        )
        .route(
            "/mobile/clock/break/current",
            guard(state, CLOCK, get(h_break_current)),
        )
        .route(
            "/mobile/clock/break/policy",
            guard(state, CLOCK, get(h_break_policy)),
        )
        .route(
            "/mobile/clock/attendance",
            guard(state, CLOCK, get(h_my_attendance)),
        )
        .route(
            "/mobile/clock/shifts/{shift_id}/selfie",
            guard(state, CLOCK, get(h_my_selfie)),
        )
        // ── /v1/admin/clock ──
        .route(
            "/v1/admin/clock/attendance",
            guard(state, ADMIN_CLOCK, get(h_admin_attendance)),
        )
        .route(
            "/v1/admin/clock/attendance/selfie",
            guard(state, ADMIN_CLOCK, get(h_admin_attendance_selfie)),
        )
        .route(
            "/v1/admin/clock/attendance/export/excel",
            guard(state, ADMIN_CLOCK, get(h_export_excel)),
        )
        .route(
            "/v1/admin/clock/attendance/export/pdf",
            guard(state, ADMIN_CLOCK, get(h_export_pdf)),
        )
        .route(
            "/v1/admin/clock/attendance/{shift_id}",
            guard(state, ADMIN_CLOCK, get(h_admin_detail)),
        )
        .route(
            "/v1/admin/clock/shifts/{shift_id}/selfie",
            guard(state, ADMIN_CLOCK, get(h_admin_selfie)),
        )
        .route(
            "/v1/admin/clock/shifts/{shift_id}/selfie/url",
            guard(state, ADMIN_SELFIE_URL, get(h_admin_selfie_url)),
        )
        // ── /mobile/kyc ──
        .route("/mobile/kyc/face", guard(state, KYC, post(h_kyc_save)))
        .route("/mobile/kyc/face", guard(state, KYC, get(h_kyc_get)))
        // ── /mobile/company ──
        .route(
            "/mobile/company/config",
            guard(state, CONFIG, get(h_config)),
        )
        .route(
            "/v1/mobile/companies/{company_uuid}/logo",
            guard(state, LOGO, get(h_logo)),
        )
        // ── /mobile/dashboard ──
        .route(
            "/mobile/dashboard/summary",
            guard(state, DASHBOARD, get(h_dashboard)),
        )
}
