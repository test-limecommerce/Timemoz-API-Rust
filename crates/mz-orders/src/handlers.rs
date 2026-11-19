//! Controller order: `admin-orders` (12), `mobile-orders` (5), `admin-order-approvals` (3),
//! `admin-order-deliveries` (3), `mobile-order-deliveries` (4), `admin-store-visit-no-order` (1).

use std::collections::HashSet;

use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post, put};
use axum::{Json, Router};
use mz_audit::AuditHandle;
use mz_core::extract::{ValidatedJson, ValidatedQuery};
use mz_core::gate::guard;
use mz_core::uuid_rules::uuid_pipe;
use mz_core::{ApiError, ApiResult, AppState, RequestCtx};
use mz_products::dto::Raw;
use mz_table_fields::apply::apply_field_policy;
use mz_table_fields::policy::CensoredColumns;
use serde_json::{Value, json};

use crate::censor::{self, envelope_list, envelope_one};
use crate::service::BeforeCommit;
use crate::{config, deliveries, dto, export, invoice_pdf, mobile, service, transitions};

fn val(e: Vec<String>) -> ApiError {
    ApiError::Validation(e)
}

fn data(v: Value) -> Json<Value> {
    Json(json!({ "data": v }))
}

// ───────────────────────── admin-orders.controller.ts ─────────────────────────

/// Node: src/orders/admin-orders.controller.ts:108 `findAll`
async fn h_find_all(
    State(state): State<AppState>,
    ctx: RequestCtx,
    RawQuery(raw): RawQuery,
) -> ApiResult<Json<Value>> {
    let q = dto::parse_list(raw.as_deref().unwrap_or_default(), false).map_err(val)?;
    let p = censor::policy(&state, &ctx).await?;
    let body = service::find_all(&state, ctx.require_user_id()?, &q, &p.orders).await?;
    Ok(Json(envelope_list(body, &p, false)))
}

/// Node: src/orders/admin-orders.controller.ts:125 `create` (201)
async fn h_create(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    ValidatedJson(body): ValidatedJson<Raw>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let d = dto::parse_create(&body.0).map_err(val)?;
    let order = service::create(&state, &audit, ctx.require_user_id()?, &d).await?;
    let p = censor::policy(&state, &ctx).await?;
    Ok((StatusCode::CREATED, Json(envelope_one(order, &p))))
}

/// Node: src/orders/admin-orders.controller.ts:166 `export`
async fn h_export(
    State(state): State<AppState>,
    ctx: RequestCtx,
    RawQuery(raw): RawQuery,
) -> ApiResult<Response> {
    let q = dto::parse_list(raw.as_deref().unwrap_or_default(), true).map_err(val)?;
    let p = censor::policy(&state, &ctx).await?;
    export::export_orders(&state, ctx.require_user_id()?, &q, &p.orders, &p.items).await
}

/// Node: src/orders/admin-orders.controller.ts:217 `getConfig`
async fn h_config(State(state): State<AppState>, ctx: RequestCtx) -> ApiResult<Json<Value>> {
    Ok(data(
        config::get_config(&state, ctx.require_user_id()?, true).await?,
    ))
}

/// Node: src/orders/admin-orders.controller.ts:232 `findOwners`
async fn h_owners(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedQuery(q): ValidatedQuery<Raw>,
) -> ApiResult<Json<Value>> {
    let (search, page, limit) = dto::parse_owners(&q.0).map_err(val)?;
    Ok(Json(
        service::find_owners(
            &state,
            ctx.require_user_id()?,
            search.as_deref(),
            page,
            limit,
        )
        .await?,
    ))
}

/// Node: src/orders/admin-orders.controller.ts:252 `findOne`
async fn h_find_one(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let order = service::find_one(&state, ctx.require_user_id()?, &id).await?;
    let p = censor::policy(&state, &ctx).await?;
    Ok(Json(envelope_one(order, &p)))
}

