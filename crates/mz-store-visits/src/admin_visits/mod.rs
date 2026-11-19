//! Port `src/store-visits/admin-store-visits.controller.ts` (`/v1/admin/store-visits`, 5 route)
//! beserta service dan export generator. `@RequireFeature('store-visits')`, `@RequireModule(SALES_TRACKER)`,
//! `@Roles(SYSTEM_ADMIN, SUPER_ADMIN, ADMIN, MANAGER)`. List/detail ber-`@TableFields('stores',
//! { path: 'data', within: 'store' })`; export menyensor sendiri (bukan JSON).

pub mod dto;
pub mod export;
pub mod range;
pub mod service;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use mz_core::enums::FeatureModule;
use mz_core::extract::ValidatedQuery;
use mz_core::gate::guard;
use mz_core::rate_limit::RateRule;
use mz_core::uuid_rules::uuid_pipe;
use mz_core::{ApiError, ApiResult, AppState, RequestCtx, RouteSpec};
use mz_sales_geo::dto::Raw;
use mz_table_fields::apply::censor_response_body;
use mz_table_fields::policy::{CensoredColumns, policy_for_request};
use serde_json::{Value, json};

use crate::enums::ExportFormat;
use crate::export::{
    CSV_EXPORT_MAX_ROWS, XLSX_EXPORT_MAX_ROWS, consume_export_quota, content_type_for,
};
use dto::parse_visit_list_query;

const TABLE_KEY: &str = "stores";

/// `main.ts` `adminStoreVisitsReadLimiter`: 60/menit per token, GET daftar + `/:id` (bukan export,
/// bukan route foto).
const READ_LIMIT: RateRule = RateRule::token("rl:admin-store-visits-read", 60, 60_000);

const BASE: RouteSpec = RouteSpec::jwt()
    .roles(crate::ADMIN_ROLES)
    .feature("store-visits")
    .modules(&[FeatureModule::SALES_TRACKER]);
const READ: RouteSpec = BASE.rate_limits(&[READ_LIMIT]);
/// `@Header('Cache-Control', SIGNED_URL_RESPONSE_CACHE_CONTROL)`.
const PHOTO_URL: RouteSpec =
    BASE.headers(&[("Cache-Control", mz_files::SIGNED_URL_RESPONSE_CACHE_CONTROL)]);

pub(crate) fn routes(state: &AppState) -> Router<AppState> {
    const P: &str = "/v1/admin/store-visits";
    Router::new()
        .route(P, guard(state, READ, get(h_find_all)))
        .route(
            &format!("{P}/export"),
            guard(state, BASE, get(h_export)).layer(axum::middleware::from_fn_with_state(
                state.clone(),
                crate::export::export_limiter,
            )),
        )
        .route(&format!("{P}/{{id}}"), guard(state, READ, get(h_find_one)))
        .route(
            &format!("{P}/{{id}}/photos/{{photoUuid}}/file"),
            guard(state, BASE, get(h_photo_file)),
        )
        .route(
            &format!("{P}/{{id}}/photos/{{photoUuid}}/file/url"),
            guard(state, PHOTO_URL, get(h_photo_file_url)),
        )
}

async fn censored_for(
    state: &AppState,
    ctx: &RequestCtx,
) -> ApiResult<std::sync::Arc<CensoredColumns>> {
    Ok(policy_for_request(state, ctx, false)
        .await?
        .for_table(TABLE_KEY))
}

/// Node: src/store-visits/admin-store-visits.controller.ts:189 `findAll`
async fn h_find_all(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedQuery(q): ValidatedQuery<Raw>,
) -> ApiResult<Json<Value>> {
    let q = parse_visit_list_query(&q.0, false).map_err(ApiError::Validation)?;
    let mut body = service::find_all(&state, ctx.require_user_id()?, &q).await?;
    let censored = censored_for(&state, &ctx).await?;
    censor_response_body(&mut body, TABLE_KEY, "data", &censored, Some("store"));
    Ok(Json(body))
}

