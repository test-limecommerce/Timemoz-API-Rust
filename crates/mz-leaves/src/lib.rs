//! Port `src/leaves/**` (10 route dasbor `/leaves`) dan `src/mobile-leaves/**` (9 route
//! `/mobile/leaves`) di atas fondasi `mz-leave` (saldo, ledger, hari kerja, antrean, email).
//! Deviasi: `plan/deviations/mz-leaves.md`. Model cuti tidak ber-audit; tidak ada cache
//! in-process yang perlu diinvalidasi.

pub mod approve;
pub mod dto;
pub mod form;
pub mod insights;
pub mod mobile;
pub mod model;
pub mod notify;
pub mod report;
pub mod review;
pub mod util;

use axum::extract::{DefaultBodyLimit, Path, RawQuery, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use mz_core::enums::Role;
use mz_core::gate::{RouteSpec, guard};
use mz_core::{ApiError, ApiResult, AppState, RequestCtx};

use crate::dto::query_object;
use crate::form::{JsonObject, LeaveForm};

/// `@Roles(SYSTEM_ADMIN, SUPER_ADMIN, ADMIN, MANAGER)` + `@RequireFeature('leaves')`.
const REVIEWER: RouteSpec = RouteSpec::jwt()
    .roles(&[
        Role::SYSTEM_ADMIN,
        Role::SUPER_ADMIN,
        Role::ADMIN,
        Role::MANAGER,
    ])
    .feature("leaves");
/// Mobile: + `STAFF`.
const EMPLOYEE: RouteSpec = RouteSpec::jwt()
    .roles(&[
        Role::SYSTEM_ADMIN,
        Role::SUPER_ADMIN,
        Role::ADMIN,
        Role::MANAGER,
        Role::STAFF,
    ])
    .feature("leaves");
/// `@Header('Cache-Control', SIGNED_URL_RESPONSE_CACHE_CONTROL)`.
const SIGNED_HEADERS: &[(&str, &str)] =
    &[("Cache-Control", mz_files::SIGNED_URL_RESPONSE_CACHE_CONTROL)];

/// Multer tanpa `limits` (Node tidak membatasi ukuran berkas); dibatasi 50 MB di sini.
pub const UPLOAD_LIMIT: usize = 50 * 1024 * 1024;

/// Company aktif pelaku; tanpa company → 404 (kontrak lama endpoint cuti, bukan 403).
async fn company(state: &AppState, ctx: &RequestCtx) -> ApiResult<i32> {
    ctx.company_id(state)
        .await?
        .ok_or_else(|| ApiError::not_found("User is not associated with any company"))
}

fn ok<T: serde::Serialize>(v: T) -> Response {
    Json(v).into_response()
}

// ── /leaves ────────────────────────────────────────────────────────────────

/// Node: src/leaves/leaves.controller.ts:231
async fn h_list(
    State(state): State<AppState>,
    ctx: RequestCtx,
    RawQuery(q): RawQuery,
) -> ApiResult<Response> {
    let q = dto::LeavesQuery::parse(&query_object(q.as_deref()))?;
    let uid = ctx.require_user_id()?;
    let c = company(&state, &ctx).await?;
    Ok(ok(review::list_requests(&state, uid, c, &q).await?))
}

/// Node: src/leaves/leaves.controller.ts:277
async fn h_calendar(
    State(state): State<AppState>,
    ctx: RequestCtx,
    RawQuery(q): RawQuery,
) -> ApiResult<Response> {
    let q = dto::CalendarQuery::parse(&query_object(q.as_deref()))?;
    let uid = ctx.require_user_id()?;
    let c = company(&state, &ctx).await?;
    Ok(ok(review::get_calendar(&state, uid, c, &q).await?))
}

/// Node: src/leaves/leaves.controller.ts:301 — `Content-Type`, `Content-Disposition`,
/// `Content-Length` dari `res.set`, body `StreamableFile`.
async fn h_report(
    State(state): State<AppState>,
    ctx: RequestCtx,
    RawQuery(q): RawQuery,
) -> ApiResult<Response> {
    let q = dto::ReportQuery::parse(&query_object(q.as_deref()))?;
    let uid = ctx.require_user_id()?;
    let c = company(&state, &ctx).await?;
    let file = report::generate_report(&state, uid, c, &q).await?;
    let mut res = file.bytes.into_response();
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(file.content_type),
    );
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{}\"", file.filename)) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    Ok(res)
}