/// Node: src/orders/admin-orders.controller.ts:265 `invoicePdf`
async fn h_invoice_pdf(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let (mut order, name, address, tz) =
        service::find_for_invoice(&state, ctx.require_user_id()?, &id).await?;
    let p = censor::policy(&state, &ctx).await?;
    apply_field_policy(&mut order, "orders", &p.orders, None);
    censor::censor_order_children(&mut order, &p);
    let number = order
        .get("orderNumber")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            crate::common::internal("Cannot read properties of null (reading 'replace')")
        })?
        .to_owned();
    let pdf = invoice_pdf::generate(&invoice_pdf::build_invoice_data(
        &order,
        &name,
        address.as_deref(),
        &tz,
    ));
    let mut resp = (StatusCode::OK, pdf).into_response();
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/pdf"),
    );
    if let Ok(v) = HeaderValue::from_str(&format!(
        "attachment; filename=\"{}\"",
        invoice_pdf::filename(&number)
    )) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    Ok(resp)
}

async fn update_inner(
    state: AppState,
    ctx: RequestCtx,
    audit: AuditHandle,
    id: String,
    body: Raw,
) -> ApiResult<Json<Value>> {
    let d = dto::parse_update(&body.0).map_err(val)?;
    let order = service::update(&state, &audit, ctx.require_user_id()?, &id, &d).await?;
    let p = censor::policy(&state, &ctx).await?;
    Ok(Json(envelope_one(order, &p)))
}

/// Node: src/orders/admin-orders.controller.ts:298 `update`
async fn h_update(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    Path(id): Path<String>,
    ValidatedJson(body): ValidatedJson<Raw>,
) -> ApiResult<Json<Value>> {
    update_inner(state, ctx, audit, id, body).await
}

/// `PATCH /v1/admin/orders/{export|config|owners}` — di Nest jatuh ke `PATCH :id`.
fn update_static(segment: &'static str) -> axum::routing::MethodRouter<AppState> {
    patch(
        move |State(state): State<AppState>,
              ctx: RequestCtx,
              audit: AuditHandle,
              ValidatedJson(body): ValidatedJson<Raw>| {
            update_inner(state, ctx, audit, segment.to_owned(), body)
        },
    )
}

/// Node: src/orders/admin-orders.controller.ts:320 `replaceItems`
async fn h_replace_items(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    Path(id): Path<String>,
    ValidatedJson(body): ValidatedJson<Raw>,
) -> ApiResult<Json<Value>> {
    let items = dto::parse_replace_items(&body.0).map_err(val)?;
    let p = censor::policy(&state, &ctx).await?;
    let censored: HashSet<&str> = p.items.keys().map(String::as_str).collect();
    let order = service::replace_items(
        &state,
        &audit,
        ctx.require_user_id()?,
        &id,
        &items,
        &censored,
        BeforeCommit::None,
    )
    .await?;
    Ok(Json(envelope_one(order, &p)))
}

/// Node: src/orders/admin-orders.controller.ts:350 `submit`
async fn h_submit(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let order = transitions::submit(&state, &audit, ctx.require_user_id()?, &id).await?;
    let p = censor::policy(&state, &ctx).await?;
    Ok(Json(envelope_one(order, &p)))
}

/// Node: src/orders/admin-orders.controller.ts:368 `cancel`
async fn h_cancel(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    Path(id): Path<String>,
    ValidatedJson(body): ValidatedJson<Raw>,
) -> ApiResult<Json<Value>> {
    let (reason, note) = dto::parse_cancel(&body.0).map_err(val)?;
    let order = transitions::cancel(
        &state,
        &audit,
        ctx.require_user_id()?,
        &id,
        reason.as_deref(),
        note.as_deref(),
    )
    .await?;
    let p = censor::policy(&state, &ctx).await?;
    Ok(Json(envelope_one(order, &p)))
}

/// Node: src/orders/admin-orders.controller.ts:387 `duplicate` (201)
async fn h_duplicate(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    Path(id): Path<String>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let order = transitions::duplicate(&state, &audit, ctx.require_user_id()?, &id).await?;
    let p = censor::policy(&state, &ctx).await?;
    Ok((StatusCode::CREATED, Json(envelope_one(order, &p))))
}

// ───────────────────────── admin-order-approvals.controller.ts ─────────────────────────

