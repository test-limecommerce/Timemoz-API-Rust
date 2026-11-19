//! Port `mobile-store-visits.controller.ts` (12 route, TANPA prefix `v1`) +
//! `mobile-store-visit-photo-file.controller.ts` (proxy foto bertanda tangan) + cron
//! `store-visits.cron.ts`.
//!
//! Kelas: `@RequireFeature('store-visits')`, `@RequireModule(SALES_TRACKER)`,
//! `@Roles(SYSTEM_ADMIN, SUPER_ADMIN, ADMIN, MANAGER, STAFF)`. Limiter express dari `main.ts`.

pub mod attendance;
pub mod auto_close;
pub mod dto;
pub mod events;
pub mod service;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use mz_audit::AuditHandle;
use mz_core::enums::{FeatureModule, PermissionAction};
use mz_core::extract::{ValidatedJson, ValidatedQuery};
use mz_core::gate::guard;
use mz_core::permissions::Exempt;
use mz_core::rate_limit::{RateKey, RateRule};
use mz_core::uuid_rules::uuid_pipe;
use mz_core::{ApiResult, AppState, RequestCtx, RouteSpec};
use mz_sales_geo::dto::Raw;
use mz_table_fields::apply::censor_response_body;
use mz_table_fields::policy::policy_for_request;
use serde_json::{Value, json};

pub use auto_close::cron_jobs;

const MINUTE: u64 = 60_000;
const HOUR: u64 = 60 * MINUTE;

/// Limiter `main.ts` (kunci = token).
pub mod limits {
    use super::{HOUR, MINUTE, RateRule};
    pub const CHECK_IN: RateRule = RateRule::token("rl:store-visits-check-in", 10, MINUTE);
    pub const VALIDATE_LOCATION: RateRule =
        RateRule::token("rl:store-visits-validate-location", 30, MINUTE);
    pub const CHECK_OUT: RateRule = RateRule::token("rl:store-visits-check-out", 10, MINUTE);
    pub const CANCEL: RateRule = RateRule::token("rl:store-visits-cancel", 10, MINUTE);
    pub const PHOTO_UPLOAD_URL: RateRule =
        RateRule::token("rl:store-visits-photo-upload-url", 60, HOUR);
    pub const PHOTO_CONFIRM: RateRule = RateRule::token("rl:store-visits-photo-confirm", 60, HOUR);
    pub const ACTIVE: RateRule = RateRule::token("rl:store-visits-active", 20, MINUTE);
    pub const READ: RateRule = RateRule::token("rl:store-visits-read", 60, MINUTE);
    pub const EVENT: RateRule = RateRule::token("rl:store-visits-event", 30, MINUTE);
    pub const EVENT_READ: RateRule = RateRule::token("rl:store-visits-event-read", 60, MINUTE);
}

/// `main.ts` `mobileFileLimiter`: 300/menit per IP, `rl:mobile-signed-file` (bucket bersama dengan
/// proxy file form kunjungan).
const MOBILE_FILE_LIMIT: RateRule = RateRule {
    key_prefix: "rl:mobile-signed-file",
    max: Some(300),
    window_ms: 60_000,
    key: RateKey::Ip,
    pass_on_store_error: false,
};

const BASE: RouteSpec = RouteSpec::jwt()
    .roles(crate::MOBILE_ROLES)
    .feature("store-visits")
    .modules(&[FeatureModule::SALES_TRACKER]);

/// `MobileStoreVisitPhotoFileController` — `@PermissionExempt('no-user')`, tanpa JWT.
const SIGNED_FILE: RouteSpec = RouteSpec::public()
    .exempt(Exempt::NoUser)
    .rate_limits(&[MOBILE_FILE_LIMIT]);

