//! Port `src/mobile-catalog/**`: `GET /v1/mobile/catalog/sync` (§10 docs/product_master.md) —
//! token dulu (satu query agregat, ETag/304), payload belakangan; cache Redis per
//! `(company, token, cursor, limit)`; cursor keyset `products` + snapshot master/price list.
//!
//! Kompresi gzip khusus route di Node (`main.ts`): di Rust sudah ada `CompressionLayer` global di
//! `mz-server`, jadi tidak dipasang lagi di sini.

use std::collections::{HashMap, HashSet};

use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use base64::Engine;
use base64::engine::DecodePaddingMode;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};
use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use fred::prelude::*;
use mz_core::company::require_company_id;
use mz_core::extract::ValidatedQuery;
use mz_core::gate::guard;
use mz_core::rate_limit::RateRule;
use mz_core::{ApiError, ApiResult, AppState, RequestCtx, RouteSpec};
use rust_decimal::Decimal;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::common::dec_str;
use crate::dto::{Raw, check_string, parse_int_value};
use crate::util::product_image_url;
use crate::{MOBILE_ROLES, SALES};

/// `CATALOG_CURSOR_VERSION` (catalog-cursor.ts:31).
pub const CATALOG_CURSOR_VERSION: i64 = 3;
/// `CATALOG_SYNC_DEFAULT_LIMIT` / `CATALOG_SYNC_MAX_LIMIT`.
pub const DEFAULT_LIMIT: i64 = 500;
pub const MAX_LIMIT: i64 = 2000;
/// `CATALOG_PAYLOAD_EPOCH_OFFSET`.
const EPOCH_OFFSET: i64 = 1000;
/// `SAFETY_LAG_MS` (§10.6).
pub const SAFETY_LAG_MS: i64 = 5_000;
const MASTER_ROWS_CAP: i64 = 10_000;
/// TTL cache payload (catalog-cache.service.ts:27).
const CACHE_TTL_SECS: i64 = 24 * 60 * 60;

/// `main.ts` — `rl:mobile-catalog-sync` 60/jam per token.
const SYNC_LIMIT: RateRule = RateRule::token("rl:mobile-catalog-sync", 60, 60 * 60 * 1000);
/// `MobileCatalogController` (`@RequireFeature('products')`, `@RequireModule(SALES_TRACKER)`).
const SYNC: RouteSpec = RouteSpec::jwt()
    .feature("products")
    .modules(SALES)
    .roles(MOBILE_ROLES)
    .rate_limits(&[SYNC_LIMIT]);

// ───────────────────────── cursor ─────────────────────────

/// `CatalogStreamPosition` — `u` = `updatedAt` epoch ms, `i` = id.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StreamPosition {
    pub u: f64,
    pub i: f64,
}

/// `CatalogCursor`.
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogCursor {
    pub v: i64,
    pub epoch: f64,
    pub products: Option<StreamPosition>,
    pub since: Option<f64>,
}

/// `emptyCursor(epoch)`.
pub fn empty_cursor(epoch: f64) -> CatalogCursor {
    CatalogCursor {
        v: CATALOG_CURSOR_VERSION,
        epoch,
        products: None,
        since: None,
    }
}

const B64URL: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::URL_SAFE,
    GeneralPurposeConfig::new()
        .with_encode_padding(false)
        .with_decode_padding_mode(DecodePaddingMode::Indifferent)
        .with_decode_allow_trailing_bits(true),
);

fn num(x: f64) -> Value {
    mz_core::jsfmt::num(x)
}

/// `encodeCatalogCursor` — base64url JSON `{v, epoch, products, since}`.
pub fn encode_cursor(c: &CatalogCursor) -> String {
    let products = c
        .products
        .map_or(Value::Null, |p| json!({ "u": num(p.u), "i": num(p.i) }));
    let v = json!({
        "v": c.v,
        "epoch": num(c.epoch),
        "products": products,
        "since": c.since.map_or(Value::Null, num),
    });
    B64URL.encode(v.to_string())
}

fn finite(v: Option<&Value>) -> Option<f64> {
    v.and_then(Value::as_f64).filter(|x| x.is_finite())
}