/// Node: src/leaves/leaves.controller.ts:448
async fn h_summary(
    State(state): State<AppState>,
    ctx: RequestCtx,
    RawQuery(q): RawQuery,
) -> ApiResult<Response> {
    let q = dto::AnalyticsQuery::parse(&query_object(q.as_deref()))?;
    let uid = ctx.require_user_id()?;
    let c = company(&state, &ctx).await?;
    Ok(ok(insights::analytics(&state, uid, c, &q).await?))
}

/// Node: src/leaves/leaves.controller.ts:695
async fn h_overview(
    State(state): State<AppState>,
    ctx: RequestCtx,
    RawQuery(q): RawQuery,
) -> ApiResult<Response> {
    let q = dto::OverviewQuery::parse(&query_object(q.as_deref()))?;
    let uid = ctx.require_user_id()?;
    let c = company(&state, &ctx).await?;
    Ok(ok(insights::overview(&state, uid, c, &q).await?))
}

/// Node: src/leaves/leaves.controller.ts:712
async fn h_document(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(uuid): Path<String>,
) -> ApiResult<Response> {
    let uid = ctx.require_user_id()?;
    let c = company(&state, &ctx).await?;
    Ok(ok(
        review::get_request_document(&state, uid, c, &uuid).await?
    ))
}

/// Node: src/leaves/leaves.controller.ts:728
async fn h_get(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(uuid): Path<String>,
) -> ApiResult<Response> {
    let uid = ctx.require_user_id()?;
    let c = company(&state, &ctx).await?;
    Ok(ok(review::get_request(&state, uid, c, &uuid).await?))
}

/// Node: src/leaves/leaves.controller.ts:773
async fn h_approve(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(uuid): Path<String>,
    JsonObject(body): JsonObject,
) -> ApiResult<Response> {
    let note = dto::optional_note(&body, "approvalNote")?;
    let uid = ctx.require_user_id()?;
    let c = company(&state, &ctx).await?;
    Ok(ok(
        approve::approve_request(&state, uid, c, &uuid, note).await?
    ))
}

/// Node: src/leaves/leaves.controller.ts:818
async fn h_reject(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(uuid): Path<String>,
    JsonObject(body): JsonObject,
) -> ApiResult<Response> {
    let reason = dto::reject_reason(&body)?;
    let uid = ctx.require_user_id()?;
    let c = company(&state, &ctx).await?;
    Ok(ok(
        review::reject_request(&state, uid, c, &uuid, reason).await?
    ))
}

/// Node: src/leaves/leaves.controller.ts:867
async fn h_revision(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(uuid): Path<String>,
    JsonObject(body): JsonObject,
) -> ApiResult<Response> {
    let note = dto::optional_note(&body, "revisionNote")?;
    let uid = ctx.require_user_id()?;
    let c = company(&state, &ctx).await?;
    Ok(ok(
        review::request_revision(&state, uid, c, &uuid, note).await?
    ))
}

// ── /mobile/leaves ─────────────────────────────────────────────────────────

/// Node: src/mobile-leaves/mobile-leaves.controller.ts:241
async fn m_balance(State(state): State<AppState>, ctx: RequestCtx) -> ApiResult<Response> {
    let uid = ctx.require_user_id()?;
    let c = company(&state, &ctx).await?;
    Ok(ok(mobile::my_balance(&state, uid, c).await?))
}

/// Node: src/mobile-leaves/mobile-leaves.controller.ts:317 (`@HttpCode(201)`).
async fn m_submit(
    State(state): State<AppState>,
    ctx: RequestCtx,
    form: LeaveForm,
) -> ApiResult<Response> {
    let d = dto::SubmitDto::parse(&form.fields)?;
    let uid = ctx.require_user_id()?;
    let c = company(&state, &ctx).await?;
    let created = mobile::submit(&state, uid, c, &d, form.document.as_ref()).await?;
    Ok((StatusCode::CREATED, Json(created)).into_response())
}

/// Node: src/mobile-leaves/mobile-leaves.controller.ts:348
async fn m_list(
    State(state): State<AppState>,
    ctx: RequestCtx,
    RawQuery(q): RawQuery,
) -> ApiResult<Response> {
    let q = dto::MyRequestsQuery::parse(&query_object(q.as_deref()))?;
    let uid = ctx.require_user_id()?;
    Ok(ok(mobile::list_mine(&state, uid, &q).await?))
}

/// Node: src/mobile-leaves/mobile-leaves.controller.ts:370
async fn m_approvers(State(state): State<AppState>, ctx: RequestCtx) -> ApiResult<Response> {
    ctx.require_user_id()?;
    let c = company(&state, &ctx).await?;
    Ok(ok(mobile::approvers(&state, c).await?))
}