/// Node: src/orders/admin-order-approvals.controller.ts:43 `findPending` — status dipaksa
/// `pending_approval`, tanpa `@TableFields`.
async fn h_pending(
    State(state): State<AppState>,
    ctx: RequestCtx,
    RawQuery(raw): RawQuery,
) -> ApiResult<Json<Value>> {
    let mut q = dto::parse_list(raw.as_deref().unwrap_or_default(), false).map_err(val)?;
    q.filter.status = Some(vec!["pending_approval".into()]);
    Ok(Json(
        service::find_all(&state, ctx.require_user_id()?, &q, &CensoredColumns::new()).await?,
    ))
}

/// Node: src/orders/admin-order-approvals.controller.ts:56 `approve`
async fn h_approve(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    Ok(data(
        transitions::approve(&state, &audit, ctx.require_user_id()?, &id).await?,
    ))
}

/// Node: src/orders/admin-order-approvals.controller.ts:68 `reject`
async fn h_reject(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    Path(id): Path<String>,
    ValidatedJson(body): ValidatedJson<Raw>,
) -> ApiResult<Json<Value>> {
    let note = dto::parse_reject(&body.0).map_err(val)?;
    Ok(data(
        transitions::reject(&state, &audit, ctx.require_user_id()?, &id, &note).await?,
    ))
}

// ───────────────────────── admin-order-deliveries.controller.ts ─────────────────────────

/// Node: src/orders/admin-order-deliveries.controller.ts:80 `findQueue` — status dipaksa `approved`.
async fn h_delivery_queue(
    State(state): State<AppState>,
    ctx: RequestCtx,
    RawQuery(raw): RawQuery,
) -> ApiResult<Json<Value>> {
    let mut q = dto::parse_list(raw.as_deref().unwrap_or_default(), false).map_err(val)?;
    q.filter.status = Some(vec!["approved".into()]);
    let p = censor::policy(&state, &ctx).await?;
    let body = service::find_all(&state, ctx.require_user_id()?, &q, &p.orders).await?;
    Ok(Json(envelope_list(body, &p, false)))
}

/// Node: src/orders/admin-order-deliveries.controller.ts:100 `deliver`
async fn h_admin_deliver(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    Path(id): Path<String>,
    ValidatedJson(body): ValidatedJson<Raw>,
) -> ApiResult<Json<Value>> {
    let id = uuid_pipe(id)?;
    let d = dto::parse_deliver(&body.0).map_err(val)?;
    let order = deliveries::deliver(
        &state,
        &audit,
        ctx.require_user_id()?,
        &id,
        &d,
        "web",
        false,
    )
    .await?;
    let p = censor::policy(&state, &ctx).await?;
    Ok(Json(envelope_one(order, &p)))
}

/// Node: src/orders/admin-order-deliveries.controller.ts:126 `correct`
async fn h_correct(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    Path(id): Path<String>,
    ValidatedJson(body): ValidatedJson<Raw>,
) -> ApiResult<Json<Value>> {
    let id = uuid_pipe(id)?;
    let d = dto::parse_correct(&body.0).map_err(val)?;
    let order = deliveries::correct(&state, &audit, ctx.require_user_id()?, &id, &d).await?;
    let p = censor::policy(&state, &ctx).await?;
    Ok(Json(envelope_one(order, &p)))
}

// ───────────────────────── mobile-order-deliveries.controller.ts ─────────────────────────

/// Node: src/orders/mobile-order-deliveries.controller.ts:77 `getConfig`
async fn h_md_config(State(state): State<AppState>, ctx: RequestCtx) -> ApiResult<Json<Value>> {
    Ok(data(
        deliveries::mobile_config(&state, ctx.require_user_id()?).await?,
    ))
}

/// Node: src/orders/mobile-order-deliveries.controller.ts:90 `findAll`
async fn h_md_list(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedQuery(q): ValidatedQuery<Raw>,
) -> ApiResult<Json<Value>> {
    let (cursor, limit, status) = dto::parse_mobile_deliveries(&q.0).map_err(val)?;
    let body = deliveries::mobile_list(
        &state,
        ctx.require_user_id()?,
        cursor.as_deref(),
        limit,
        status.as_deref(),
    )
    .await?;
    let p = censor::policy(&state, &ctx).await?;
    Ok(Json(envelope_list(body, &p, true)))
}