/// `decodeCatalogCursor` — `None` untuk apa pun yang rusak (full resync, bukan 400).
pub fn decode_cursor(raw: &str) -> Option<CatalogCursor> {
    // Buffer.from(raw, 'base64url') toleran: `+`/`/` diterima, karakter asing dibuang.
    let cleaned: String = raw
        .chars()
        .filter_map(|c| match c {
            '+' => Some('-'),
            '/' => Some('_'),
            c if c.is_ascii_alphanumeric() || c == '-' || c == '_' => Some(c),
            _ => None,
        })
        .collect();
    let bytes = B64URL.decode(cleaned.as_bytes()).ok()?;
    let parsed: Value = serde_json::from_slice(&bytes).ok()?;
    let obj = parsed.as_object()?;
    if obj.get("v").and_then(Value::as_f64) != Some(3.0) {
        return None;
    }
    let epoch = finite(obj.get("epoch"))?;
    let products = match obj.get("products") {
        Some(Value::Null) => None,
        Some(Value::Object(p)) => Some(StreamPosition {
            u: finite(p.get("u"))?,
            i: finite(p.get("i"))?,
        }),
        _ => return None,
    };
    let since = match obj.get("since") {
        Some(Value::Null) => None,
        v => Some(finite(v)?),
    };
    Some(CatalogCursor {
        v: CATALOG_CURSOR_VERSION,
        epoch,
        products,
        since,
    })
}

/// `matchesEtag(header, token)`: daftar dipisah koma, prefiks lemah `W/` diabaikan.
pub fn matches_etag(header: Option<&str>, token: &str) -> bool {
    let Some(h) = header.filter(|h| !h.is_empty()) else {
        return false;
    };
    let expected = format!("\"{token}\"");
    h.split(',')
        .map(str::trim)
        .map(|t| t.strip_prefix("W/").unwrap_or(t))
        .any(|t| t == expected)
}

/// `catalogToken`: `sha256("3|epoch|maxMs")[..32]`.
pub fn catalog_token(epoch: i64, max_updated_ms: Option<i64>) -> String {
    let src = format!(
        "{CATALOG_CURSOR_VERSION}|{epoch}|{}",
        max_updated_ms.map_or_else(|| "null".to_owned(), |m| m.to_string())
    );
    let mut h = format!("{:x}", Sha256::digest(src.as_bytes()));
    h.truncate(32);
    h
}

// ───────────────────────── DTO ─────────────────────────

/// `CatalogSyncQueryDto`.
pub struct SyncQuery {
    pub cursor: Option<String>,
    pub limit: Option<i64>,
}

pub fn parse_query(m: &Map<String, Value>) -> Result<SyncQuery, Vec<String>> {
    let mut e = Vec::new();
    let cursor = match m.get("cursor") {
        None | Some(Value::Null) => None,
        v => check_string(&mut e, "cursor", v),
    };
    let blank = |v: &Value| v.is_null() || v.as_str() == Some("");
    let limit = match m.get("limit") {
        Some(v) if !blank(v) => {
            #[allow(clippy::cast_precision_loss)]
            let n = parse_int_value(v).map(|n| n as f64);
            mz_core::extract::check_int_range(&mut e, "limit", n, 1, MAX_LIMIT)
        }
        _ => None,
    };
    if e.is_empty() {
        Ok(SyncQuery { cursor, limit })
    } else {
        Err(e)
    }
}

// ───────────────────────── service ─────────────────────────

/// Hasil `sync`: `body = None` → 304.
pub struct SyncResult {
    pub catalog_token: String,
    pub body: Option<Value>,
}

struct TokenResult {
    epoch: i64,
    token: String,
    per_table: HashMap<String, Option<i64>>,
}

fn ms_of(dt: NaiveDateTime) -> i64 {
    dt.and_utc().timestamp_millis()
}

