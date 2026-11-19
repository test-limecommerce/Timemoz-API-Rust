//! Controller: `google-calendar.controller.ts` (10 route, `@RequireFeature('integrations')`),
//! `google-calendar-user.controller.ts` (7 route, `@RequireFeature('calendar')`),
//! `google-calendar-webhook.controller.ts` (1 route, `@PermissionExempt('no-user')`).
//! Semua class ber-`@SkipThrottle()` (tanpa limiter).

use axum::Json;
use axum::Router;
use axum::extract::{RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use mz_audit::AuditHandle;
use mz_core::enums::Role;
use mz_core::extract::{PathParams, ValidatedJson};
use mz_core::gate::guard;
use mz_core::permissions::Exempt;
use mz_core::{ApiResult, AppState, RequestCtx, RouteSpec};
use serde::Serialize;

use crate::dto::{CalendarMonth, CalendarQuery, EventBody};
use crate::util::{QVal, parse_query, query_get};
use crate::{company, user};

const MANAGERS: &[Role] = &[
    Role::SYSTEM_ADMIN,
    Role::SUPER_ADMIN,
    Role::ADMIN,
    Role::MANAGER,
];
const ADMINS: &[Role] = &[Role::SYSTEM_ADMIN, Role::SUPER_ADMIN, Role::ADMIN];
const EVERYONE: &[Role] = &[
    Role::SYSTEM_ADMIN,
    Role::SUPER_ADMIN,
    Role::ADMIN,
    Role::MANAGER,
    Role::STAFF,
    Role::GUEST,
];

const C_MANAGERS: RouteSpec = RouteSpec::jwt().roles(MANAGERS).feature("integrations");
const C_ADMINS: RouteSpec = RouteSpec::jwt().roles(ADMINS).feature("integrations");
const C_EVERYONE: RouteSpec = RouteSpec::jwt().roles(EVERYONE).feature("integrations");
/// `@Get('callback')` tanpa guard JWT (fitur tidak berlaku tanpa user).
const C_CALLBACK: RouteSpec = RouteSpec::public().feature("integrations");
const U_EVERYONE: RouteSpec = RouteSpec::jwt().roles(EVERYONE).feature("calendar");
const U_CALLBACK: RouteSpec = RouteSpec::public().feature("calendar");
const WEBHOOK: RouteSpec = RouteSpec::public().exempt(Exempt::NoUser);

pub fn router(state: &AppState) -> Router<AppState> {
    let s = state;
    Router::new()
        .route(
            "/google-calendar/auth-url",
            guard(s, C_MANAGERS, get(c_auth_url)),
        )
        .route(
            "/google-calendar/callback",
            guard(s, C_CALLBACK, get(c_callback)),
        )
        .route(
            "/google-calendar/disconnect",
            guard(s, C_ADMINS, delete(c_disconnect)),
        )
        .route(
            "/google-calendar/status",
            guard(s, C_MANAGERS, get(c_status)),
        )
        .route("/google-calendar/sync", guard(s, C_MANAGERS, post(c_sync)))
        .route(
            "/google-calendar/calendar",
            guard(s, C_EVERYONE, get(c_calendar)),
        )
        .route(
            "/google-calendar/events",
            guard(s, C_EVERYONE, get(c_events)),
        )
        .route(
            "/google-calendar/events",
            guard(s, C_MANAGERS, post(c_create_event)),
        )
        .route(
            "/google-calendar/calendars",
            guard(s, C_MANAGERS, get(c_calendars)),
        )
        .route(
            "/google-calendar/events/{uuid}",
            guard(s, C_MANAGERS, delete(c_delete_event)),
        )
        .route("/google-calendar/webhook", guard(s, WEBHOOK, post(webhook)))
        .route(
            "/google-calendar/user/auth-url",
            guard(s, U_EVERYONE, get(u_auth_url)),
        )
        .route(
            "/google-calendar/user/callback",
            guard(s, U_CALLBACK, get(u_callback)),
        )
        .route(
            "/google-calendar/user/disconnect",
            guard(s, U_EVERYONE, delete(u_disconnect)),
        )
        .route(
            "/google-calendar/user/status",
            guard(s, U_EVERYONE, get(u_status)),
        )
        .route(
            "/google-calendar/user/events",
            guard(s, U_EVERYONE, get(u_events)),
        )
        .route(
            "/google-calendar/user/events",
            guard(s, U_EVERYONE, post(u_create_event)),
        )
        .route(
            "/google-calendar/user/events/{uuid}",
            guard(s, U_EVERYONE, delete(u_delete_event)),
        )
}

#[derive(Serialize)]
struct UrlData {
    url: String,
}

#[derive(Serialize)]
struct AuthUrlResp {
    success: bool,
    data: UrlData,
}

fn auth_url_resp(url: String) -> Response {
    Json(AuthUrlResp {
        success: true,
        data: UrlData { url },
    })
    .into_response()
}

/// `@Query('code')` / `@Query('state')` (nilai berulang = array → tidak dipakai).
fn callback_params(raw: Option<&str>) -> (Option<String>, Option<String>) {
    let q = parse_query(raw);
    let one = |k: &str| query_get(&q, k).and_then(QVal::as_str).map(str::to_owned);
    (one("code"), one("state"))
}

/// `@Redirect()` + `{ url }` → `res.redirect(302, url)` Express 4.
fn redirect(headers: &HeaderMap, url: &str) -> Response {
    mz_core::redirect::redirect_302(headers, url)
}

fn ok<T: Serialize>(v: T) -> Response {
    Json(v).into_response()
}

// ── company ─────────────────────────────────────────────────────────────────

/// Node: src/integrations/google-calendar/google-calendar.controller.ts:35
async fn c_auth_url(State(state): State<AppState>, ctx: RequestCtx) -> ApiResult<Response> {
    let uid = ctx.require_user_id()?;
    Ok(auth_url_resp(company::generate_auth_url(&state, uid)))
}

/// Node: src/integrations/google-calendar/google-calendar.controller.ts:45
async fn c_callback(
    State(state): State<AppState>,
    audit: AuditHandle,
    headers: HeaderMap,
    RawQuery(q): RawQuery,
) -> Response {
    let (code, st) = callback_params(q.as_deref());
    let url = company::handle_callback(&state, &audit, code, st).await;
    redirect(&headers, &url)
}

/// Node: src/integrations/google-calendar/google-calendar.controller.ts:53
async fn c_disconnect(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
) -> ApiResult<Response> {
    let uid = ctx.require_user_id()?;
    Ok(ok(company::disconnect(&state, &audit, uid).await))
}

/// Node: src/integrations/google-calendar/google-calendar.controller.ts:62
async fn c_status(State(state): State<AppState>, ctx: RequestCtx) -> ApiResult<Response> {
    let uid = ctx.require_user_id()?;
    Ok(ok(company::get_status(&state, uid).await))
}

/// Node: src/integrations/google-calendar/google-calendar.controller.ts:71
async fn c_sync(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
) -> ApiResult<Response> {
    let uid = ctx.require_user_id()?;
    Ok(ok(company::trigger_sync(&state, &audit, uid).await))
}

/// Node: src/integrations/google-calendar/google-calendar.controller.ts:80
async fn c_calendar(
    State(state): State<AppState>,
    ctx: RequestCtx,
    RawQuery(q): RawQuery,
) -> ApiResult<Response> {
    let uid = ctx.require_user_id()?;
    let dto = CalendarMonth::from_query(q.as_deref())?;
    Ok(ok(company::get_calendar(&state, uid, dto).await))
}

/// Node: src/integrations/google-calendar/google-calendar.controller.ts:94
async fn c_events(
    State(state): State<AppState>,
    ctx: RequestCtx,
    RawQuery(q): RawQuery,
) -> ApiResult<Response> {
    let uid = ctx.require_user_id()?;
    let dto = CalendarQuery::from_query(q.as_deref())?;
    Ok(ok(company::get_events(&state, uid, dto).await))
}

/// Node: src/integrations/google-calendar/google-calendar.controller.ts:110
async fn c_calendars(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
) -> ApiResult<Response> {
    let uid = ctx.require_user_id()?;
    Ok(ok(company::list_calendars(&state, &audit, uid).await))
}

/// Node: src/integrations/google-calendar/google-calendar.controller.ts:119
async fn c_create_event(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    ValidatedJson(body): ValidatedJson<EventBody>,
) -> ApiResult<Response> {
    let uid = ctx.require_user_id()?;
    let dto = body.into_dto(true)?;
    let env = company::create_company_event(&state, &audit, uid, dto).await;
    Ok((StatusCode::CREATED, Json(env)).into_response())
}

/// Node: src/integrations/google-calendar/google-calendar.controller.ts:132
async fn c_delete_event(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    PathParams(uuid): PathParams<String>,
) -> ApiResult<Response> {
    let uid = ctx.require_user_id()?;
    Ok(ok(company::delete_company_event(
        &state, &audit, uid, &uuid,
    )
    .await))
}

#[derive(Serialize)]
struct WebhookAck {
    ok: bool,
}

/// Node: src/integrations/google-calendar/google-calendar-webhook.controller.ts:17
async fn webhook(
    State(state): State<AppState>,
    audit: AuditHandle,
    headers: HeaderMap,
) -> Response {
    let h = |k: &str| headers.get(k).and_then(|v| v.to_str().ok());
    if let Err(e) = company::handle_webhook_notification(
        &state,
        &audit,
        h("x-goog-channel-id"),
        h("x-goog-resource-state"),
    )
    .await
    {
        tracing::error!(error = %e.message(), "Google Calendar webhook error");
    }
    ok(WebhookAck { ok: true })
}

// ── user ────────────────────────────────────────────────────────────────────

/// Node: src/integrations/google-calendar/google-calendar-user.controller.ts:34
async fn u_auth_url(State(state): State<AppState>, ctx: RequestCtx) -> ApiResult<Response> {
    let uid = ctx.require_user_id()?;
    Ok(auth_url_resp(user::generate_auth_url(&state, uid)))
}

/// Node: src/integrations/google-calendar/google-calendar-user.controller.ts:44
async fn u_callback(
    State(state): State<AppState>,
    audit: AuditHandle,
    headers: HeaderMap,
    RawQuery(q): RawQuery,
) -> Response {
    let (code, st) = callback_params(q.as_deref());
    let url = user::handle_callback(&state, &audit, code, st).await;
    redirect(&headers, &url)
}

/// Node: src/integrations/google-calendar/google-calendar-user.controller.ts:52
async fn u_disconnect(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
) -> ApiResult<Response> {
    let uid = ctx.require_user_id()?;
    Ok(ok(user::disconnect(&state, &audit, uid).await))
}

/// Node: src/integrations/google-calendar/google-calendar-user.controller.ts:61
async fn u_status(State(state): State<AppState>, ctx: RequestCtx) -> ApiResult<Response> {
    let uid = ctx.require_user_id()?;
    Ok(ok(user::get_status(&state, uid).await))
}

/// Node: src/integrations/google-calendar/google-calendar-user.controller.ts:70
async fn u_events(
    State(state): State<AppState>,
    ctx: RequestCtx,
    RawQuery(q): RawQuery,
) -> ApiResult<Response> {
    let uid = ctx.require_user_id()?;
    let dto = CalendarQuery::from_query(q.as_deref())?;
    Ok(ok(user::get_events(&state, uid, dto).await))
}

/// Node: src/integrations/google-calendar/google-calendar-user.controller.ts:83
async fn u_create_event(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    ValidatedJson(body): ValidatedJson<EventBody>,
) -> ApiResult<Response> {
    let uid = ctx.require_user_id()?;
    let dto = body.into_dto(false)?;
    let env = user::create_user_event(&state, &audit, uid, dto).await;
    Ok((StatusCode::CREATED, Json(env)).into_response())
}

/// Node: src/integrations/google-calendar/google-calendar-user.controller.ts:96
async fn u_delete_event(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    PathParams(uuid): PathParams<String>,
) -> ApiResult<Response> {
    let uid = ctx.require_user_id()?;
    Ok(ok(user::delete_user_event(&state, &audit, uid, &uuid).await))
}
