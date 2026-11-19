//! Route template admin (`admin-store-visit-forms.controller.ts`): template, assignee, versi,
//! validate, pratinjau PDF, aset kop/kaki, katalog — 20 route.

pub mod catalog;
pub mod dto;
pub mod service;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post, put};
use axum::{Json, Router};
use mz_audit::AuditHandle;
use mz_core::enums::PermissionAction;
use mz_core::gate::guard;
use mz_core::uuid_rules::uuid_pipe;
use mz_core::{ApiResult, AppState, RequestCtx, RouteSpec};
use serde_json::{Value, json};

use super::{ADMIN, limits};
use dto::JsonBody;

const READ: RouteSpec = ADMIN.rate_limits(&[limits::READ]);
const WRITE: RouteSpec = ADMIN.rate_limits(&[limits::WRITE]);
/// `@RequirePermission(READ)` — dry-run validate.
const VALIDATE: RouteSpec = ADMIN
    .action(PermissionAction::READ)
    .rate_limits(&[limits::VALIDATE]);
/// `@RequirePermission(READ)` — render pratinjau.
const PDF_PREVIEW: RouteSpec = ADMIN
    .action(PermissionAction::READ)
    .rate_limits(&[limits::PDF_PREVIEW]);
const ASSET_UPLOAD: RouteSpec = ADMIN.rate_limits(&[limits::ASSET_UPLOAD]);
const ASSET_CONFIRM: RouteSpec = ADMIN.rate_limits(&[limits::ASSET_CONFIRM]);
/// Proxy baca aset: tanpa limiter.
const ASSET_READ: RouteSpec = ADMIN;
/// `@Header('Cache-Control', SIGNED_URL_RESPONSE_CACHE_CONTROL)`.
const ASSET_URL: RouteSpec =
    ADMIN.headers(&[("cache-control", mz_files::SIGNED_URL_RESPONSE_CACHE_CONTROL)]);

fn data(v: Value) -> Json<Value> {
    Json(json!({ "data": v }))
}

fn created(v: Value) -> Response {
    (StatusCode::CREATED, data(v)).into_response()
}

/// Node: src/store-visit-forms/admin-store-visit-forms.controller.ts:202 `createTemplate` (201).
async fn h_create_template(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    JsonBody(body): JsonBody,
) -> ApiResult<Response> {
    let user_id = ctx.require_user_id()?;
    let dto = dto::parse_create(&body)?;
    Ok(created(
        service::create_template(&state, &audit, user_id, &dto).await?,
    ))
}

/// Node: src/store-visit-forms/admin-store-visit-forms.controller.ts:261 `findTemplates`.
async fn h_find_templates(
    State(state): State<AppState>,
    ctx: RequestCtx,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    let q = dto::parse_list_query(uri.query().unwrap_or_default())?;
    Ok(Json(service::find_templates(&state, user_id, &q).await?))
}

/// Node: src/store-visit-forms/admin-store-visit-forms.controller.ts:285 `findTemplate`.
async fn h_find_template(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(uuid): Path<String>,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    let uuid = uuid_pipe(uuid)?;
    Ok(data(service::find_template(&state, user_id, &uuid).await?))
}

/// Node: src/store-visit-forms/admin-store-visit-forms.controller.ts:327 `updateTemplate`.
async fn h_update_template(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    Path(uuid): Path<String>,
    JsonBody(body): JsonBody,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    let uuid = uuid_pipe(uuid)?;
    let dto = dto::parse_update(&body)?;
    Ok(data(
        service::update_template(&state, &audit, user_id, &uuid, &dto).await?,
    ))
}

/// Node: src/store-visit-forms/admin-store-visit-forms.controller.ts:371 `removeTemplate`.
async fn h_remove_template(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    Path(uuid): Path<String>,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    let uuid = uuid_pipe(uuid)?;
    Ok(data(
        service::remove_template(&state, &audit, user_id, &uuid).await?,
    ))
}

/// Node: src/store-visit-forms/admin-store-visit-forms.controller.ts:391 `findAssignees`
/// (tanpa bungkus tambahan: `{ data, scope }`).
async fn h_find_assignees(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(uuid): Path<String>,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    let uuid = uuid_pipe(uuid)?;
    Ok(Json(service::find_assignees(&state, user_id, &uuid).await?))
}