/// `computeCatalogToken` (mobile-catalog.service.ts:214).
async fn compute_token(db: &sqlx::PgPool, company_id: i32) -> ApiResult<TokenResult> {
    let epoch: Option<i32> =
        sqlx::query_scalar(r#"SELECT "catalogEpoch" FROM company WHERE id = $1"#)
            .bind(company_id)
            .fetch_optional(db)
            .await?;
    let rows: Vec<(String, Option<NaiveDateTime>)> = sqlx::query_as(
        r#"SELECT 'product' AS tbl, MAX("updatedAt") AS m FROM "product" WHERE "companyId" = $1
        UNION ALL SELECT 'productCategory', MAX("updatedAt") FROM "productCategory" WHERE "companyId" = $1
        UNION ALL SELECT 'productSubCategory', MAX("updatedAt") FROM "productSubCategory" WHERE "companyId" = $1
        UNION ALL SELECT 'productBrand', MAX("updatedAt") FROM "productBrand" WHERE "companyId" = $1
        UNION ALL SELECT 'productPrincipal', MAX("updatedAt") FROM "productPrincipal" WHERE "companyId" = $1
        UNION ALL SELECT 'uom', MAX("updatedAt") FROM "uom" WHERE "companyId" = $1
        UNION ALL SELECT 'taxRate', MAX("updatedAt") FROM "taxRate" WHERE "companyId" = $1
        UNION ALL SELECT 'productCustomField', MAX("updatedAt") FROM "productCustomField" WHERE "companyId" = $1
        UNION ALL SELECT 'priceList', MAX("updatedAt") FROM "priceList" WHERE "companyId" = $1
        UNION ALL SELECT 'priceListItem', MAX("updatedAt") FROM "priceListItem" WHERE "companyId" = $1
        UNION ALL SELECT 'customer', MAX("updatedAt") FROM "customer" WHERE "companyId" = $1"#,
    )
    .bind(company_id)
    .fetch_all(db)
    .await?;
    let epoch = i64::from(epoch.unwrap_or(1)) + EPOCH_OFFSET;
    let mut per_table = HashMap::new();
    let mut max: Option<i64> = None;
    for (tbl, m) in rows {
        let ms = m.map(ms_of);
        if let Some(v) = ms
            && max.is_none_or(|x| v > x)
        {
            max = Some(v);
        }
        per_table.insert(tbl, ms);
    }
    Ok(TokenResult {
        epoch,
        token: catalog_token(epoch, max),
        per_table,
    })
}

async fn cache_get(state: &AppState, key: &str) -> Option<String> {
    match state.redis.next().get::<Option<String>, _>(key).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("Cache get failed for {key}: {e}");
            None
        }
    }
}

async fn cache_set(state: &AppState, key: &str, value: String) {
    let res: Result<(), _> = state
        .redis
        .next()
        .set(
            key,
            value,
            Some(Expiration::EX(CACHE_TTL_SECS)),
            None,
            false,
        )
        .await;
    if let Err(e) = res {
        tracing::warn!("Cache set failed for {key}: {e}");
    }
}

/// §10.3 aturan 2 — `null` dibuang dari objek.
fn omit_nullish(m: Map<String, Value>) -> Value {
    Value::Object(m.into_iter().filter(|(_, v)| !v.is_null()).collect())
}

