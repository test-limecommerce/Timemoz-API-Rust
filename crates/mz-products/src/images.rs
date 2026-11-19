//! Port `products/product-images.service.ts` + 3 route gambar di `admin-products.controller.ts`
//! + `product-image-public.controller.ts` (proxy publik tanpa login).

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::{delete, get, post};
use axum::{Json, Router, http::StatusCode};
use mz_audit::{AuditHandle, Lookup, Model};
use mz_core::company::require_company_id;
use mz_core::extract::ValidatedJson;
use mz_core::gate::guard;
use mz_core::permissions::Exempt;
use mz_core::rate_limit::{RateKey, RateRule};
use mz_core::time::now_naive;
use mz_core::uuid_rules::parse_uuid_pipe;
use mz_core::{ApiError, ApiResult, AppState, RequestCtx, RouteSpec};
use serde_json::{Value, json};

use crate::common::{coded_details, ms_opt, not_found, row_uuid, unprocessable};
use crate::dto::{Raw, check_int_min, check_max_string, check_string, re};
use crate::products::{ProductRow, find_live_by_uuid};
use crate::util::{
    MAX_PRODUCT_IMAGE_BYTES, extension_for_content_type, product_image_file_id, product_image_path,
    product_image_url,
};

/// `UPLOAD_URL_TTL_SECONDS`.
pub const UPLOAD_URL_TTL_SECONDS: i64 = 900;
/// `PRODUCT_IMAGE_CACHE_CONTROL`.
pub const PRODUCT_IMAGE_CACHE_CONTROL: &str = "public, max-age=31536000, immutable";

/// `main.ts` — `rl:product-image` 600/menit per IP.
const IMAGE_LIMIT: RateRule = RateRule {
    key_prefix: "rl:product-image",
    max: Some(600),
    window_ms: 60_000,
    key: RateKey::Ip,
    pass_on_store_error: false,
};
/// `ProductImagePublicController` — tanpa JWT, `@PermissionExempt('no-user')`.
const PUBLIC_IMAGE: RouteSpec = RouteSpec::public()
    .exempt(Exempt::NoUser)
    .rate_limits(&[IMAGE_LIMIT]);

async fn find_row_or_throw(state: &AppState, company_id: i32, uuid: &str) -> ApiResult<ProductRow> {
    find_live_by_uuid(&state.db, company_id, row_uuid(uuid)?)
        .await?
        .ok_or_else(|| not_found("PRODUCT_NOT_FOUND", "Product not found"))
}

fn too_large() -> ApiError {
    coded_details(
        StatusCode::UNPROCESSABLE_ENTITY,
        "PRODUCT_IMAGE_TOO_LARGE",
        format!("Image exceeds the {MAX_PRODUCT_IMAGE_BYTES} byte limit"),
        json!({ "maxSizeBytes": MAX_PRODUCT_IMAGE_BYTES }),
    )
}

fn to_response(state: &AppState, row: &ProductRow) -> Value {
    json!({
        "id": row.uuid,
        "imageUrl": product_image_url(state, &row.uuid, row.image_object_key.as_deref()),
        "imageUpdatedAt": ms_opt(row.image_updated_at),
    })
}

/// `deleteObjectQuietly`.
async fn delete_quietly(state: &AppState, key: &str) {
    if let Err(e) = mz_files::delete_file(state, key).await {
        tracing::warn!("failed to delete bucket object {key}: {e}");
    }
}

/// Tulis kolom gambar produk (`update` ber-audit), kembalikan baris baru.
pub async fn set_image_columns(
    state: &AppState,
    audit: &AuditHandle,
    product_id: i32,
    image_url: Option<&str>,
    object_key: Option<&str>,
    set_time: bool,
) -> ApiResult<ProductRow> {
    let pre = audit.pre_read(Model::Product, Lookup::Id(product_id)).await;
    let now = now_naive();
    sqlx::query(
        r#"UPDATE product SET "imageUrl" = $2, "imageObjectKey" = $3, "imageUpdatedAt" = $4,
             "updatedAt" = $5 WHERE id = $1"#,
    )
    .bind(product_id)
    .bind(image_url)
    .bind(object_key)
    .bind(set_time.then_some(now))
    .bind(now)
    .execute(&state.db)
    .await?;
    if let Some(r) = audit
        .result_row(&state.db, Model::Product, Lookup::Id(product_id))
        .await
    {
        audit.record_update(Model::Product, pre, &r).await;
    }
    Ok(sqlx::query_as(&format!(
        "SELECT {} FROM product WHERE id = $1",
        crate::products::PRODUCT_COLS
    ))
    .bind(product_id)
    .fetch_one(&state.db)
    .await?)
}