/// Node: src/orders/mobile-order-deliveries.controller.ts:115 `findOne`
async fn h_md_one(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let id = uuid_pipe(id)?;
    let order = deliveries::mobile_find_one(&state, ctx.require_user_id()?, &id).await?;
    let p = censor::policy(&state, &ctx).await?;
    Ok(Json(envelope_one(order, &p)))
}

/// Node: src/orders/mobile-order-deliveries.controller.ts:128 `deliver`
async fn h_md_deliver(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    Path(id): Path<String>,
    ValidatedJson(body): ValidatedJson<Raw>,
) -> ApiResult<Json<Value>> {
    let id = uuid_pipe(id)?;
    let d = dto::parse_deliver(&body.0).map_err(val)?;
    let order = deliveries::deliver(
        &state,
        &audit,
        ctx.require_user_id()?,
        &id,
        &d,
        "mobile",
        true,
    )
    .await?;
    let p = censor::policy(&state, &ctx).await?;
    Ok(Json(envelope_one(order, &p)))
}

// ───────────────────────── mobile-orders.controller.ts ─────────────────────────

/// Node: src/orders/mobile-orders.controller.ts:71 `getConfig`
async fn h_m_config(State(state): State<AppState>, ctx: RequestCtx) -> ApiResult<Json<Value>> {
    Ok(data(
        config::get_config(&state, ctx.require_user_id()?, false).await?,
    ))
}

/// Node: src/orders/mobile-orders.controller.ts:80 `create` — 201 bila baris baru, 200 bila replay.
async fn h_m_create(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    ValidatedJson(body): ValidatedJson<Raw>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let (header, items) = dto::parse_create_mobile(&body.0).map_err(val)?;
    let (order, is_new) =
        mobile::create(&state, &audit, ctx.require_user_id()?, &header, &items).await?;
    Ok((
        if is_new {
            StatusCode::CREATED
        } else {
            StatusCode::OK
        },
        data(order),
    ))
}

/// Node: src/orders/mobile-orders.controller.ts:116 `findAll`
async fn h_m_list(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedQuery(q): ValidatedQuery<Raw>,
) -> ApiResult<Json<Value>> {
    let (cursor, limit) = dto::parse_mobile_list(&q.0).map_err(val)?;
    Ok(Json(
        mobile::find_all(&state, ctx.require_user_id()?, cursor.as_deref(), limit).await?,
    ))
}

/// Node: src/orders/mobile-orders.controller.ts:134 `findOne`
async fn h_m_one(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let id = uuid_pipe(id)?;
    Ok(data(
        mobile::find_one(&state, ctx.require_user_id()?, &id).await?,
    ))
}

/// Node: src/orders/mobile-orders.controller.ts:145 `createNoOrder` (201)
async fn h_m_no_order(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(id): Path<String>,
    ValidatedJson(body): ValidatedJson<Raw>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let id = uuid_pipe(id)?;
    let (reason, note) = dto::parse_no_order(&body.0).map_err(val)?;
    let v = mobile::create_no_order(
        &state,
        ctx.require_user_id()?,
        &id,
        &reason,
        note.as_deref(),
    )
    .await?;
    Ok((StatusCode::CREATED, data(v)))
}

// ───────────────────────── admin-store-visit-no-order.controller.ts ─────────────────────────

/// Node: src/orders/admin-store-visit-no-order.controller.ts:57 `findNoOrder`
async fn h_admin_no_order(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let id = uuid_pipe(id)?;
    Ok(data(
        mobile::find_no_order(&state, ctx.require_user_id()?, &id).await?,
    ))
}