fn obj(pairs: Vec<(&str, Value)>) -> Value {
    omit_nullish(pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
}

#[derive(sqlx::FromRow)]
struct MasterRef {
    uuid: String,
    code: String,
    name: String,
    #[sqlx(rename = "isActive")]
    is_active: bool,
}

/// `fetchMasters` — hanya kind yang tabelnya berubah sejak `since`.
async fn fetch_masters(
    db: &sqlx::PgPool,
    company_id: i32,
    changed: &(dyn Fn(&str) -> bool + Sync),
) -> ApiResult<Option<Value>> {
    const KINDS: [(&str, &str, bool); 6] = [
        ("categories", "productCategory", true),
        ("subCategories", "productSubCategory", true),
        ("brands", "productBrand", true),
        ("principals", "productPrincipal", false),
        ("uoms", "uom", false),
        ("taxes", "taxRate", false),
    ];
    let mut out = Map::new();
    for (key, table, sort) in KINDS {
        if !changed(table) {
            continue;
        }
        let order = if sort {
            r#""sortOrder" ASC, id ASC"#
        } else {
            "name ASC, id ASC"
        };
        let rows: Vec<MasterRef> = sqlx::query_as(&format!(
            r#"SELECT uuid::text AS uuid, code, name, "isActive" FROM "{table}" WHERE "companyId" = $1 ORDER BY {order} LIMIT $2"#
        ))
        .bind(company_id)
        .bind(MASTER_ROWS_CAP)
        .fetch_all(db)
        .await?;
        let list: Vec<Value> = rows
            .into_iter()
            .map(|r| json!({ "uuid": r.uuid, "code": r.code, "name": r.name, "isActive": r.is_active }))
            .collect();
        out.insert(key.to_owned(), Value::Array(list));
    }
    Ok((!out.is_empty()).then_some(Value::Object(out)))
}

async fn fetch_custom_field_defs(db: &sqlx::PgPool, company_id: i32) -> ApiResult<Value> {
    let rows: Vec<(String, String, String, bool)> = sqlx::query_as(
        r#"SELECT key, label, type::text, "isSensitive" FROM "productCustomField"
           WHERE "companyId" = $1 AND entity = 'product' AND "isActive" = true
           ORDER BY "sortOrder" ASC, id ASC LIMIT $2"#,
    )
    .bind(company_id)
    .bind(MASTER_ROWS_CAP)
    .fetch_all(db)
    .await?;
    Ok(Value::Array(
        rows.into_iter()
            .filter(|r| !r.3)
            .map(|(key, label, t, _)| json!({ "key": key, "label": label, "type": t }))
            .collect(),
    ))
}

#[derive(sqlx::FromRow)]
struct PlHeader {
    id: i32,
    uuid: String,
    code: String,
    name: String,
    #[sqlx(rename = "currencyCode")]
    currency_code: String,
    #[sqlx(rename = "startDate")]
    start_date: Option<NaiveDate>,
    #[sqlx(rename = "endDate")]
    end_date: Option<NaiveDate>,
    #[sqlx(rename = "isDefault")]
    is_default: bool,
    #[sqlx(rename = "isActive")]
    is_active: bool,
}

#[derive(sqlx::FromRow)]
struct PlItem {
    #[sqlx(rename = "priceListId")]
    price_list_id: i32,
    uuid: String,
    #[sqlx(rename = "productUuid")]
    product_uuid: String,
    #[sqlx(rename = "uomUuid")]
    uom_uuid: String,
    #[sqlx(rename = "minQty")]
    min_qty: Decimal,
    #[sqlx(rename = "maxQty")]
    max_qty: Option<Decimal>,
    price: Decimal,
}

#[derive(sqlx::FromRow)]
struct PlCustomer {
    #[sqlx(rename = "priceListId")]
    price_list_id: i32,
    uuid: String,
    code: String,
    name: String,
}

/// `fetchPriceLists` — snapshot utuh, tiga query dalam satu transaksi REPEATABLE READ.
async fn fetch_price_lists(db: &sqlx::PgPool, company_id: i32) -> ApiResult<Value> {
    let mut tx = db.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *tx)
        .await?;
    let headers: Vec<PlHeader> = sqlx::query_as(
        r#"SELECT id, uuid::text AS uuid, code, name, "currencyCode", "startDate", "endDate", "isDefault", "isActive"
           FROM "priceList" WHERE "companyId" = $1 AND "deletedAt" IS NULL ORDER BY id ASC LIMIT $2"#,
    )
    .bind(company_id)
    .bind(MASTER_ROWS_CAP)
    .fetch_all(&mut *tx)
    .await?;
    let items: Vec<PlItem> = sqlx::query_as(
        r#"SELECT pli."priceListId" AS "priceListId", pli.uuid::text AS uuid,
                  pr.uuid::text AS "productUuid", u.uuid::text AS "uomUuid",
                  pli."minQty", pli."maxQty", pli.price
           FROM "priceListItem" pli
           JOIN "priceList" pl ON pl.id = pli."priceListId"
           JOIN "product" pr ON pr.id = pli."productId"
           JOIN "uom" u ON u.id = pli."uomId"
           WHERE pli."companyId" = $1 AND pli."deletedAt" IS NULL AND pl."deletedAt" IS NULL
           ORDER BY pli."priceListId" ASC, pr.id ASC, u.id ASC, pli."minQty" ASC, pli.id ASC"#,
    )
    .bind(company_id)
    .fetch_all(&mut *tx)
    .await?;
    let customers: Vec<PlCustomer> = sqlx::query_as(
        r#"SELECT c."priceListId" AS "priceListId", c.uuid::text AS uuid, c.code, c.name
           FROM "customer" c JOIN "priceList" pl ON pl.id = c."priceListId"
           WHERE c."companyId" = $1 AND c."deletedAt" IS NULL AND pl."deletedAt" IS NULL
           ORDER BY c."priceListId" ASC, c.name ASC, c.id ASC"#,
    )
    .bind(company_id)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;

    let mut items_by: HashMap<i32, Vec<Value>> = HashMap::new();
    for it in items {
        items_by.entry(it.price_list_id).or_default().push(obj(vec![
            ("id", json!(it.uuid)),
            ("productId", json!(it.product_uuid)),
            ("uomId", json!(it.uom_uuid)),
            ("minQty", json!(dec_str(&it.min_qty))),
            (
                "maxQty",
                it.max_qty
                    .as_ref()
                    .map_or(Value::Null, |m| json!(dec_str(m))),
            ),
            ("price", json!(dec_str(&it.price))),
        ]));
    }
    let mut cust_by: HashMap<i32, Vec<Value>> = HashMap::new();
    for c in customers {
        cust_by
            .entry(c.price_list_id)
            .or_default()
            .push(json!({ "id": c.uuid, "code": c.code, "name": c.name }));
    }
    let date =
        |d: Option<NaiveDate>| d.map_or(Value::Null, |d| json!(d.format("%Y-%m-%d").to_string()));
    Ok(Value::Array(
        headers
            .into_iter()
            .map(|h| {
                obj(vec![
                    ("id", json!(h.uuid)),
                    ("code", json!(h.code)),
                    ("name", json!(h.name)),
                    ("currencyCode", json!(h.currency_code)),
                    ("startDate", date(h.start_date)),
                    ("endDate", date(h.end_date)),
                    ("isDefault", json!(h.is_default)),
                    ("isActive", json!(h.is_active)),
                    (
                        "items",
                        Value::Array(items_by.remove(&h.id).unwrap_or_default()),
                    ),
                    (
                        "customers",
                        Value::Array(cust_by.remove(&h.id).unwrap_or_default()),
                    ),
                ])
            })
            .collect(),
    ))
}