/// Node: src/products/product-images.service.ts:142 `createUploadUrl`
pub async fn create_upload_url(
    state: &AppState,
    user_id: i32,
    product_uuid: &str,
    content_type: &str,
    size_bytes: i64,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let product = find_row_or_throw(state, company_id, product_uuid).await?;
    let ext = extension_for_content_type(content_type).ok_or_else(|| {
        unprocessable(
            "PRODUCT_IMAGE_TYPE_REJECTED",
            "Only image/jpeg, image/png, and image/webp are accepted",
        )
    })?;
    if u64::try_from(size_bytes).unwrap_or(0) > MAX_PRODUCT_IMAGE_BYTES {
        return Err(too_large());
    }
    let key = format!(
        "products/{company_id}/{}/{}.{ext}",
        product.uuid,
        uuid::Uuid::new_v4()
    );
    let signed =
        mz_files::get_signed_file_url(state, &key, UPLOAD_URL_TTL_SECONDS, content_type).await?;
    Ok(json!({
        "objectKey": key,
        "uploadUrl": signed.url,
        "expiresInSeconds": UPLOAD_URL_TTL_SECONDS,
        "maxSizeBytes": MAX_PRODUCT_IMAGE_BYTES,
    }))
}

/// `assertObjectKeyOwnedByProduct`.
fn assert_key_owned(company_id: i32, product_uuid: &str, key: &str) -> ApiResult<()> {
    static SUFFIX: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| re(r"^[0-9a-f-]{36}\.(jpg|png|webp)$"));
    let prefix = format!("products/{company_id}/{product_uuid}/");
    if key
        .strip_prefix(&prefix)
        .is_none_or(|s| !SUFFIX.is_match(s))
    {
        return Err(unprocessable(
            "PRODUCT_IMAGE_OBJECT_KEY_INVALID",
            "objectKey does not belong to this product",
        ));
    }
    Ok(())
}

/// Node: src/products/product-images.service.ts:166 `confirm`
pub async fn confirm(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    product_uuid: &str,
    object_key: &str,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let product = find_row_or_throw(state, company_id, product_uuid).await?;
    assert_key_owned(company_id, &product.uuid, object_key)?;
    let meta = mz_files::get_object_metadata(state, object_key).await?;
    if !meta.exists {
        return Err(unprocessable(
            "PRODUCT_IMAGE_NOT_UPLOADED",
            "No uploaded object was found for this image",
        ));
    }
    if meta.size_bytes.is_some_and(|s| s > MAX_PRODUCT_IMAGE_BYTES) {
        return Err(too_large());
    }
    let path = product_image_path(&product.uuid, Some(object_key));
    let updated = set_image_columns(
        state,
        audit,
        product.id,
        path.as_deref(),
        Some(object_key),
        true,
    )
    .await?;
    if let Some(old) = product
        .image_object_key
        .as_deref()
        .filter(|k| *k != object_key)
    {
        delete_quietly(state, old).await;
    }
    Ok(to_response(state, &updated))
}

/// Node: src/products/product-images.service.ts:207 `remove` — idempoten.
pub async fn remove(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    product_uuid: &str,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let product = find_row_or_throw(state, company_id, product_uuid).await?;
    let Some(old) = product.image_object_key.clone() else {
        return Ok(to_response(state, &product));
    };
    let updated = set_image_columns(state, audit, product.id, None, None, false).await?;
    delete_quietly(state, &old).await;
    Ok(to_response(state, &updated))
}

