//! Port `src/store-visits/mobile-offline-sync.controller.ts` — `GET /v1/mobile/offline/sync`
//! (`@RequireFeature('store-visit-plans')`, `@RequireModule(SALES_TRACKER)`, role mobile,
//! `@TableFields('stores', { path: 'stores' })`, limiter `rl:mobile-offline-sync` 6/jam).
//! `ETag: "<syncToken>"` + `If-None-Match` → 304 tanpa body.

pub mod service;

use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use mz_core::enums::FeatureModule;
use mz_core::extract::ValidatedQuery;
use mz_core::gate::guard;
use mz_core::rate_limit::RateRule;
use mz_core::{ApiError, ApiResult, AppState, RequestCtx, RouteSpec};
use mz_sales_geo::dto::Raw;
use mz_table_fields::apply::censor_response_body;
use mz_table_fields::policy::policy_for_request;

/// `main.ts` `offlineSyncLimiter`: 6 per jam per token.
const SYNC_LIMIT: RateRule = RateRule::token("rl:mobile-offline-sync", 6, 60 * 60 * 1000);

const SYNC: RouteSpec = RouteSpec::jwt()
    .roles(crate::MOBILE_ROLES)
    .feature("store-visit-plans")
    .modules(&[FeatureModule::SALES_TRACKER])
    .rate_limits(&[SYNC_LIMIT]);

pub(crate) fn routes(state: &AppState) -> Router<AppState> {
    Router::new().route("/v1/mobile/offline/sync", guard(state, SYNC, get(h_sync)))
}

/// Node: src/store-visits/mobile-offline-sync.controller.ts:64 `sync`
async fn h_sync(
    State(state): State<AppState>,
    ctx: RequestCtx,
    headers: HeaderMap,
    ValidatedQuery(q): ValidatedQuery<Raw>,
) -> ApiResult<Response> {
    let q = service::parse_offline_sync_query(&q.0).map_err(ApiError::Validation)?;
    let censored = policy_for_request(&state, &ctx, false)
        .await?
        .for_table("stores");
    let mut bundle = service::sync(
        &state,
        ctx.require_user_id()?,
        &q,
        &censored,
        ctx.access().map(AsRef::as_ref),
    )
    .await?;
    let token = bundle
        .get("syncToken")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_owned();
    let etag = HeaderValue::from_str(&format!("\"{token}\""))
        .map_err(|e| ApiError::internal(anyhow::Error::from(e)))?;
    let inm = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok());
    let mut resp = if service::matches_etag(inm, &token) {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        // Interceptor `@TableFields`: sensor ulang `stores` (idempoten) + tempel `fieldPolicy`.
        censor_response_body(&mut bundle, "stores", "stores", &censored, None);
        Json(bundle).into_response()
    };
    resp.headers_mut().insert(header::ETAG, etag);
    Ok(resp)
}