#[derive(sqlx::FromRow)]
struct ProductPageRow {
    id: i32,
    uuid: String,
    sku: String,
    name: String,
    #[sqlx(rename = "categoryUuid")]
    category_uuid: String,
    #[sqlx(rename = "subCategoryUuid")]
    sub_category_uuid: Option<String>,
    #[sqlx(rename = "brandUuid")]
    brand_uuid: Option<String>,
    #[sqlx(rename = "principalUuid")]
    principal_uuid: Option<String>,
    #[sqlx(rename = "baseUomUuid")]
    base_uom_uuid: String,
    #[sqlx(rename = "taxRateUuid")]
    tax_rate_uuid: Option<String>,
    #[sqlx(rename = "imageObjectKey")]
    image_object_key: Option<String>,
    #[sqlx(rename = "imageUpdatedAt")]
    image_updated_at: Option<NaiveDateTime>,
    #[sqlx(rename = "isOrderable")]
    is_orderable: bool,
    #[sqlx(rename = "isActive")]
    is_active: bool,
    #[sqlx(rename = "customFields")]
    custom_fields: Value,
    #[sqlx(rename = "deletedAt")]
    deleted_at: Option<NaiveDateTime>,
    #[sqlx(rename = "updatedAt")]
    updated_at: NaiveDateTime,
}

#[derive(sqlx::FromRow)]
struct UomRow {
    #[sqlx(rename = "productId")]
    product_id: i32,
    #[sqlx(rename = "uomUuid")]
    uom_uuid: String,
    operator: String,
    factor: Decimal,
    #[sqlx(rename = "conversionRate")]
    conversion_rate: Decimal,
    #[sqlx(rename = "isBase")]
    is_base: bool,
    #[sqlx(rename = "isSalesUom")]
    is_sales_uom: bool,
}

struct ProductsPage {
    products: Vec<Value>,
    deleted: Vec<String>,
    has_more: bool,
    next: Option<StreamPosition>,
}

fn from_ms(ms: i64) -> NaiveDateTime {
    DateTime::<Utc>::from_timestamp_millis(ms)
        .unwrap_or_default()
        .naive_utc()
}