/// Node: src/products/admin-products.controller.ts:430 `createImageUploadUrl`
async fn h_upload_url(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(id): Path<String>,
    ValidatedJson(body): ValidatedJson<Raw>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let mut e = Vec::new();
    let content_type = check_string(&mut e, "contentType", body.0.get("contentType"));
    let size = check_int_min(&mut e, "sizeBytes", body.0.get("sizeBytes"), 1);
    let (Some(content_type), Some(size), true) = (content_type, size, e.is_empty()) else {
        return Err(ApiError::Validation(e));
    };
    let data = create_upload_url(&state, ctx.require_user_id()?, &id, &content_type, size).await?;
    Ok((StatusCode::CREATED, Json(json!({ "data": data }))))
}

/// Node: src/products/admin-products.controller.ts:466 `confirmImage` (`@HttpCode(200)`)
async fn h_confirm(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    Path(id): Path<String>,
    ValidatedJson(body): ValidatedJson<Raw>,
) -> ApiResult<Json<Value>> {
    let mut e = Vec::new();
    let key = check_max_string(&mut e, "objectKey", body.0.get("objectKey"), 500);
    let Some(key) = key.filter(|_| e.is_empty()) else {
        return Err(ApiError::Validation(e));
    };
    let data = confirm(&state, &audit, ctx.require_user_id()?, &id, &key).await?;
    Ok(Json(json!({ "data": data })))
}

/// Node: src/products/admin-products.controller.ts:502 `removeImage`
async fn h_remove(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let data = remove(&state, &audit, ctx.require_user_id()?, &id).await?;
    Ok(Json(json!({ "data": data })))
}

/// Node: src/products/product-image-public.controller.ts:24 `image` →
/// `ProductImagesService.streamPublicImage` (product-images.service.ts:233).
async fn h_public_image(
    State(state): State<AppState>,
    Path((product_uuid, file_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let product_uuid = parse_uuid_pipe(&product_uuid)?;
    parse_uuid_pipe(&file_id)?;
    let key: Option<Option<String>> = sqlx::query_scalar(
        r#"SELECT "imageObjectKey" FROM product WHERE uuid = $1 AND "deletedAt" IS NULL LIMIT 1"#,
    )
    .bind(product_uuid)
    .fetch_optional(&state.db)
    .await?;
    let key = key.flatten();
    let matches = product_image_file_id(key.as_deref()) == Some(file_id.to_lowercase());
    let Some(key) = key.filter(|_| matches) else {
        return Err(not_found("PRODUCT_IMAGE_NOT_FOUND", "Image not found"));
    };
    mz_files::stream_object(
        &state,
        &key,
        Some(&headers),
        mz_files::StreamObjectOptions {
            cache_control: Some(PRODUCT_IMAGE_CACHE_CONTROL.to_owned()),
            ..Default::default()
        },
    )
    .await
}

/// 3 route admin gambar + 1 route publik.
pub fn routes(state: &AppState) -> Router<AppState> {
    let s = crate::PRODUCTS;
    Router::new()
        .route(
            "/v1/admin/products/{id}/image",
            guard(state, s, post(h_upload_url)),
        )
        .route(
            "/v1/admin/products/{id}/image",
            guard(state, s, delete(h_remove)),
        )
        .route(
            "/v1/admin/products/{id}/image/confirm",
            guard(state, s, post(h_confirm)),
        )
        .route(
            "/products/{product_uuid}/image/{file_id}",
            guard(state, PUBLIC_IMAGE, get(h_public_image)),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_milik_produk() {
        let p = "9f1c2d3e-4a5b-4c7d-8e9f-0a1b2c3d4e5f";
        let ok = format!("products/10/{p}/7c2a1b30-9d44-4e21-8f77-1b2c3d4e5f60.jpg");
        assert!(assert_key_owned(10, p, &ok).is_ok());
        assert!(assert_key_owned(11, p, &ok).is_err());
        assert!(assert_key_owned(10, p, &format!("{ok}x")).is_err());
        assert!(assert_key_owned(10, p, &format!("products/10/{p}/../x.jpg")).is_err());
    }
}