/// 12 route mobile + 1 proxy foto.
pub(crate) fn routes(state: &AppState) -> Router<AppState> {
    use limits::*;
    const P: &str = "/mobile/store-visits";
    Router::new()
        .route(
            &format!("{P}/active"),
            guard(state, BASE.rate_limits(&[ACTIVE]), get(h_active)),
        )
        .route(
            &format!("{P}/summary"),
            guard(state, BASE.rate_limits(&[READ]), get(h_summary)),
        )
        .route(P, guard(state, BASE.rate_limits(&[READ]), get(h_find_all)))
        .route(
            P,
            guard(state, BASE.rate_limits(&[CHECK_IN]), post(h_check_in)),
        )
        .route(
            &format!("{P}/event"),
            guard(state, BASE.rate_limits(&[EVENT_READ]), get(h_event_history)),
        )
        .route(
            &format!("{P}/event"),
            guard(state, BASE.rate_limits(&[EVENT]), post(h_log_event)),
        )
        .route(
            &format!("{P}/validate-location"),
            guard(
                state,
                BASE.action(PermissionAction::READ)
                    .rate_limits(&[VALIDATE_LOCATION]),
                post(h_validate_location),
            ),
        )
        // `GET validate-location` jatuh ke `GET :id` di Nest → ParseUUIDPipe 400. Keduanya kena
        // limiter regex `main.ts:418-421` (`rl:store-visits-read`, selain active/summary/event).
        .route(
            &format!("{P}/validate-location"),
            guard(state, BASE.rate_limits(&[READ]), get(h_static_as_id)),
        )
        .route(
            &format!("{P}/{{id}}"),
            guard(state, BASE.rate_limits(&[READ]), get(h_find_one)),
        )
        .route(
            &format!("{P}/{{id}}/check-out"),
            guard(state, BASE.rate_limits(&[CHECK_OUT]), patch(h_check_out)),
        )
        .route(
            &format!("{P}/{{id}}/cancel"),
            guard(state, BASE.rate_limits(&[CANCEL]), patch(h_cancel)),
        )
        .route(
            &format!("{P}/{{id}}/photos/upload-url"),
            guard(
                state,
                BASE.rate_limits(&[PHOTO_UPLOAD_URL]),
                post(h_photo_upload_url),
            ),
        )
        .route(
            &format!("{P}/{{id}}/photos/{{photo_id}}/confirm"),
            guard(
                state,
                BASE.rate_limits(&[PHOTO_CONFIRM]),
                post(h_photo_confirm),
            ),
        )
        // Nama param sama dengan route form `mz-visit-forms` (`{visit_uuid}`) supaya bisa di-merge.
        .route(
            "/v1/mobile/store-visits/{visit_uuid}/photos/{photo_uuid}/file",
            guard(state, SIGNED_FILE, get(h_photo_file)),
        )
}

fn data(v: Value) -> Json<Value> {
    Json(json!({ "data": v }))
}

/// Node: src/store-visits/mobile-store-visits.controller.ts:162 `getActive`.
async fn h_active(State(state): State<AppState>, ctx: RequestCtx) -> ApiResult<Json<Value>> {
    let uid = ctx.require_user_id()?;
    Ok(data(service::get_active_visit(&state.db, uid).await?))
}

/// Node: src/store-visits/mobile-store-visits.controller.ts:224 `getSummary`.
async fn h_summary(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedQuery(q): ValidatedQuery<Raw>,
) -> ApiResult<Json<Value>> {
    let uid = ctx.require_user_id()?;
    let date = dto::parse_summary_query(&q.0)?;
    Ok(data(service::get_summary(&state.db, uid, &date).await?))
}

/// Node: src/store-visits/mobile-store-visits.controller.ts:282 `findAll` (tanpa bungkus `data`).
async fn h_find_all(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedQuery(q): ValidatedQuery<Raw>,
) -> ApiResult<Json<Value>> {
    let uid = ctx.require_user_id()?;
    let q = dto::parse_list_query(&q.0)?;
    Ok(Json(
        service::find_visits_for_user(&state.db, uid, &q).await?,
    ))
}

/// Node: src/store-visits/mobile-store-visits.controller.ts:325 `listEventHistory` (tanpa bungkus).
async fn h_event_history(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedQuery(q): ValidatedQuery<Raw>,
) -> ApiResult<Json<Value>> {
    let uid = ctx.require_user_id()?;
    let q = dto::parse_event_history_query(&q.0)?;
    Ok(Json(events::list_event_history(&state, uid, &q).await?))
}

/// Node: src/store-visits/mobile-store-visits.controller.ts:369 `logEvent` (201).
async fn h_log_event(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedJson(body): ValidatedJson<Raw>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let uid = ctx.require_user_id()?;
    let dto = dto::parse_event(&body.0)?;
    Ok((
        StatusCode::CREATED,
        data(events::log_event(&state, uid, &dto).await?),
    ))
}

/// Node: src/store-visits/mobile-store-visits.controller.ts:402 `findOne`
/// (`@TableFields('stores', { path: 'data', within: 'store' })`).
async fn h_find_one(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let uid = ctx.require_user_id()?;
    let id = uuid_pipe(id)?;
    let mut body = json!({ "data": service::find_visit_for_user(&state, uid, &id).await? });
    let censored = policy_for_request(&state, &ctx, false)
        .await?
        .for_table("stores");
    censor_response_body(&mut body, "stores", "data", &censored, Some("store"));
    Ok(Json(body))
}