/// `fetchProductsPage` — keyset `(updatedAt, id)`, dibatasi `updatedAt <= upperBound`.
async fn fetch_products_page(
    state: &AppState,
    company_id: i32,
    pos: StreamPosition,
    upper: NaiveDateTime,
    limit: i64,
    sensitive: &HashSet<String>,
) -> ApiResult<ProductsPage> {
    #[allow(clippy::cast_possible_truncation)]
    let (pos_u, pos_i) = (from_ms(pos.u.trunc() as i64), pos.i.trunc() as i64);
    let rows: Vec<ProductPageRow> = sqlx::query_as(
        r#"SELECT p.id, p.uuid::text AS uuid, p.sku, p.name,
                  pc.uuid::text AS "categoryUuid", psc.uuid::text AS "subCategoryUuid",
                  pb.uuid::text AS "brandUuid", pp.uuid::text AS "principalUuid",
                  bu.uuid::text AS "baseUomUuid", tr.uuid::text AS "taxRateUuid",
                  p."imageObjectKey", p."imageUpdatedAt", p."isOrderable", p."isActive", p."customFields",
                  p."deletedAt", p."updatedAt"
           FROM "product" p
           JOIN "productCategory" pc ON pc.id = p."categoryId"
           LEFT JOIN "productSubCategory" psc ON psc.id = p."subCategoryId"
           LEFT JOIN "productBrand" pb ON pb.id = p."brandId"
           LEFT JOIN "productPrincipal" pp ON pp.id = p."principalId"
           JOIN "uom" bu ON bu.id = p."baseUomId"
           LEFT JOIN "taxRate" tr ON tr.id = p."taxRateId"
           WHERE p."companyId" = $1
             AND (p."updatedAt", p.id::bigint) > ($2, $3)
             AND p."updatedAt" <= $4
           ORDER BY p."updatedAt" ASC, p.id ASC
           LIMIT $5"#,
    )
    .bind(company_id)
    .bind(pos_u)
    .bind(pos_i)
    .bind(upper)
    .bind(limit + 1)
    .fetch_all(&state.db)
    .await?;
    let has_more = rows.len() as i64 > limit;
    let page: Vec<ProductPageRow> = rows
        .into_iter()
        .take(usize::try_from(limit).unwrap_or(0))
        .collect();
    #[allow(clippy::cast_precision_loss)]
    let next = page.last().map(|r| StreamPosition {
        u: ms_of(r.updated_at) as f64,
        i: f64::from(r.id),
    });
    let deleted: Vec<String> = page
        .iter()
        .filter(|r| r.deleted_at.is_some())
        .map(|r| r.uuid.clone())
        .collect();
    let alive: Vec<&ProductPageRow> = page.iter().filter(|r| r.deleted_at.is_none()).collect();
    let ids: Vec<i32> = alive.iter().map(|r| r.id).collect();
    let (uoms, barcodes): (Vec<UomRow>, Vec<(i32, String)>) = if ids.is_empty() {
        (Vec::new(), Vec::new())
    } else {
        let u = sqlx::query_as(
            r#"SELECT pu."productId" AS "productId", u.uuid::text AS "uomUuid",
                      pu."operator"::text AS "operator", pu."factor", pu."conversionRate", pu."isBase", pu."isSalesUom"
               FROM "productUom" pu JOIN "uom" u ON u.id = pu."uomId"
               WHERE pu."productId" = ANY($1) ORDER BY pu.id"#,
        )
        .bind(&ids)
        .fetch_all(&state.db)
        .await?;
        let b = sqlx::query_as(
            r#"SELECT "productId", barcode FROM "productBarcode" WHERE "productId" = ANY($1) ORDER BY id"#,
        )
        .bind(&ids)
        .fetch_all(&state.db)
        .await?;
        (u, b)
    };
    let mut uoms_by: HashMap<i32, Vec<Value>> = HashMap::new();
    for u in uoms {
        uoms_by.entry(u.product_id).or_default().push(obj(vec![
            ("uomId", json!(u.uom_uuid)),
            ("operator", json!(u.operator)),
            ("factor", json!(dec_str(&u.factor))),
            ("rate", json!(dec_str(&u.conversion_rate))),
            ("isBase", json!(u.is_base)),
            ("isSalesUom", json!(u.is_sales_uom)),
        ]));
    }
    let mut bc_by: HashMap<i32, Vec<Value>> = HashMap::new();
    for (pid, bc) in barcodes {
        bc_by.entry(pid).or_default().push(json!(bc));
    }
    let products = alive
        .iter()
        .map(|r| {
            let cf: Map<String, Value> = r
                .custom_fields
                .as_object()
                .map(|m| {
                    m.iter()
                        .filter(|(k, _)| !sensitive.contains(*k))
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect()
                })
                .unwrap_or_default();
            obj(vec![
                ("id", json!(r.uuid)),
                ("sku", json!(r.sku)),
                ("name", json!(r.name)),
                ("categoryId", json!(r.category_uuid)),
                ("subCategoryId", json!(r.sub_category_uuid)),
                ("brandId", json!(r.brand_uuid)),
                ("principalId", json!(r.principal_uuid)),
                ("baseUomId", json!(r.base_uom_uuid)),
                ("taxRateId", json!(r.tax_rate_uuid)),
                (
                    "imageUrl",
                    json!(product_image_url(
                        state,
                        &r.uuid,
                        r.image_object_key.as_deref()
                    )),
                ),
                ("imageUpdatedAt", json!(r.image_updated_at.map(ms_of))),
                ("isOrderable", json!(r.is_orderable)),
                ("isActive", json!(r.is_active)),
                (
                    "uoms",
                    Value::Array(uoms_by.remove(&r.id).unwrap_or_default()),
                ),
                (
                    "barcodes",
                    Value::Array(bc_by.remove(&r.id).unwrap_or_default()),
                ),
                ("cf", Value::Object(cf)),
                ("updatedAt", json!(ms_of(r.updated_at))),
            ])
        })
        .collect();
    Ok(ProductsPage {
        products,
        deleted,
        has_more,
        next,
    })
}