/// Node: src/store-visit-forms/admin-store-visit-forms.controller.ts:424 `replaceAssignees`.
async fn h_replace_assignees(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    Path(uuid): Path<String>,
    JsonBody(body): JsonBody,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    let uuid = uuid_pipe(uuid)?;
    let user_uuids = dto::parse_replace_assignees(&body)?;
    Ok(Json(
        service::replace_assignees(&state, &audit, user_id, &uuid, &user_uuids).await?,
    ))
}

/// Node: src/store-visit-forms/admin-store-visit-forms.controller.ts:501 `findVersions`.
async fn h_find_versions(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(uuid): Path<String>,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    let uuid = uuid_pipe(uuid)?;
    Ok(Json(service::find_versions(&state, user_id, &uuid).await?))
}

/// Node: src/store-visit-forms/admin-store-visit-forms.controller.ts:532 `createVersion` (201).
async fn h_create_version(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(uuid): Path<String>,
) -> ApiResult<Response> {
    let user_id = ctx.require_user_id()?;
    let uuid = uuid_pipe(uuid)?;
    Ok(created(
        service::create_version(&state, user_id, &uuid).await?,
    ))
}

/// Node: src/store-visit-forms/admin-store-visit-forms.controller.ts:564 `findVersion`.
async fn h_find_version(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path((uuid, version_uuid)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    let uuid = uuid_pipe(uuid)?;
    let version_uuid = uuid_pipe(version_uuid)?;
    Ok(data(
        service::find_version_detail(&state, user_id, &uuid, &version_uuid).await?,
    ))
}

/// Node: src/store-visit-forms/admin-store-visit-forms.controller.ts:603 `updateVersionSchema`.
async fn h_update_version_schema(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    Path((uuid, version_uuid)): Path<(String, String)>,
    JsonBody(body): JsonBody,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    let uuid = uuid_pipe(uuid)?;
    let version_uuid = uuid_pipe(version_uuid)?;
    let dto = dto::parse_update_version_schema(&body)?;
    Ok(data(
        service::update_version_schema(&state, &audit, user_id, &uuid, &version_uuid, &dto).await?,
    ))
}

/// Node: src/store-visit-forms/admin-store-visit-forms.controller.ts:695 `publishVersion` (200).
async fn h_publish_version(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path((uuid, version_uuid)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    let uuid = uuid_pipe(uuid)?;
    let version_uuid = uuid_pipe(version_uuid)?;
    Ok(data(
        service::publish_version(&state, user_id, &uuid, &version_uuid).await?,
    ))
}

/// Node: src/store-visit-forms/admin-store-visit-forms.controller.ts:774 `validateSchema` (200).
async fn h_validate_schema(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(uuid): Path<String>,
    JsonBody(body): JsonBody,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    let uuid = uuid_pipe(uuid)?;
    let schema = dto::parse_schema_body(&body)?;
    Ok(data(
        service::validate_schema(&state, user_id, &uuid, &schema).await?,
    ))
}

/// Node: src/store-visit-forms/admin-store-visit-forms.controller.ts:852 `previewPdf` (200,
/// `inline`, `no-store`).
async fn h_preview_pdf(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(uuid): Path<String>,
    JsonBody(body): JsonBody,
) -> ApiResult<Response> {
    let user_id = ctx.require_user_id()?;
    let uuid = uuid_pipe(uuid)?;
    let schema = dto::parse_schema_body(&body)?;
    let (buffer, filename) = service::generate_preview_pdf(&state, user_id, &uuid, &schema).await?;
    let len = buffer.len();
    let mut res = (StatusCode::OK, buffer).into_response();
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/pdf"),
    );
    if let Ok(v) = HeaderValue::from_str(&format!("inline; filename=\"{filename}\"")) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(res)
}

/// Node: src/store-visit-forms/admin-store-visit-forms.controller.ts:922 `createTemplateAssetUpload` (201).
async fn h_create_asset(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(uuid): Path<String>,
    JsonBody(body): JsonBody,
) -> ApiResult<Response> {
    let user_id = ctx.require_user_id()?;
    let uuid = uuid_pipe(uuid)?;
    let dto = dto::parse_asset_upload(&body)?;
    Ok(created(
        service::create_template_asset_upload(
            &state,
            user_id,
            &uuid,
            &dto.content_type,
            dto.size_bytes,
        )
        .await?,
    ))
}

/// Node: src/store-visit-forms/admin-store-visit-forms.controller.ts:979 `confirmTemplateAsset` (200).
async fn h_confirm_asset(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path((uuid, asset_uuid)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    let uuid = uuid_pipe(uuid)?;
    let asset_uuid = uuid_pipe(asset_uuid)?;
    Ok(data(
        service::confirm_template_asset(&state, user_id, &uuid, &asset_uuid).await?,
    ))
}

/// Node: src/store-visit-forms/admin-store-visit-forms.controller.ts:1043 `templateAsset` (stream).
async fn h_asset(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path((uuid, asset_uuid)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let user_id = ctx.require_user_id()?;
    let uuid = uuid_pipe(uuid)?;
    let asset_uuid = uuid_pipe(asset_uuid)?;
    let template = service::template_for(&state, user_id, &uuid).await?;
    crate::files::stream_template_asset(&state, template.id, &asset_uuid, &headers).await
}

/// Node: src/store-visit-forms/admin-store-visit-forms.controller.ts:1080 `templateAssetUrl`.
async fn h_asset_url(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path((uuid, asset_uuid)): Path<(String, String)>,
) -> ApiResult<Json<mz_files::SignedReadUrl>> {
    let user_id = ctx.require_user_id()?;
    let uuid = uuid_pipe(uuid)?;
    let asset_uuid = uuid_pipe(asset_uuid)?;
    let template = service::template_for(&state, user_id, &uuid).await?;
    Ok(Json(
        crate::files::template_asset_url(&state, template.id, &asset_uuid).await?,
    ))
}

/// Node: src/store-visit-forms/admin-store-visit-forms.controller.ts:1110 `getCatalog`.
async fn h_catalog(ctx: RequestCtx) -> ApiResult<Json<Value>> {
    ctx.require_user_id()?;
    Ok(data(catalog::catalog()))
}

const BASE: &str = "/v1/admin/store-visit-forms";

pub fn router(state: &AppState) -> Router<AppState> {
    let p = |s: &str| format!("{BASE}{s}");
    Router::new()
        .route(&p("/catalog"), guard(state, READ, get(h_catalog)))
        .route(
            &p("/templates"),
            guard(state, WRITE, post(h_create_template)),
        )
        .route(&p("/templates"), guard(state, READ, get(h_find_templates)))
        .route(
            &p("/templates/{uuid}"),
            guard(state, READ, get(h_find_template)),
        )
        .route(
            &p("/templates/{uuid}"),
            guard(state, WRITE, patch(h_update_template)),
        )
        .route(
            &p("/templates/{uuid}"),
            guard(state, WRITE, delete(h_remove_template)),
        )
        .route(
            &p("/templates/{uuid}/assignees"),
            guard(state, READ, get(h_find_assignees)),
        )
        .route(
            &p("/templates/{uuid}/assignees"),
            guard(state, WRITE, put(h_replace_assignees)),
        )
        .route(
            &p("/templates/{uuid}/versions"),
            guard(state, READ, get(h_find_versions)),
        )
        .route(
            &p("/templates/{uuid}/versions"),
            guard(state, WRITE, post(h_create_version)),
        )
        .route(
            &p("/templates/{uuid}/versions/{version_uuid}"),
            guard(state, READ, get(h_find_version)),
        )
        .route(
            &p("/templates/{uuid}/versions/{version_uuid}"),
            guard(state, WRITE, patch(h_update_version_schema)),
        )
        .route(
            &p("/templates/{uuid}/versions/{version_uuid}/publish"),
            guard(state, WRITE, post(h_publish_version)),
        )
        .route(
            &p("/templates/{uuid}/validate"),
            guard(state, VALIDATE, post(h_validate_schema)),
        )
        .route(
            &p("/templates/{uuid}/pdf-preview"),
            guard(state, PDF_PREVIEW, post(h_preview_pdf)),
        )
        .route(
            &p("/templates/{uuid}/assets"),
            guard(state, ASSET_UPLOAD, post(h_create_asset)),
        )
        .route(
            &p("/templates/{uuid}/assets/{asset_uuid}/confirm"),
            guard(state, ASSET_CONFIRM, post(h_confirm_asset)),
        )
        .route(
            &p("/templates/{uuid}/assets/{asset_uuid}"),
            guard(state, ASSET_READ, get(h_asset)),
        )
        .route(
            &p("/templates/{uuid}/assets/{asset_uuid}/url"),
            guard(state, ASSET_URL, get(h_asset_url)),
        )
}