/// `GET /mobile/store-visits/validate-location` → `GET :id` dengan id bukan UUID.
async fn h_static_as_id(ctx: RequestCtx) -> ApiResult<Json<Value>> {
    ctx.require_user_id()?;
    uuid_pipe("validate-location".to_owned())?;
    Ok(Json(Value::Null))
}

/// Node: src/store-visits/mobile-store-visits.controller.ts:528 `validateLocation` (200).
async fn h_validate_location(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedJson(body): ValidatedJson<Raw>,
) -> ApiResult<Json<Value>> {
    let uid = ctx.require_user_id()?;
    let dto = dto::parse_validate_location(&body.0)?;
    Ok(data(service::validate_location(&state, uid, &dto).await?))
}

/// Node: src/store-visits/mobile-store-visits.controller.ts:692 `checkIn` (201, replay 200).
async fn h_check_in(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedJson(body): ValidatedJson<Raw>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let uid = ctx.require_user_id()?;
    let dto = dto::parse_check_in(&body.0)?;
    let out = service::check_in(&state, uid, &dto).await?;
    let status = if out.replayed {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    Ok((status, data(out.data)))
}

/// Node: src/store-visits/mobile-store-visits.controller.ts:773 `createPhotoUploadUrl` (201).
async fn h_photo_upload_url(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(id): Path<String>,
    ValidatedJson(body): ValidatedJson<Raw>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let uid = ctx.require_user_id()?;
    let id = uuid_pipe(id)?;
    let dto = crate::photos::parse_create_upload_url(&body.0)?;
    Ok((
        StatusCode::CREATED,
        data(crate::photos::create_upload_url(&state, uid, &id, &dto).await?),
    ))
}

/// Node: src/store-visits/mobile-store-visits.controller.ts:873 `confirmPhoto` (200).
async fn h_photo_confirm(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path((id, photo_id)): Path<(String, String)>,
    ValidatedJson(body): ValidatedJson<Raw>,
) -> ApiResult<Json<Value>> {
    let uid = ctx.require_user_id()?;
    let id = uuid_pipe(id)?;
    let photo_id = uuid_pipe(photo_id)?;
    let dto = crate::photos::parse_confirm(&body.0)?;
    Ok(data(
        crate::photos::confirm_photo(&state, uid, &id, &photo_id, &dto).await?,
    ))
}

/// Node: src/store-visits/mobile-store-visits.controller.ts:998 `checkOut` (200).
async fn h_check_out(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    Path(id): Path<String>,
    ValidatedJson(body): ValidatedJson<Raw>,
) -> ApiResult<Json<Value>> {
    let uid = ctx.require_user_id()?;
    let id = uuid_pipe(id)?;
    let dto = dto::parse_check_out(&body.0)?;
    Ok(data(
        service::check_out(&state, &audit, uid, &id, &dto).await?,
    ))
}

/// Node: src/store-visits/mobile-store-visits.controller.ts:1052 `cancel` (200).
async fn h_cancel(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(id): Path<String>,
    ValidatedJson(body): ValidatedJson<Raw>,
) -> ApiResult<Json<Value>> {
    let uid = ctx.require_user_id()?;
    let id = uuid_pipe(id)?;
    let reason = dto::parse_cancel(&body.0)?;
    Ok(data(
        service::cancel(&state.db, uid, &id, reason.as_deref()).await?,
    ))
}

/// Node: src/store-visits/mobile-store-visit-photo-file.controller.ts:39 `photoFile`.
async fn h_photo_file(
    State(state): State<AppState>,
    Path((visit_uuid, photo_uuid)): Path<(String, String)>,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    let (visit_uuid, photo_uuid) = match (uuid_pipe(visit_uuid), uuid_pipe(photo_uuid)) {
        (Ok(v), Ok(p)) => (v, p),
        (Err(e), _) | (_, Err(e)) => return e.into_response(),
    };
    let q = mz_core::qs::parse(uri.query().unwrap_or_default());
    let exp = mz_core::qs::get(&q, "exp").and_then(|v| v.as_str());
    let sig = mz_core::qs::get(&q, "sig").and_then(|v| v.as_str());
    crate::photos::stream_signed_photo(&state, &visit_uuid, &photo_uuid, exp, sig, &headers).await
}