/// Node: src/mobile-catalog/mobile-catalog.service.ts:266 `sync`
pub async fn sync(
    state: &AppState,
    user_id: i32,
    q: &SyncQuery,
    if_none_match: Option<&str>,
) -> ApiResult<SyncResult> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let TokenResult {
        epoch,
        token,
        per_table,
    } = compute_token(&state.db, company_id).await?;
    if matches_etag(if_none_match, &token) {
        return Ok(SyncResult {
            catalog_token: token,
            body: None,
        });
    }
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT);
    let cache_key = format!(
        "catalog:{company_id}:{token}:{}:{limit}",
        q.cursor.as_deref().unwrap_or("full")
    );
    if let Some(cached) = cache_get(state, &cache_key).await
        && let Ok(mut body) = serde_json::from_str::<Value>(&cached)
    {
        if let Some(o) = body.as_object_mut() {
            o.insert("syncedAt".into(), json!(Utc::now().timestamp_millis()));
        }
        return Ok(SyncResult {
            catalog_token: token,
            body: Some(body),
        });
    }

    let decoded = q
        .cursor
        .as_deref()
        .filter(|c| !c.is_empty())
        .and_then(decode_cursor);
    #[allow(clippy::cast_precision_loss)]
    let epoch_f = epoch as f64;
    let (mode, cursor) = match decoded {
        Some(c) if c.epoch == epoch_f => ("delta", c),
        _ => ("full", empty_cursor(epoch_f)),
    };
    let upper_ms = Utc::now().timestamp_millis() - SAFETY_LAG_MS;
    let upper = from_ms(upper_ms);
    let since = if mode == "full" { None } else { cursor.since };
    let threshold = since.unwrap_or(f64::NEG_INFINITY);
    let changed = |table: &str| -> bool {
        if mode == "full" {
            return true;
        }
        #[allow(clippy::cast_precision_loss)]
        per_table
            .get(table)
            .copied()
            .flatten()
            .is_some_and(|ms| ms as f64 > threshold)
    };
    let price_lists_changed =
        changed("priceList") || changed("priceListItem") || changed("customer");

    let sensitive: HashSet<String> = sqlx::query_scalar(
        r#"SELECT key FROM "productCustomField" WHERE "companyId" = $1 AND entity = 'product' AND "isSensitive" = true"#,
    )
    .bind(company_id)
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .collect();
    let masters = fetch_masters(&state.db, company_id, &changed).await?;
    let custom_fields = if changed("productCustomField") {
        Some(fetch_custom_field_defs(&state.db, company_id).await?)
    } else {
        None
    };
    let price_lists = if price_lists_changed {
        Some(fetch_price_lists(&state.db, company_id).await?)
    } else {
        None
    };
    let page = fetch_products_page(
        state,
        company_id,
        cursor.products.unwrap_or(StreamPosition { u: 0.0, i: 0.0 }),
        upper,
        limit,
        &sensitive,
    )
    .await?;
    #[allow(clippy::cast_precision_loss)]
    let next = CatalogCursor {
        v: CATALOG_CURSOR_VERSION,
        epoch: epoch_f,
        products: page.next.or(cursor.products),
        since: Some(upper_ms as f64),
    };
    let body = obj(vec![
        ("epoch", json!(epoch)),
        ("mode", json!(mode)),
        ("syncedAt", json!(Utc::now().timestamp_millis())),
        ("catalogToken", json!(token)),
        ("hasMore", json!(page.has_more)),
        ("nextCursor", json!(encode_cursor(&next))),
        ("masters", masters.unwrap_or(Value::Null)),
        ("customFields", custom_fields.unwrap_or(Value::Null)),
        ("products", Value::Array(page.products)),
        ("productsDeleted", json!(page.deleted)),
        ("priceLists", price_lists.unwrap_or(Value::Null)),
    ]);
    cache_set(state, &cache_key, body.to_string()).await;
    Ok(SyncResult {
        catalog_token: token,
        body: Some(body),
    })
}