/// Node: src/mobile-leaves/mobile-leaves.controller.ts:407
async fn m_day_count(
    State(state): State<AppState>,
    ctx: RequestCtx,
    RawQuery(q): RawQuery,
) -> ApiResult<Response> {
    let q = dto::DayCountQuery::parse(&query_object(q.as_deref()))?;
    let uid = ctx.require_user_id()?;
    let c = company(&state, &ctx).await?;
    Ok(ok(mobile::day_count_preview(&state, uid, c, &q).await?))
}

/// Node: src/mobile-leaves/mobile-leaves.controller.ts:423
async fn m_document(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(uuid): Path<String>,
) -> ApiResult<Response> {
    let uid = ctx.require_user_id()?;
    Ok(ok(mobile::my_document(&state, uid, &uuid).await?))
}

/// Node: src/mobile-leaves/mobile-leaves.controller.ts:442
async fn m_get(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(uuid): Path<String>,
) -> ApiResult<Response> {
    let uid = ctx.require_user_id()?;
    Ok(ok(mobile::get_mine(&state, uid, &uuid).await?))
}

/// Node: src/mobile-leaves/mobile-leaves.controller.ts:521
async fn m_revise(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(uuid): Path<String>,
    form: LeaveForm,
) -> ApiResult<Response> {
    let d = dto::ReviseDto::parse(&form.fields)?;
    let uid = ctx.require_user_id()?;
    let c = company(&state, &ctx).await?;
    Ok(ok(mobile::revise(
        &state,
        uid,
        c,
        &uuid,
        &d,
        form.document.as_ref(),
    )
    .await?))
}

/// Node: src/mobile-leaves/mobile-leaves.controller.ts:554
async fn m_cancel(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(uuid): Path<String>,
) -> ApiResult<Response> {
    let uid = ctx.require_user_id()?;
    Ok(ok(mobile::cancel(&state, uid, &uuid).await?))
}

/// Router `/leaves` + `/mobile/leaves`.
pub fn router(state: &AppState) -> Router<AppState> {
    let signed = REVIEWER.headers(SIGNED_HEADERS);
    let signed_mobile = EMPLOYEE.headers(SIGNED_HEADERS);
    Router::new()
        .route("/leaves", guard(state, REVIEWER, get(h_list)))
        .route("/leaves/calendar", guard(state, REVIEWER, get(h_calendar)))
        .route("/leaves/report", guard(state, REVIEWER, get(h_report)))
        .route(
            "/leaves/report/summary",
            guard(state, REVIEWER, get(h_summary)),
        )
        .route("/leaves/overview", guard(state, REVIEWER, get(h_overview)))
        .route(
            "/leaves/{uuid}/document",
            guard(state, signed, get(h_document)),
        )
        .route("/leaves/{uuid}", guard(state, REVIEWER, get(h_get)))
        .route(
            "/leaves/{uuid}/approve",
            guard(state, REVIEWER, put(h_approve)),
        )
        .route(
            "/leaves/{uuid}/reject",
            guard(state, REVIEWER, put(h_reject)),
        )
        .route(
            "/leaves/{uuid}/needs-revision",
            guard(state, REVIEWER, put(h_revision)),
        )
        .route(
            "/mobile/leaves/balance",
            guard(state, EMPLOYEE, get(m_balance)),
        )
        .route(
            "/mobile/leaves",
            guard(
                state,
                EMPLOYEE,
                post(m_submit).layer(DefaultBodyLimit::max(UPLOAD_LIMIT)),
            ),
        )
        .route("/mobile/leaves", guard(state, EMPLOYEE, get(m_list)))
        .route(
            "/mobile/leaves/approvers",
            guard(state, EMPLOYEE, get(m_approvers)),
        )
        .route(
            "/mobile/leaves/day-count",
            guard(state, EMPLOYEE, get(m_day_count)),
        )
        .route(
            "/mobile/leaves/{uuid}/document",
            guard(state, signed_mobile, get(m_document)),
        )
        .route("/mobile/leaves/{uuid}", guard(state, EMPLOYEE, get(m_get)))
        .route(
            "/mobile/leaves/{uuid}/revise",
            guard(
                state,
                EMPLOYEE,
                put(m_revise).layer(DefaultBodyLimit::max(UPLOAD_LIMIT)),
            ),
        )
        .route(
            "/mobile/leaves/{uuid}",
            guard(state, EMPLOYEE, delete(m_cancel)),
        )
}