/// Node: src/store-visits/admin-store-visits.controller.ts:291 `export`
async fn h_export(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedQuery(q): ValidatedQuery<Raw>,
) -> ApiResult<Response> {
    let q = parse_visit_list_query(&q.0, true).map_err(ApiError::Validation)?;
    let censored = censored_for(&state, &ctx).await?;
    let user_id = ctx.require_user_id()?;
    // `exportVisits(userId, query, censored)`.
    let r = service::resolve_where(&state, user_id, &q).await?;
    let total = service::count_visits(&state.db, &r.filter).await?;
    if q.format == ExportFormat::xlsx && total > XLSX_EXPORT_MAX_ROWS {
        return Err(ApiError::coded(
            axum::http::StatusCode::BAD_REQUEST,
            "EXPORT_TOO_MANY_ROWS",
            format!(
                "xlsx export is limited to {XLSX_EXPORT_MAX_ROWS} rows (matched {total}); narrow the filters or use format=csv"
            ),
        ));
    }
    if total > CSV_EXPORT_MAX_ROWS {
        return Err(ApiError::coded(
            axum::http::StatusCode::BAD_REQUEST,
            "EXPORT_TOO_MANY_ROWS",
            format!(
                "Export is limited to {CSV_EXPORT_MAX_ROWS} rows (matched {total}); narrow the filters"
            ),
        ));
    }
    consume_export_quota(&state.db, r.company_id).await?;
    match q.format {
        ExportFormat::xlsx => {
            let buffer =
                export::build_visits_xlsx(&state, &r.filter, &r.time_zone, &censored).await?;
            Ok(crate::export::file_response(
                content_type_for(ExportFormat::xlsx),
                "store-visits-export.xlsx",
                buffer,
            ))
        }
        ExportFormat::csv => {
            let body =
                export::csv_stream(state.clone(), r.filter, r.time_zone, (*censored).clone());
            let mut resp = body.into_response();
            let h = resp.headers_mut();
            h.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static(content_type_for(ExportFormat::csv)),
            );
            h.insert(
                header::CONTENT_DISPOSITION,
                HeaderValue::from_static("attachment; filename=\"store-visits-export.csv\""),
            );
            Ok(resp)
        }
    }
}

/// Node: src/store-visits/admin-store-visits.controller.ts:359 `findOne`
async fn h_find_one(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let id = uuid_pipe(id)?;
    let data = service::find_one(&state, ctx.require_user_id()?, &id).await?;
    let mut body = json!({ "data": data });
    let censored = censored_for(&state, &ctx).await?;
    censor_response_body(&mut body, TABLE_KEY, "data", &censored, Some("store"));
    Ok(Json(body))
}

/// Node: src/store-visits/admin-store-visits.controller.ts:398 `photoFile`
async fn h_photo_file(
    State(state): State<AppState>,
    ctx: RequestCtx,
    headers: HeaderMap,
    Path((visit_uuid, photo_uuid)): Path<(String, String)>,
) -> ApiResult<Response> {
    let visit_uuid = uuid_pipe(visit_uuid)?;
    let photo_uuid = uuid_pipe(photo_uuid)?;
    service::stream_photo_file(
        &state,
        ctx.require_user_id()?,
        &visit_uuid,
        &photo_uuid,
        &headers,
    )
    .await
}

/// Node: src/store-visits/admin-store-visits.controller.ts:434 `photoFileUrl`
async fn h_photo_file_url(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path((visit_uuid, photo_uuid)): Path<(String, String)>,
) -> ApiResult<Json<mz_files::SignedReadUrl>> {
    let visit_uuid = uuid_pipe(visit_uuid)?;
    let photo_uuid = uuid_pipe(photo_uuid)?;
    Ok(Json(
        service::photo_file_url(&state, ctx.require_user_id()?, &visit_uuid, &photo_uuid).await?,
    ))
}