/// Node: src/mobile-catalog/mobile-catalog.controller.ts:48 `sync`
pub async fn h_sync(
    State(state): State<AppState>,
    ctx: RequestCtx,
    headers: HeaderMap,
    ValidatedQuery(q): ValidatedQuery<Raw>,
) -> ApiResult<Response> {
    let q = parse_query(&q.0).map_err(ApiError::Validation)?;
    let inm = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok());
    let res = sync(&state, ctx.require_user_id()?, &q, inm).await?;
    let etag = HeaderValue::from_str(&format!("\"{}\"", res.catalog_token))
        .map_err(|e| ApiError::internal(anyhow::anyhow!("{e}")))?;
    let mut resp = match res.body {
        Some(b) => Json(b).into_response(),
        None => StatusCode::NOT_MODIFIED.into_response(),
    };
    resp.headers_mut().insert(header::ETAG, etag);
    Ok(resp)
}

/// 1 route `GET /v1/mobile/catalog/sync`.
pub fn routes(state: &AppState) -> Router<AppState> {
    Router::new().route("/v1/mobile/catalog/sync", guard(state, SYNC, get(h_sync)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(v: Value) -> String {
        B64URL.encode(v.to_string())
    }

    // catalog-cursor.spec.ts
    #[test]
    fn round_trip() {
        let c = CatalogCursor {
            v: 3,
            epoch: 3.0,
            products: Some(StreamPosition { u: 1000.0, i: 5.0 }),
            since: Some(1500.0),
        };
        assert_eq!(decode_cursor(&encode_cursor(&c)), Some(c));
        assert_eq!(
            empty_cursor(7.0),
            CatalogCursor {
                v: 3,
                epoch: 7.0,
                products: None,
                since: None
            }
        );
        let s = encode_cursor(&empty_cursor(1001.0));
        assert_eq!(
            String::from_utf8(B64URL.decode(s).unwrap()).unwrap(),
            r#"{"v":3,"epoch":1001,"products":null,"since":null}"#
        );
    }

    #[test]
    fn cursor_tidak_valid() {
        let legacy = enc(json!({"epoch": 3, "products": {"u": 1000, "i": 5},
            "priceListItems": {"u": 2000, "i": 9}, "customers": null, "since": 1500}));
        assert_eq!(decode_cursor(&legacy), None);
        assert_eq!(
            decode_cursor(&enc(
                json!({"v": 2, "epoch": 3, "products": {"u": 1000, "i": 5}, "since": 1500})
            )),
            None
        );
        for raw in [
            "not-a-real-cursor".to_owned(),
            B64URL.encode("42"),
            enc(json!({"v": 3, "products": null})),
            enc(json!({"v": 3, "epoch": 1, "products": {"u": "nope"}, "since": null})),
            enc(json!({"v": 3, "epoch": 1, "products": null, "since": "yesterday"})),
            enc(json!({"v": 99, "epoch": 1, "products": null, "since": null})),
        ] {
            assert_eq!(decode_cursor(&raw), None, "{raw}");
        }
    }

    #[test]
    fn etag() {
        assert!(matches_etag(Some("\"abc\""), "abc"));
        assert!(matches_etag(Some("W/\"x\", \"abc\""), "abc"));
        assert!(!matches_etag(Some("abc"), "abc"));
        assert!(!matches_etag(None, "abc"));
        assert_eq!(catalog_token(1001, None).len(), 32);
        assert_ne!(catalog_token(1001, None), catalog_token(1002, None));
    }

    #[test]
    fn query() {
        let m = json!({"limit": "0"});
        assert_eq!(
            parse_query(m.as_object().unwrap()).err().unwrap(),
            vec!["limit must not be less than 1"]
        );
        let m = json!({"limit": "", "cursor": "abc"});
        let q = parse_query(m.as_object().unwrap()).ok().unwrap();
        assert_eq!(q.limit, None);
        assert_eq!(q.cursor.as_deref(), Some("abc"));
    }
}