/// 28 route order (di luar master & custom field).
pub fn routes(state: &AppState) -> Router<AppState> {
    use crate::{
        MOBILE_ORDER_DELIVERIES, MOBILE_ORDERS, ORDER_APPROVALS, ORDER_DELIVERIES, ORDERS,
    };
    const A: &str = "/v1/admin/orders";
    Router::new()
        .route(A, guard(state, ORDERS, get(h_find_all)))
        .route(A, guard(state, ORDERS, post(h_create)))
        .route(&format!("{A}/export"), guard(state, ORDERS, get(h_export)))
        .route(
            &format!("{A}/export"),
            guard(state, ORDERS, update_static("export")),
        )
        .route(&format!("{A}/config"), guard(state, ORDERS, get(h_config)))
        .route(
            &format!("{A}/config"),
            guard(state, ORDERS, update_static("config")),
        )
        .route(&format!("{A}/owners"), guard(state, ORDERS, get(h_owners)))
        .route(
            &format!("{A}/owners"),
            guard(state, ORDERS, update_static("owners")),
        )
        .route(
            &format!("{A}/{{id}}"),
            guard(state, ORDERS, get(h_find_one)),
        )
        .route(
            &format!("{A}/{{id}}"),
            guard(state, ORDERS, patch(h_update)),
        )
        .route(
            &format!("{A}/{{id}}/invoice-pdf"),
            guard(state, ORDERS, get(h_invoice_pdf)),
        )
        .route(
            &format!("{A}/{{id}}/items"),
            guard(state, ORDERS, put(h_replace_items)),
        )
        .route(
            &format!("{A}/{{id}}/submit"),
            guard(state, ORDERS, post(h_submit)),
        )
        .route(
            &format!("{A}/{{id}}/cancel"),
            guard(state, ORDERS, post(h_cancel)),
        )
        .route(
            &format!("{A}/{{id}}/duplicate"),
            guard(state, ORDERS, post(h_duplicate)),
        )
        .route(
            "/v1/admin/order-approvals",
            guard(state, ORDER_APPROVALS, get(h_pending)),
        )
        .route(
            "/v1/admin/order-approvals/{id}/approve",
            guard(state, ORDER_APPROVALS, post(h_approve)),
        )
        .route(
            "/v1/admin/order-approvals/{id}/reject",
            guard(state, ORDER_APPROVALS, post(h_reject)),
        )
        .route(
            "/v1/admin/order-deliveries",
            guard(state, ORDER_DELIVERIES, get(h_delivery_queue)),
        )
        .route(
            "/v1/admin/order-deliveries/{id}/deliver",
            guard(state, ORDER_DELIVERIES, post(h_admin_deliver)),
        )
        .route(
            "/v1/admin/order-deliveries/{id}/correct",
            guard(state, ORDER_DELIVERIES, post(h_correct)),
        )
        .route(
            "/v1/mobile/order-deliveries/config",
            guard(state, MOBILE_ORDER_DELIVERIES, get(h_md_config)),
        )
        .route(
            "/v1/mobile/order-deliveries",
            guard(state, MOBILE_ORDER_DELIVERIES, get(h_md_list)),
        )
        .route(
            "/v1/mobile/order-deliveries/{id}",
            guard(state, MOBILE_ORDER_DELIVERIES, get(h_md_one)),
        )
        .route(
            "/v1/mobile/order-deliveries/{id}/deliver",
            guard(state, MOBILE_ORDER_DELIVERIES, post(h_md_deliver)),
        )
        .route(
            "/v1/mobile/orders/config",
            guard(state, MOBILE_ORDERS, get(h_m_config)),
        )
        .route(
            "/v1/mobile/orders",
            guard(state, MOBILE_ORDERS, post(h_m_create)),
        )
        .route(
            "/v1/mobile/orders",
            guard(state, MOBILE_ORDERS, get(h_m_list)),
        )
        .route(
            "/v1/mobile/orders/{id}",
            guard(state, MOBILE_ORDERS, get(h_m_one)),
        )
        .route(
            "/v1/mobile/store-visits/{visit_uuid}/no-order",
            guard(state, MOBILE_ORDERS, post(h_m_no_order)),
        )
        .route(
            "/v1/admin/store-visits/{id}/no-order",
            guard(state, ORDERS, get(h_admin_no_order)),
        )
}
