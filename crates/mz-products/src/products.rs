//! Port `products/admin-products.controller.ts` (CRUD, list + `cf[...]`, custom-field-summary,
//! export) + `products.service.ts` + `product.delegate.ts`. Route gambar → `images.rs`, import →
//! `import/`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router, http::StatusCode};
use chrono::NaiveDateTime;
use mz_audit::{AuditHandle, Lookup, Model, Row};
use mz_core::company::require_company_id;
use mz_core::extract::ValidatedJson;
use mz_core::gate::guard;
use mz_core::qs::{self, QVal};
use mz_core::time::now_naive;
use mz_core::{ApiError, ApiResult, AppState, RequestCtx};
use mz_table_fields::policy::CensoredColumns;
use rust_decimal::Decimal;
use serde_json::{Map, Value, json};
use sqlx::{PgConnection, PgExecutor, Postgres, QueryBuilder};

use crate::catalog_query::{
    self, CUSTOM_FIELD_SUMMARY_MAX_VALUES, Containment, IdsSort, regular_sort_column,
};
use crate::common::{
    bad_request, conflict, dec_opt, dec_str, internal, like_pattern, ms, ms_opt, not_found, offset,
    page_meta, row_uuid, unprocessable,
};
use crate::custom_fields::{active_types, definitions};
use crate::custom_fields_validator::{
    CfEntity, cast_filter_value, cast_summary_value, js_keys, parse_cf_sort_by, sort_cast,
    validate_custom_field_values,
};
use crate::dto::{
    BARCODE, CONVERSION_RATE, CUSTOM_FIELD_KEY, Field, PRODUCT_SKU, Raw, WEIGHT_VOLUME, check_bool,
    check_enum, check_length_string, check_matches, check_max_string, check_uuid4,
    nested_not_object, re,
};
use crate::masters::{self, MasterKind, MasterRow, ref_json};
use crate::util::{UOM_OPERATORS, compute_base_equivalent, invalid_factor_message};

/// Base UOM bawaan saat `baseUomId` tidak dikirim.
const DEFAULT_BASE_UOM_CODE: &str = "PCS";
/// §17 — batas kandidat jalur `cf[...]`.
pub const MAX_CANDIDATES: usize = 100_000;
/// Batas jumlah kunci `cf[...]` per request.
const MAX_CUSTOM_FIELD_FILTERS: usize = 20;
/// Kunci tabel field-visibility.
pub const TABLE_KEY: &str = "products";

// ───────────────────────── baris ─────────────────────────

/// Baris `product` (semua kolom skalar).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ProductRow {
    pub id: i32,
    pub uuid: String,
    #[sqlx(rename = "companyId")]
    pub company_id: i32,
    pub sku: String,
    pub name: String,
    pub description: Option<String>,
    #[sqlx(rename = "categoryId")]
    pub category_id: i32,
    #[sqlx(rename = "subCategoryId")]
    pub sub_category_id: Option<i32>,
    #[sqlx(rename = "brandId")]
    pub brand_id: Option<i32>,
    #[sqlx(rename = "principalId")]
    pub principal_id: Option<i32>,
    #[sqlx(rename = "baseUomId")]
    pub base_uom_id: i32,
    #[sqlx(rename = "taxRateId")]
    pub tax_rate_id: Option<i32>,
    pub weight: Option<Decimal>,
    pub volume: Option<Decimal>,
    #[sqlx(rename = "imageUrl")]
    pub image_url: Option<String>,
    #[sqlx(rename = "imageObjectKey")]
    pub image_object_key: Option<String>,
    #[sqlx(rename = "imageUpdatedAt")]
    pub image_updated_at: Option<NaiveDateTime>,
    #[sqlx(rename = "isOrderable")]
    pub is_orderable: bool,
    #[sqlx(rename = "isActive")]
    pub is_active: bool,
    #[sqlx(rename = "customFields")]
    pub custom_fields: Value,
    #[sqlx(rename = "createdById")]
    pub created_by_id: i32,
    #[sqlx(rename = "deletedAt")]
    pub deleted_at: Option<NaiveDateTime>,
    #[sqlx(rename = "createdAt")]
    pub created_at: NaiveDateTime,
    #[sqlx(rename = "updatedAt")]
    pub updated_at: Option<NaiveDateTime>,
}

/// Kolom `SELECT` untuk [`ProductRow`] (alias tabel opsional `p.`).
pub const PRODUCT_COLS: &str = r#"id, uuid::text AS uuid, "companyId", sku, name, description,
    "categoryId", "subCategoryId", "brandId", "principalId", "baseUomId", "taxRateId", weight, volume,
    "imageUrl", "imageObjectKey", "imageUpdatedAt", "isOrderable", "isActive", "customFields",
    "createdById", "deletedAt", "createdAt", "updatedAt""#;

/// Baris `productUom`.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ProductUomRow {
    pub id: i32,
    pub uuid: String,
    #[sqlx(rename = "productId")]
    pub product_id: i32,
    #[sqlx(rename = "uomId")]
    pub uom_id: i32,
    #[sqlx(rename = "conversionRate")]
    pub conversion_rate: Decimal,
    pub operator: String,
    pub factor: Decimal,
    #[sqlx(rename = "isBase")]
    pub is_base: bool,
    #[sqlx(rename = "isSalesUom")]
    pub is_sales_uom: bool,
    #[sqlx(rename = "isPurchaseUom")]
    pub is_purchase_uom: bool,
}

const UOM_COLS: &str = r#"id, uuid::text AS uuid, "productId", "uomId", "conversionRate",
    operator::text AS operator, factor, "isBase", "isSalesUom", "isPurchaseUom""#;

/// Baris `productBarcode`.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ProductBarcodeRow {
    pub id: i32,
    pub uuid: String,
    #[sqlx(rename = "productId")]
    pub product_id: i32,
    #[sqlx(rename = "uomId")]
    pub uom_id: Option<i32>,
    pub barcode: String,
}

/// `findFirst({ where: { uuid, companyId, deletedAt: null } })` — produk hidup company.
pub async fn find_live_by_uuid<'e>(
    exec: impl PgExecutor<'e>,
    company_id: i32,
    uuid: uuid::Uuid,
) -> ApiResult<Option<ProductRow>> {
    Ok(sqlx::query_as(&format!(
        r#"SELECT {PRODUCT_COLS} FROM product
           WHERE uuid = $1 AND "companyId" = $2 AND "deletedAt" IS NULL LIMIT 1"#
    ))
    .bind(uuid)
    .bind(company_id)
    .fetch_optional(exec)
    .await?)
}

/// Produk per id (company), termasuk yang sudah dihapus.
pub async fn find_by_ids<'e>(
    exec: impl PgExecutor<'e>,
    company_id: i32,
    ids: &[i32],
) -> ApiResult<Vec<ProductRow>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    Ok(sqlx::query_as(&format!(
        r#"SELECT {PRODUCT_COLS} FROM product WHERE "companyId" = $1 AND id = ANY($2) ORDER BY id"#
    ))
    .bind(company_id)
    .bind(ids)
    .fetch_all(exec)
    .await?)
}

/// `productUom.findMany({ where: { productId: { in } } })`, urut id.
pub async fn uoms_of<'e>(
    exec: impl PgExecutor<'e>,
    product_ids: &[i32],
) -> ApiResult<Vec<ProductUomRow>> {
    if product_ids.is_empty() {
        return Ok(Vec::new());
    }
    Ok(sqlx::query_as(&format!(
        r#"SELECT {UOM_COLS} FROM "productUom" WHERE "productId" = ANY($1) ORDER BY id"#
    ))
    .bind(product_ids)
    .fetch_all(exec)
    .await?)
}

/// `productBarcode.findMany({ where: { productId: { in } } })`, urut id.
pub async fn barcodes_of<'e>(
    exec: impl PgExecutor<'e>,
    product_ids: &[i32],
) -> ApiResult<Vec<ProductBarcodeRow>> {
    if product_ids.is_empty() {
        return Ok(Vec::new());
    }
    Ok(sqlx::query_as(
        r#"SELECT id, uuid::text AS uuid, "productId", "uomId", barcode FROM "productBarcode"
           WHERE "productId" = ANY($1) ORDER BY id"#,
    )
    .bind(product_ids)
    .fetch_all(exec)
    .await?)
}

// ───────────────────────── serializer ─────────────────────────

/// Ref master satu produk (`RefBundle`).
#[derive(Debug, Clone)]
pub struct RefBundle {
    pub category: Option<MasterRow>,
    pub sub_category: Option<MasterRow>,
    pub brand: Option<MasterRow>,
    pub principal: Option<MasterRow>,
    pub base_uom: Option<MasterRow>,
    pub tax_rate: Option<MasterRow>,
}

/// Peta ref master untuk sekumpulan produk (satu query per kind).
pub struct RefMaps(HashMap<MasterKind, HashMap<i32, MasterRow>>);

impl RefMaps {
    pub async fn load(
        conn: &mut PgConnection,
        company_id: i32,
        rows: &[ProductRow],
    ) -> ApiResult<Self> {
        let mut out = HashMap::new();
        let ids = |k: MasterKind| -> Vec<i32> {
            rows.iter()
                .filter_map(|r| match k {
                    MasterKind::Categories => Some(r.category_id),
                    MasterKind::SubCategories => r.sub_category_id,
                    MasterKind::Brands => r.brand_id,
                    MasterKind::Principals => r.principal_id,
                    MasterKind::Uoms => Some(r.base_uom_id),
                    MasterKind::Taxes => r.tax_rate_id,
                })
                .collect()
        };
        for k in MasterKind::ALL {
            out.insert(
                k,
                masters::map_by_ids(&mut *conn, k, company_id, ids(k)).await?,
            );
        }
        Ok(Self(out))
    }

    fn get(&self, k: MasterKind, id: Option<i32>) -> Option<MasterRow> {
        id.and_then(|id| self.0.get(&k).and_then(|m| m.get(&id)).cloned())
    }

    pub fn bundle(&self, r: &ProductRow) -> RefBundle {
        RefBundle {
            category: self.get(MasterKind::Categories, Some(r.category_id)),
            sub_category: self.get(MasterKind::SubCategories, r.sub_category_id),
            brand: self.get(MasterKind::Brands, r.brand_id),
            principal: self.get(MasterKind::Principals, r.principal_id),
            base_uom: self.get(MasterKind::Uoms, Some(r.base_uom_id)),
            tax_rate: self.get(MasterKind::Taxes, r.tax_rate_id),
        }
    }
}

/// `toEntity(row, refs, detail)` (products.service.ts:566). `detail` = `(uoms, barcodes)` JSON.
pub fn to_entity(
    state: &AppState,
    row: &ProductRow,
    refs: &RefBundle,
    detail: Option<(Value, Value)>,
) -> Value {
    let mut m = Map::new();
    m.insert("id".into(), json!(row.uuid));
    m.insert("sku".into(), json!(row.sku));
    m.insert("name".into(), json!(row.name));
    m.insert("description".into(), json!(row.description));
    m.insert("category".into(), ref_json(refs.category.as_ref()));
    m.insert("subCategory".into(), ref_json(refs.sub_category.as_ref()));
    m.insert("brand".into(), ref_json(refs.brand.as_ref()));
    m.insert("principal".into(), ref_json(refs.principal.as_ref()));
    m.insert("baseUom".into(), ref_json(refs.base_uom.as_ref()));
    m.insert("taxRate".into(), ref_json(refs.tax_rate.as_ref()));
    m.insert("weight".into(), dec_opt(row.weight.as_ref()));
    m.insert("volume".into(), dec_opt(row.volume.as_ref()));
    m.insert(
        "imageUrl".into(),
        json!(crate::util::product_image_url(
            state,
            &row.uuid,
            row.image_object_key.as_deref()
        )),
    );
    m.insert("isOrderable".into(), json!(row.is_orderable));
    m.insert("isActive".into(), json!(row.is_active));
    if let Some((uoms, barcodes)) = detail {
        m.insert("uoms".into(), uoms);
        m.insert("barcodes".into(), barcodes);
        m.insert("customFields".into(), row.custom_fields.clone());
    }
    m.insert("createdAt".into(), json!(ms(row.created_at)));
    m.insert("updatedAt".into(), ms_opt(row.updated_at));
    Value::Object(m)
}

/// `buildDetailArrays` (products.service.ts:610).
async fn detail_arrays(
    conn: &mut PgConnection,
    company_id: i32,
    product_id: i32,
) -> ApiResult<(Value, Value)> {
    let uoms = uoms_of(&mut *conn, &[product_id]).await?;
    let barcodes = barcodes_of(&mut *conn, &[product_id]).await?;
    let ids = uoms
        .iter()
        .map(|u| u.uom_id)
        .chain(barcodes.iter().filter_map(|b| b.uom_id));
    let refs = masters::map_by_ids(&mut *conn, MasterKind::Uoms, company_id, ids).await?;
    let uoms: Vec<Value> = uoms
        .iter()
        .map(|u| {
            json!({
                "id": u.uuid,
                "uom": ref_json(refs.get(&u.uom_id)),
                "operator": u.operator,
                "factor": dec_str(&u.factor),
                "baseEquivalent": dec_str(&u.conversion_rate),
                "conversionRate": dec_str(&u.conversion_rate),
                "isBase": u.is_base,
                "isSalesUom": u.is_sales_uom,
                "isPurchaseUom": u.is_purchase_uom,
            })
        })
        .collect();
    let barcodes: Vec<Value> = barcodes
        .iter()
        .map(|b| {
            json!({
                "id": b.uuid,
                "barcode": b.barcode,
                "uom": ref_json(b.uom_id.and_then(|id| refs.get(&id))),
            })
        })
        .collect();
    Ok((Value::Array(uoms), Value::Array(barcodes)))
}

/// `assembleEntity(client, companyId, row, includeDetail)`.
pub async fn assemble_entity(
    state: &AppState,
    conn: &mut PgConnection,
    company_id: i32,
    row: &ProductRow,
    include_detail: bool,
) -> ApiResult<Value> {
    let refs = RefMaps::load(&mut *conn, company_id, std::slice::from_ref(row)).await?;
    let detail = if include_detail {
        Some(detail_arrays(&mut *conn, company_id, row.id).await?)
    } else {
        None
    };
    Ok(to_entity(state, row, &refs.bundle(row), detail))
}

// ───────────────────────── errors ─────────────────────────

fn product_not_found() -> ApiError {
    not_found("PRODUCT_NOT_FOUND", "Product not found")
}

fn sku_taken(sku: &str) -> ApiError {
    conflict(
        "PRODUCT_SKU_TAKEN",
        format!("SKU {sku} is already used by an active product in this company"),
    )
}

fn barcode_taken(barcode: &str, owner: Option<&str>) -> ApiError {
    conflict(
        "BARCODE_TAKEN",
        match owner {
            Some(o) => format!("Barcode {barcode} is already used by SKU {o}"),
            None => format!("Barcode {barcode} is already used by another product in this company"),
        },
    )
}

fn unknown_cf_key(key: &str) -> ApiError {
    bad_request(
        "CUSTOM_FIELD_UNKNOWN_KEY",
        format!("unknown custom field \"{key}\""),
    )
}

fn null_500(field: &str) -> ApiError {
    internal(&format!(
        "{field}: null tidak didukung (TypeError/Prisma di Node)"
    ))
}

// ───────────────────────── DTO: list/export ─────────────────────────

const SORT_FIELDS: &[&str] = &["sku", "name", "createdAt", "updatedAt"];

/// `ListProductsQueryDto` / `ExportProductsQueryDto`.
#[derive(Debug, Default)]
pub struct ListQuery {
    pub search: Option<String>,
    pub category_id: Option<Vec<String>>,
    pub sub_category_id: Option<Vec<String>>,
    pub brand_id: Option<Vec<String>>,
    pub principal_id: Option<Vec<String>>,
    pub tax_rate_id: Option<Vec<String>>,
    pub is_active: Option<bool>,
    pub is_orderable: Option<bool>,
    pub has_image: Option<bool>,
    /// `cf[key]=value` (urutan kunci JS). Nilai bukan string tunggal = `None`.
    pub cf: Vec<(String, Option<String>)>,
    pub sort_by: Option<String>,
    pub sort_desc: bool,
    pub page: i64,
    pub limit: i64,
}

fn qv_to_json(v: &QVal) -> Value {
    match v {
        QVal::One(s) => Value::String(s.clone()),
        QVal::Many(items) => Value::Array(items.iter().cloned().map(Value::String).collect()),
        QVal::Obj => Value::Object(Map::new()),
    }
}

/// `cf[...]` dari query mentah: `Err(())` = `cf` bukan objek (string/array).
/// `(key, nilai)` filter `cf[...]`; `None` = bukan string tunggal.
pub type CfPairs = Vec<(String, Option<String>)>;

pub fn parse_cf(raw: &str) -> (bool, Result<CfPairs, ()>) {
    let mut present = false;
    let mut not_object = false;
    let mut out: Vec<(String, Option<String>)> = Vec::new();
    for part in raw.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = part.split_once('=').unwrap_or((part, ""));
        let k = qs::decode(k);
        let Some(rest) = k.strip_prefix("cf") else {
            continue;
        };
        if rest.is_empty() || rest == "[]" {
            present = true;
            not_object = true;
            continue;
        }
        let Some(inner) = rest.strip_prefix('[') else {
            continue;
        };
        present = true;
        let (key, tail) = inner.split_once(']').unwrap_or((inner, ""));
        let nested = !tail.is_empty();
        let val = qs::decode(v);
        match out.iter_mut().find(|(k2, _)| k2 == key) {
            Some(slot) => slot.1 = None,
            None => out.push((key.to_owned(), (!nested).then_some(val))),
        }
    }
    if !present {
        return (false, Ok(Vec::new()));
    }
    if not_object {
        return (true, Err(()));
    }
    // `Object.entries`: kunci indeks dulu.
    let m: Map<String, Value> = out
        .iter()
        .map(|(k, v)| (k.clone(), v.clone().map_or(Value::Null, Value::String)))
        .collect();
    let order: Vec<String> = js_keys(&m).into_iter().cloned().collect();
    let sorted = order
        .into_iter()
        .filter_map(|k| out.iter().find(|(k2, _)| *k2 == k).cloned())
        .collect();
    (true, Ok(sorted))
}

fn uuid_array(m: &Map<String, Value>, e: &mut Vec<String>, key: &str) -> Option<Vec<String>> {
    let v = m.get(key)?;
    let arr: Vec<Value> = match v {
        Value::Null => return None,
        Value::String(s) if s.is_empty() => return None,
        Value::Array(a) => a.clone(),
        Value::String(s) => s.split(',').map(|x| Value::String(x.to_owned())).collect(),
        _ => vec![Value::String("[object Object]".into())],
    };
    let before = e.len();
    if !arr
        .iter()
        .all(|x| x.as_str().is_some_and(mz_core::uuid_rules::is_uuid_v4))
    {
        e.push(format!("each value in {key} must be a UUID"));
    }
    if arr.len() > 100 {
        e.push(format!("{key} must contain no more than 100 elements"));
    }
    (e.len() == before).then(|| {
        arr.iter()
            .filter_map(|x| x.as_str().map(str::to_owned))
            .collect()
    })
}

/// Validasi query list (`export = true` → tanpa page/limit/cf, `sortBy` `@IsIn`).
pub fn parse_list(raw: &str, export: bool) -> Result<ListQuery, Vec<String>> {
    let parsed = qs::parse(raw);
    let m: Map<String, Value> = parsed
        .iter()
        .map(|(k, v)| (k.clone(), qv_to_json(v)))
        .collect();
    let mut e = Vec::new();
    let mut q = ListQuery {
        page: 1,
        limit: 20,
        ..ListQuery::default()
    };
    q.search = crate::dto::opt_max_string(&m, &mut e, "search", 120);
    q.category_id = uuid_array(&m, &mut e, "categoryId");
    q.sub_category_id = uuid_array(&m, &mut e, "subCategoryId");
    q.brand_id = uuid_array(&m, &mut e, "brandId");
    q.principal_id = uuid_array(&m, &mut e, "principalId");
    q.tax_rate_id = uuid_array(&m, &mut e, "taxRateId");
    q.is_active = crate::dto::query_bool(&m, &mut e, "isActive");
    q.is_orderable = crate::dto::query_bool(&m, &mut e, "isOrderable");
    q.has_image = crate::dto::query_bool(&m, &mut e, "hasImage");
    if !export {
        let (present, cf) = parse_cf(raw);
        match cf {
            Ok(v) => q.cf = v,
            Err(()) if present => e.push("cf must be an object".into()),
            Err(()) => {}
        }
        if let Some(v) = crate::dto::present(&m, "sortBy") {
            static SORT_BY: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
                re(r"^(sku|name|createdAt|updatedAt|cf:[a-z][a-z0-9_]{0,39})$")
            });
            q.sort_by = check_matches(
                &mut e,
                "sortBy",
                Some(v),
                &SORT_BY,
                "sortBy must match /^(sku|name|createdAt|updatedAt|cf:[a-z][a-z0-9_]{0,39})$/ regular expression",
            );
        }
    }
    if let Some(v) = crate::dto::present(&m, "sortDir") {
        q.sort_desc =
            check_enum(&mut e, "sortDir", Some(v), &["asc", "desc"]).as_deref() == Some("desc");
    }
    if export {
        if let Some(v) = crate::dto::present(&m, "sortBy") {
            q.sort_by = check_enum(&mut e, "sortBy", Some(v), SORT_FIELDS);
        }
    } else {
        let (p, l) = crate::dto::page_limit_max(&m, &mut e, 100);
        q.page = p;
        q.limit = l;
    }
    if e.is_empty() { Ok(q) } else { Err(e) }
}

// ───────────────────────── list ─────────────────────────

/// `resolveManyMasterIds`: uuid → id (yang tidak ada dibuang).
async fn resolve_many(
    state: &AppState,
    company_id: i32,
    kind: MasterKind,
    uuids: Option<&Vec<String>>,
) -> ApiResult<Option<Vec<i32>>> {
    let Some(uuids) = uuids.filter(|u| !u.is_empty()) else {
        return Ok(None);
    };
    let parsed: Vec<uuid::Uuid> = uuids
        .iter()
        .filter_map(|u| uuid::Uuid::parse_str(u).ok())
        .collect();
    let rows = masters::find_by_uuids(&state.db, kind, company_id, &parsed).await?;
    let by_uuid: HashMap<&str, i32> = rows.iter().map(|r| (r.uuid.as_str(), r.id)).collect();
    Ok(Some(
        uuids
            .iter()
            .filter_map(|u| by_uuid.get(u.to_lowercase().as_str()).copied())
            .collect(),
    ))
}

/// Klausa `where` (`buildWhere`) yang sudah di-resolve.
#[derive(Debug, Clone)]
pub struct ProductWhere {
    company_id: i32,
    q_search: Option<String>,
    search_cols: Vec<&'static str>,
    is_active: Option<bool>,
    is_orderable: Option<bool>,
    has_image: Option<bool>,
    ids: Vec<(&'static str, Vec<i32>)>,
}

async fn build_where(
    state: &AppState,
    company_id: i32,
    q: &ListQuery,
    censored: &CensoredColumns,
) -> ApiResult<ProductWhere> {
    let mut ids = Vec::new();
    for (col, kind, list) in [
        (r#""categoryId""#, MasterKind::Categories, &q.category_id),
        (
            r#""subCategoryId""#,
            MasterKind::SubCategories,
            &q.sub_category_id,
        ),
        (r#""brandId""#, MasterKind::Brands, &q.brand_id),
        (r#""principalId""#, MasterKind::Principals, &q.principal_id),
        (r#""taxRateId""#, MasterKind::Taxes, &q.tax_rate_id),
    ] {
        if let Some(v) = resolve_many(state, company_id, kind, list.as_ref()).await? {
            ids.push((col, v));
        }
    }
    let search_cols = mz_table_fields::search_sort::allowed_search_branches(
        [("sku", "sku"), ("name", "name"), ("barcode", "barcode")],
        censored,
    );
    Ok(ProductWhere {
        company_id,
        q_search: q.search.clone().filter(|s| !s.is_empty()),
        search_cols,
        is_active: q.is_active,
        is_orderable: q.is_orderable,
        has_image: q.has_image,
        ids,
    })
}

fn push_where(qb: &mut QueryBuilder<'_, Postgres>, w: &ProductWhere) {
    qb.push(r#" WHERE p."companyId" = "#)
        .push_bind(w.company_id)
        .push(r#" AND p."deletedAt" IS NULL"#);
    if let Some(a) = w.is_active {
        qb.push(r#" AND p."isActive" = "#).push_bind(a);
    }
    if let Some(a) = w.is_orderable {
        qb.push(r#" AND p."isOrderable" = "#).push_bind(a);
    }
    match w.has_image {
        Some(true) => {
            qb.push(r#" AND p."imageUrl" IS NOT NULL"#);
        }
        Some(false) => {
            qb.push(r#" AND p."imageUrl" IS NULL"#);
        }
        None => {}
    }
    for (col, ids) in &w.ids {
        qb.push(format!(" AND p.{col} = ANY("))
            .push_bind(ids.clone())
            .push(")");
    }
    if let Some(s) = &w.q_search {
        let p = like_pattern(s);
        if w.search_cols.is_empty() {
            qb.push(" AND false");
        } else {
            qb.push(" AND (");
            for (i, col) in w.search_cols.iter().enumerate() {
                if i > 0 {
                    qb.push(" OR ");
                }
                if *col == "barcode" {
                    qb.push(
                        r#"EXISTS (SELECT 1 FROM "productBarcode" b WHERE b."productId" = p.id AND b.barcode ILIKE "#,
                    )
                    .push_bind(p.clone())
                    .push(")");
                } else {
                    qb.push(format!("p.{col} ILIKE ")).push_bind(p.clone());
                }
            }
            qb.push(")");
        }
    }
}

fn product_select() -> String {
    let cols = PRODUCT_COLS
        .split(',')
        .map(|c| format!("p.{}", c.trim()))
        .collect::<Vec<_>>()
        .join(", ");
    format!("SELECT {cols} FROM product p")
}

fn order_sql(sort_by: Option<&str>, desc: bool) -> String {
    format!(
        "ORDER BY p.{} {}, p.id ASC",
        regular_sort_column(sort_by),
        if desc { "DESC" } else { "ASC" }
    )
}

async fn list_entities(
    state: &AppState,
    company_id: i32,
    rows: &[ProductRow],
) -> ApiResult<Vec<Value>> {
    let mut conn = state.db.acquire().await?;
    let refs = RefMaps::load(&mut conn, company_id, rows).await?;
    Ok(rows
        .iter()
        .map(|r| to_entity(state, r, &refs.bundle(r), None))
        .collect())
}

/// Node: src/products/products.service.ts:893 `findAll`
pub async fn find_all(
    state: &AppState,
    user_id: i32,
    q: &ListQuery,
    censored: &CensoredColumns,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    mz_table_fields::search_sort::assert_sortable_column(
        TABLE_KEY,
        Some(q.sort_by.as_deref().unwrap_or("name")),
        censored,
    )?;
    let uses_cf = !q.cf.is_empty() || q.sort_by.as_deref().is_some_and(|s| s.starts_with("cf:"));
    if uses_cf {
        return find_all_cf(state, company_id, q, censored).await;
    }
    let w = build_where(state, company_id, q, censored).await?;
    let mut qb = QueryBuilder::new(product_select());
    push_where(&mut qb, &w);
    qb.push(" ")
        .push(order_sql(q.sort_by.as_deref(), q.sort_desc));
    qb.push(" LIMIT ")
        .push_bind(q.limit)
        .push(" OFFSET ")
        .push_bind(offset(q.page, q.limit));
    let rows: Vec<ProductRow> = qb.build_query_as().fetch_all(&state.db).await?;
    let mut cq = QueryBuilder::new("SELECT COUNT(*) FROM product p");
    push_where(&mut cq, &w);
    let total: i64 = cq.build_query_scalar().fetch_one(&state.db).await?;
    Ok(json!({
        "data": list_entities(state, company_id, &rows).await?,
        "meta": page_meta(total, q.page, q.limit),
    }))
}

/// `findAllViaDrizzle` (products.service.ts:848).
async fn find_all_cf(
    state: &AppState,
    company_id: i32,
    q: &ListQuery,
    censored: &CensoredColumns,
) -> ApiResult<Value> {
    let types = active_types(&state.db, company_id).await?;
    // resolveCustomFieldFilters
    if q.cf.len() > MAX_CUSTOM_FIELD_FILTERS {
        return Err(bad_request(
            "CUSTOM_FIELD_TOO_MANY_FILTERS",
            format!("at most {MAX_CUSTOM_FIELD_FILTERS} cf[...] filters are allowed per request"),
        ));
    }
    let mut filters = Vec::new();
    for (key, raw) in &q.cf {
        let Some(raw) = raw else {
            return Err(bad_request(
                "CUSTOM_FIELD_NESTED_VALUE",
                format!("cf[{key}] must be a single string value"),
            ));
        };
        let t = types.get(key).ok_or_else(|| unknown_cf_key(key))?;
        let value = cast_filter_value(key, t, raw)
            .map_err(|m| bad_request("CUSTOM_FIELD_TYPE_MISMATCH", m))?;
        filters.push(Containment {
            key: key.clone(),
            value,
        });
    }
    // resolveDrizzleSort
    let sort = match q.sort_by.as_deref().and_then(parse_cf_sort_by) {
        None => IdsSort::Column {
            column: regular_sort_column(q.sort_by.as_deref()),
            desc: q.sort_desc,
        },
        Some(key) => {
            let t = types.get(&key).ok_or_else(|| unknown_cf_key(&key))?;
            if sort_cast(t).is_none() {
                return Err(bad_request(
                    "CUSTOM_FIELD_NOT_SORTABLE",
                    format!("custom field \"{key}\" is multiselect and cannot be used as sortBy"),
                ));
            }
            IdsSort::CustomField {
                key,
                field_type: t.clone(),
                desc: q.sort_desc,
            }
        }
    };
    let w = build_where(state, company_id, q, censored).await?;
    let mut qb = QueryBuilder::new("SELECT p.id FROM product p");
    push_where(&mut qb, &w);
    qb.push(" LIMIT ")
        .push_bind(i64::try_from(MAX_CANDIDATES).unwrap_or(i64::MAX));
    let candidates: Vec<i32> = qb.build_query_scalar().fetch_all(&state.db).await?;
    let (ids, total) = catalog_query::list_product_ids(
        &state.db,
        company_id,
        &candidates,
        &filters,
        &sort,
        offset(q.page, q.limit),
        q.limit,
    )
    .await?;
    let page_rows: Vec<ProductRow> = if ids.is_empty() {
        Vec::new()
    } else {
        sqlx::query_as(&format!(
            r#"SELECT {PRODUCT_COLS} FROM product
               WHERE "companyId" = $1 AND "deletedAt" IS NULL AND id = ANY($2)"#
        ))
        .bind(company_id)
        .bind(&ids)
        .fetch_all(&state.db)
        .await?
    };
    let by_id: HashMap<i32, ProductRow> = page_rows.into_iter().map(|r| (r.id, r)).collect();
    let ordered: Vec<ProductRow> = ids.iter().filter_map(|i| by_id.get(i).cloned()).collect();
    let mut meta = page_meta(total, q.page, q.limit);
    if candidates.len() >= MAX_CANDIDATES
        && let Some(m) = meta.as_object_mut()
    {
        m.insert("truncated".into(), json!(true));
        m.insert("candidateLimit".into(), json!(MAX_CANDIDATES));
    }
    Ok(json!({ "data": list_entities(state, company_id, &ordered).await?, "meta": meta }))
}

/// Node: src/products/products.service.ts:1027 `customFieldSummary`
pub async fn custom_field_summary(state: &AppState, user_id: i32, key: &str) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let types = active_types(&state.db, company_id).await?;
    let t = types.get(key).ok_or_else(|| unknown_cf_key(key))?;
    let rows = catalog_query::custom_field_summary(&state.db, company_id, key, t).await?;
    let values: Vec<Value> = rows
        .iter()
        .map(|(v, c)| json!({ "value": cast_summary_value(t, v.as_deref()), "count": c }))
        .collect();
    let mut out = json!({ "key": key, "values": values });
    if rows.len() >= CUSTOM_FIELD_SUMMARY_MAX_VALUES
        && let Some(m) = out.as_object_mut()
    {
        m.insert("truncated".into(), json!(true));
        m.insert("valueLimit".into(), json!(CUSTOM_FIELD_SUMMARY_MAX_VALUES));
    }
    Ok(out)
}

/// Node: src/products/products.service.ts:942 `exportProducts`
pub async fn export_products(
    state: &AppState,
    user_id: i32,
    q: &ListQuery,
    censored: &CensoredColumns,
) -> ApiResult<Vec<u8>> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let tz = crate::common::company_time_zone(&state.db, company_id)
        .await?
        .ok_or_else(|| ApiError::forbidden("No company found for user"))?;
    let w = build_where(state, company_id, q, censored).await?;
    let mut qb = QueryBuilder::new(product_select());
    push_where(&mut qb, &w);
    qb.push(" ")
        .push(order_sql(q.sort_by.as_deref(), q.sort_desc));
    let rows: Vec<ProductRow> = qb.build_query_as().fetch_all(&state.db).await?;
    let ids: Vec<i32> = rows.iter().map(|r| r.id).collect();
    let mut conn = state.db.acquire().await?;
    let refs = RefMaps::load(&mut conn, company_id, &rows).await?;
    let barcodes = barcodes_of(&mut *conn, &ids).await?;
    let uoms = uoms_of(&mut *conn, &ids).await?;
    let uom_refs = masters::map_by_ids(
        &mut *conn,
        MasterKind::Uoms,
        company_id,
        uoms.iter().map(|u| u.uom_id),
    )
    .await?;
    let mut barcodes_by: HashMap<i32, Vec<String>> = HashMap::new();
    for b in barcodes {
        barcodes_by.entry(b.product_id).or_default().push(b.barcode);
    }
    let mut uoms_by: HashMap<i32, Vec<crate::export::ExportUom>> = HashMap::new();
    for u in &uoms {
        let code = uom_refs
            .get(&u.uom_id)
            .map(|r| r.code.clone())
            .ok_or_else(|| internal("Data integrity: referenced product master row not found"))?;
        uoms_by
            .entry(u.product_id)
            .or_default()
            .push(crate::export::ExportUom {
                code,
                operator: u.operator.clone(),
                factor: dec_str(&u.factor),
                base_equivalent: dec_str(&u.conversion_rate),
                is_base: u.is_base,
            });
    }
    let export_rows: Vec<crate::export::ExportRow> = rows
        .iter()
        .map(|r| {
            let b = refs.bundle(r);
            crate::export::ExportRow {
                id: r.uuid.clone(),
                sku: r.sku.clone(),
                name: r.name.clone(),
                category: b.category,
                sub_category: b.sub_category,
                brand: b.brand,
                principal: b.principal,
                base_uom: b.base_uom,
                tax_rate: b.tax_rate,
                weight: r.weight.as_ref().map(dec_str),
                volume: r.volume.as_ref().map(dec_str),
                barcodes: barcodes_by.remove(&r.id).unwrap_or_default(),
                uoms: uoms_by.remove(&r.id).unwrap_or_default(),
                is_orderable: r.is_orderable,
                is_active: r.is_active,
                created_at: ms(r.created_at),
                updated_at: r.updated_at.map(ms),
            }
        })
        .collect();
    crate::export::build_products_xlsx(&export_rows, &tz, censored)
}

// ───────────────────────── DTO: create/update ─────────────────────────

/// `ProductUomInputDto`.
#[derive(Debug, Clone, Default)]
pub struct UomInput {
    pub uom_id: String,
    pub operator: Option<String>,
    pub factor: Option<String>,
    pub is_base: Option<bool>,
    pub is_sales_uom: Option<bool>,
    pub is_purchase_uom: Option<bool>,
}

/// `ProductBarcodeInputDto`.
#[derive(Debug, Clone, Default)]
pub struct BarcodeInput {
    pub barcode: String,
    pub uom_id: Option<String>,
}

/// `CreateProductDto` / `UpdateProductDto` (tiga keadaan).
#[derive(Debug, Default)]
pub struct ProductDto {
    pub sku: Field<String>,
    pub name: Field<String>,
    pub description: Field<String>,
    pub category_id: Field<String>,
    pub sub_category_id: Field<String>,
    pub brand_id: Field<String>,
    pub principal_id: Field<String>,
    pub base_uom_id: Field<String>,
    pub tax_rate_id: Field<String>,
    pub weight: Field<String>,
    pub volume: Field<String>,
    pub is_orderable: Field<bool>,
    pub is_active: Field<bool>,
    pub uoms: Field<Vec<UomInput>>,
    pub barcodes: Field<Vec<BarcodeInput>>,
    pub custom_fields: Field<Map<String, Value>>,
}

fn tri<T>(
    m: &Map<String, Value>,
    key: &str,
    required: bool,
    check: impl FnOnce(Option<&Value>) -> Option<T>,
) -> Field<T> {
    match m.get(key) {
        None if !required => Field::Absent,
        Some(Value::Null) if !required => Field::Null,
        v => check(v).map_or(Field::Absent, Field::Val),
    }
}

fn opt_tri<T>(
    o: &Map<String, Value>,
    key: &str,
    check: impl FnOnce(&Value) -> Option<T>,
) -> Option<T> {
    o.get(key).filter(|v| !v.is_null()).and_then(check)
}

/// `@IsArray() @ValidateNested({ each: true })` — anak via `child(prefix, obj, errs)`.
fn nested_array<T>(
    e: &mut Vec<String>,
    field: &str,
    v: Option<&Value>,
    mut child: impl FnMut(&str, &Map<String, Value>, &mut Vec<String>) -> Option<T>,
) -> Option<Vec<T>> {
    let Some(Value::Array(items)) = v else {
        e.push(format!("{field} must be an array"));
        if !matches!(v, Some(Value::Object(_))) {
            e.push(nested_not_object(field));
        }
        return None;
    };
    let mut errs = Vec::new();
    let mut out = Vec::new();
    for (i, item) in items.iter().enumerate() {
        match item {
            Value::Object(o) => {
                if let Some(t) = child(&format!("{field}.{i}"), o, &mut errs) {
                    out.push(t);
                }
            }
            _ => errs.push(format!(
                "{field}.{i}.nested property {i} must be either object or array"
            )),
        }
    }
    if errs.is_empty() {
        Some(out)
    } else {
        e.extend(errs);
        None
    }
}

fn parse_uom_input(p: &str, o: &Map<String, Value>, e: &mut Vec<String>) -> Option<UomInput> {
    let before = e.len();
    let uom_id = check_uuid4(e, &format!("{p}.uomId"), o.get("uomId"));
    let operator = opt_tri(o, "operator", |v| {
        check_enum(e, &format!("{p}.operator"), Some(v), UOM_OPERATORS)
    });
    let factor = opt_tri(o, "factor", |v| {
        check_matches(
            e,
            &format!("{p}.factor"),
            Some(v),
            &CONVERSION_RATE,
            &format!("{p}.factor must be a positive number with up to 6 decimal places"),
        )
    });
    // `@IsEmpty({ message })`: '' / null / undefined lolos.
    if o.get("conversionRate")
        .is_some_and(|v| !(v.is_null() || v.as_str() == Some("")))
    {
        e.push(format!(
            "{p}.conversionRate is no longer accepted — send operator and factor instead"
        ));
    }
    let is_base = opt_tri(o, "isBase", |v| {
        check_bool(e, &format!("{p}.isBase"), Some(v))
    });
    let is_sales_uom = opt_tri(o, "isSalesUom", |v| {
        check_bool(e, &format!("{p}.isSalesUom"), Some(v))
    });
    let is_purchase_uom = opt_tri(o, "isPurchaseUom", |v| {
        check_bool(e, &format!("{p}.isPurchaseUom"), Some(v))
    });
    (e.len() == before).then(|| UomInput {
        uom_id: uom_id.unwrap_or_default(),
        operator,
        factor,
        is_base,
        is_sales_uom,
        is_purchase_uom,
    })
}

fn parse_barcode_input(
    p: &str,
    o: &Map<String, Value>,
    e: &mut Vec<String>,
) -> Option<BarcodeInput> {
    let before = e.len();
    let barcode = check_matches(
        e,
        &format!("{p}.barcode"),
        o.get("barcode"),
        &BARCODE,
        &format!("{p}.barcode must be 1-100 letters/digits"),
    );
    let uom_id = opt_tri(o, "uomId", |v| {
        check_uuid4(e, &format!("{p}.uomId"), Some(v))
    });
    (e.len() == before).then(|| BarcodeInput {
        barcode: barcode.unwrap_or_default(),
        uom_id,
    })
}

/// Validasi body produk (`create = true` → `sku`/`name`/`categoryId` wajib).
pub fn parse_product(m: &Map<String, Value>, create: bool) -> Result<ProductDto, Vec<String>> {
    let mut e = Vec::new();
    let mut d = ProductDto {
        sku: tri(m, "sku", create, |v| {
            check_matches(
                &mut e,
                "sku",
                v,
                &PRODUCT_SKU,
                "sku must be 1-100 letters, digits, dot, underscore, or hyphen",
            )
        }),
        ..ProductDto::default()
    };
    d.name = tri(m, "name", create, |v| {
        check_length_string(&mut e, "name", v, 1, 200)
    });
    d.description = tri(m, "description", false, |v| {
        check_max_string(&mut e, "description", v, 2000)
    });
    d.category_id = tri(m, "categoryId", create, |v| {
        check_uuid4(&mut e, "categoryId", v)
    });
    // Update: subCategoryId/brandId/principalId/taxRateId dideklarasikan ulang di subkelas.
    if create {
        d.sub_category_id = tri(m, "subCategoryId", false, |v| {
            check_uuid4(&mut e, "subCategoryId", v)
        });
        d.brand_id = tri(m, "brandId", false, |v| check_uuid4(&mut e, "brandId", v));
        d.principal_id = tri(m, "principalId", false, |v| {
            check_uuid4(&mut e, "principalId", v)
        });
    }
    d.base_uom_id = tri(m, "baseUomId", false, |v| {
        check_uuid4(&mut e, "baseUomId", v)
    });
    if create {
        d.tax_rate_id = tri(m, "taxRateId", false, |v| {
            check_uuid4(&mut e, "taxRateId", v)
        });
    }
    d.weight = tri(m, "weight", false, |v| {
        check_matches(
            &mut e,
            "weight",
            v,
            &WEIGHT_VOLUME,
            "weight must be a non-negative number",
        )
    });
    d.volume = tri(m, "volume", false, |v| {
        check_matches(
            &mut e,
            "volume",
            v,
            &WEIGHT_VOLUME,
            "volume must be a non-negative number",
        )
    });
    d.is_orderable = tri(m, "isOrderable", false, |v| {
        check_bool(&mut e, "isOrderable", v)
    });
    d.is_active = tri(m, "isActive", false, |v| check_bool(&mut e, "isActive", v));
    d.uoms = tri(m, "uoms", false, |v| {
        nested_array(&mut e, "uoms", v, parse_uom_input)
    });
    d.barcodes = tri(m, "barcodes", false, |v| {
        nested_array(&mut e, "barcodes", v, parse_barcode_input)
    });
    d.custom_fields = tri(m, "customFields", false, |v| match v {
        Some(Value::Object(o)) => Some(o.clone()),
        _ => {
            e.push("customFields must be an object".into());
            None
        }
    });
    if !create {
        d.sub_category_id = tri(m, "subCategoryId", false, |v| {
            check_uuid4(&mut e, "subCategoryId", v)
        });
        d.brand_id = tri(m, "brandId", false, |v| check_uuid4(&mut e, "brandId", v));
        d.principal_id = tri(m, "principalId", false, |v| {
            check_uuid4(&mut e, "principalId", v)
        });
        d.tax_rate_id = tri(m, "taxRateId", false, |v| {
            check_uuid4(&mut e, "taxRateId", v)
        });
    }
    if e.is_empty() { Ok(d) } else { Err(e) }
}

// ───────────────────────── service: write helpers ─────────────────────────

/// `resolveActiveMasterRef`: uuid → baris company, 404 / 422 archived.
pub async fn resolve_active_master_ref(
    conn: &mut PgConnection,
    company_id: i32,
    kind: MasterKind,
    uuid: &str,
) -> ApiResult<MasterRow> {
    let row = masters::find_by_uuid(&mut *conn, kind, company_id, row_uuid(uuid)?)
        .await?
        .ok_or_else(|| not_found("PRODUCT_MASTER_NOT_FOUND", "Product master row not found"))?;
    if !row.is_active {
        return Err(unprocessable(
            "PRODUCT_MASTER_ARCHIVED",
            "Cannot assign an archived product master row",
        ));
    }
    Ok(row)
}

fn assert_sub_category(sub: Option<&MasterRow>, category_id: i32) -> ApiResult<()> {
    if sub.is_some_and(|s| s.category_id != Some(category_id)) {
        return Err(unprocessable(
            "SUBCATEGORY_CATEGORY_MISMATCH",
            "subCategoryId does not belong to categoryId",
        ));
    }
    Ok(())
}

async fn resolve_default_base_uom(
    conn: &mut PgConnection,
    company_id: i32,
) -> ApiResult<MasterRow> {
    let row = masters::find_by_code(
        &mut *conn,
        MasterKind::Uoms,
        company_id,
        DEFAULT_BASE_UOM_CODE,
    )
    .await?
    .ok_or_else(|| {
        unprocessable(
            "DEFAULT_BASE_UOM_NOT_FOUND",
            format!("No {DEFAULT_BASE_UOM_CODE} unit of measure in this company — send baseUomId"),
        )
    })?;
    if !row.is_active {
        return Err(unprocessable(
            "PRODUCT_MASTER_ARCHIVED",
            format!("The {DEFAULT_BASE_UOM_CODE} unit of measure is archived — send baseUomId"),
        ));
    }
    Ok(row)
}

/// `ResolvedUom`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedUom {
    pub uom_id: i32,
    pub conversion_rate: String,
    pub operator: String,
    pub factor: String,
    pub is_base: bool,
    pub is_sales_uom: bool,
    pub is_purchase_uom: bool,
}

/// `resolveUomInputs` (products.service.ts:358).
async fn resolve_uom_inputs(
    conn: &mut PgConnection,
    company_id: i32,
    uoms: &[UomInput],
    base_uom_id: i32,
) -> ApiResult<Vec<ResolvedUom>> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for input in uoms {
        if !seen.insert(input.uom_id.clone()) {
            return Err(unprocessable(
                "PRODUCT_UOM_DUPLICATE",
                "The same uomId is listed more than once in uoms",
            ));
        }
        let row =
            resolve_active_master_ref(&mut *conn, company_id, MasterKind::Uoms, &input.uom_id)
                .await?;
        let is_base = input.is_base.unwrap_or(row.id == base_uom_id);
        let operator = input.operator.clone().unwrap_or_else(|| {
            if is_base {
                "multiply".into()
            } else {
                row.default_operator
                    .clone()
                    .unwrap_or_else(|| "multiply".into())
            }
        });
        let factor = input
            .factor
            .clone()
            .unwrap_or_else(|| match (&row.default_factor, is_base) {
                (Some(f), false) => dec_str(f),
                _ => "1".into(),
            });
        let conversion_rate = compute_base_equivalent(&operator, &factor).ok_or_else(|| {
            unprocessable(
                "PRODUCT_UOM_INVALID_FACTOR",
                invalid_factor_message(&operator, "factor"),
            )
        })?;
        out.push(ResolvedUom {
            uom_id: row.id,
            conversion_rate,
            operator,
            factor,
            is_base,
            is_sales_uom: input.is_sales_uom.unwrap_or(true),
            is_purchase_uom: input.is_purchase_uom.unwrap_or(false),
        });
    }
    if !out.iter().any(|u| u.uom_id == base_uom_id) {
        out.insert(
            0,
            ResolvedUom {
                uom_id: base_uom_id,
                conversion_rate: "1".into(),
                operator: "multiply".into(),
                factor: "1".into(),
                is_base: true,
                is_sales_uom: true,
                is_purchase_uom: false,
            },
        );
    }
    Ok(out)
}

/// `assertUomSetValid` (§0.26).
pub fn assert_uom_set_valid(uoms: &[ResolvedUom], base_uom_id: i32) -> ApiResult<()> {
    let bases: Vec<&ResolvedUom> = uoms.iter().filter(|u| u.is_base).collect();
    if bases.len() > 1 {
        return Err(unprocessable(
            "MULTIPLE_BASE_UOM",
            "Only one row in uoms can be isBase",
        ));
    }
    if !uoms.iter().any(|u| u.uom_id == base_uom_id) {
        return Err(unprocessable(
            "BASE_UOM_NOT_IN_LIST",
            "baseUomId must also appear in uoms",
        ));
    }
    let ok = bases.first().is_some_and(|b| {
        b.uom_id == base_uom_id
            && b.operator == "multiply"
            && b.factor.parse::<Decimal>().ok() == Some(Decimal::ONE)
    });
    if !ok {
        return Err(unprocessable(
            "BASE_UOM_MISMATCH",
            "The uoms row matching baseUomId must be isBase with operator \"multiply\" and factor \"1\"",
        ));
    }
    Ok(())
}

fn uom_rows_to_resolved(rows: &[ProductUomRow]) -> Vec<ResolvedUom> {
    rows.iter()
        .map(|u| ResolvedUom {
            uom_id: u.uom_id,
            conversion_rate: dec_str(&u.conversion_rate),
            operator: u.operator.clone(),
            factor: dec_str(&u.factor),
            is_base: u.is_base,
            is_sales_uom: u.is_sales_uom,
            is_purchase_uom: u.is_purchase_uom,
        })
        .collect()
}

async fn resolve_barcodes(
    conn: &mut PgConnection,
    company_id: i32,
    barcodes: &[BarcodeInput],
) -> ApiResult<Vec<(String, Option<i32>)>> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for b in barcodes {
        if !seen.insert(b.barcode.clone()) {
            return Err(unprocessable(
                "BARCODE_TAKEN",
                format!(
                    "Barcode {} is listed more than once in this request",
                    b.barcode
                ),
            ));
        }
        let uom_id = match b.uom_id.as_deref().filter(|u| !u.is_empty()) {
            Some(u) => Some(
                resolve_active_master_ref(&mut *conn, company_id, MasterKind::Uoms, u)
                    .await?
                    .id,
            ),
            None => None,
        };
        out.push((b.barcode.clone(), uom_id));
    }
    Ok(out)
}

async fn assert_barcodes_available(
    conn: &mut PgConnection,
    company_id: i32,
    barcodes: &[(String, Option<i32>)],
) -> ApiResult<()> {
    if barcodes.is_empty() {
        return Ok(());
    }
    let values: Vec<String> = barcodes.iter().map(|b| b.0.clone()).collect();
    let clash: Option<(String, i32)> = sqlx::query_as(
        r#"SELECT barcode, "productId" FROM "productBarcode"
           WHERE "companyId" = $1 AND barcode = ANY($2) ORDER BY id LIMIT 1"#,
    )
    .bind(company_id)
    .bind(&values)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((barcode, product_id)) = clash else {
        return Ok(());
    };
    let owner: Option<String> =
        sqlx::query_scalar(r#"SELECT sku FROM product WHERE "companyId" = $1 AND id = $2"#)
            .bind(company_id)
            .bind(product_id)
            .fetch_optional(&mut *conn)
            .await?;
    Err(barcode_taken(&barcode, owner.as_deref()))
}

async fn assert_sku_available(
    conn: &mut PgConnection,
    company_id: i32,
    sku: &str,
    exclude: Option<i32>,
) -> ApiResult<()> {
    let clash: Option<i32> = sqlx::query_scalar(
        r#"SELECT id FROM product WHERE "companyId" = $1 AND sku = $2 AND "deletedAt" IS NULL
           AND ($3::int IS NULL OR id <> $3) LIMIT 1"#,
    )
    .bind(company_id)
    .bind(sku)
    .bind(exclude)
    .fetch_optional(&mut *conn)
    .await?;
    if clash.is_some() {
        return Err(sku_taken(sku));
    }
    Ok(())
}

/// `assertCustomFieldsValid` (products.service.ts:549): 422 isu pertama + `details.issues`.
pub async fn assert_custom_fields_valid(
    conn: &mut PgConnection,
    company_id: i32,
    values: &Map<String, Value>,
) -> ApiResult<()> {
    let defs = definitions(&mut *conn, company_id).await?;
    let issues = validate_custom_field_values(CfEntity::Product, &defs, values);
    let Some(first) = issues.first() else {
        return Ok(());
    };
    Err(crate::common::coded_details(
        StatusCode::UNPROCESSABLE_ENTITY,
        &first.code,
        first.message.clone(),
        json!({ "issues": issues }),
    ))
}

/// `mergeCustomFields(existing, patch)`: `null` menghapus kunci, kunci prototipe dibuang.
pub fn merge_custom_fields(existing: &Value, patch: &Map<String, Value>) -> Map<String, Value> {
    let mut out = existing.as_object().cloned().unwrap_or_default();
    for key in js_keys(patch) {
        if matches!(key.as_str(), "__proto__" | "constructor" | "prototype") {
            continue;
        }
        match patch.get(key) {
            Some(Value::Null) => {
                out.remove(key);
            }
            Some(v) => {
                out.insert(key.clone(), v.clone());
            }
            None => {}
        }
    }
    out
}

async fn insert_uoms(
    conn: &mut PgConnection,
    product_id: i32,
    uoms: &[ResolvedUom],
) -> ApiResult<()> {
    if uoms.is_empty() {
        return Ok(());
    }
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
        r#"INSERT INTO "productUom" ("productId", "uomId", "conversionRate", operator, factor, "isBase", "isSalesUom", "isPurchaseUom") "#,
    );
    qb.push_values(uoms, |mut b, u| {
        b.push_bind(product_id)
            .push_bind(u.uom_id)
            .push_bind(u.conversion_rate.clone())
            .push_unseparated("::numeric")
            .push_bind(u.operator.clone())
            .push_unseparated(r#"::"UomOperator""#)
            .push_bind(u.factor.clone())
            .push_unseparated("::numeric")
            .push_bind(u.is_base)
            .push_bind(u.is_sales_uom)
            .push_bind(u.is_purchase_uom);
    });
    qb.build().execute(&mut *conn).await?;
    Ok(())
}

async fn insert_barcodes(
    conn: &mut PgConnection,
    company_id: i32,
    product_id: i32,
    barcodes: &[(String, Option<i32>)],
) -> ApiResult<()> {
    if barcodes.is_empty() {
        return Ok(());
    }
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
        r#"INSERT INTO "productBarcode" ("companyId", "productId", "uomId", barcode) "#,
    );
    qb.push_values(barcodes, |mut b, (code, uom)| {
        b.push_bind(company_id)
            .push_bind(product_id)
            .push_bind(*uom)
            .push_bind(code.clone());
    });
    match qb.build().execute(&mut *conn).await {
        Ok(_) => Ok(()),
        Err(e) if mz_core::db::is_unique_violation(&e) => Err(barcode_taken(
            barcodes.first().map(|b| b.0.as_str()).unwrap_or_default(),
            None,
        )),
        Err(e) => Err(e.into()),
    }
}

async fn row_by_id(conn: &mut PgConnection, id: i32) -> ApiResult<ProductRow> {
    Ok(
        sqlx::query_as(&format!("SELECT {PRODUCT_COLS} FROM product WHERE id = $1"))
            .bind(id)
            .fetch_one(&mut *conn)
            .await?,
    )
}

// ───────────────────────── service: create/update/remove ─────────────────────────

/// Node: src/products/products.service.ts:1046 `create`
pub async fn create(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    dto: &ProductDto,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let (Field::Val(sku), Field::Val(name), Field::Val(category_uuid)) =
        (&dto.sku, &dto.name, &dto.category_id)
    else {
        return Err(internal("sku/name/categoryId wajib"));
    };
    let mut tx = state.db.begin().await?;
    let category =
        resolve_active_master_ref(&mut tx, company_id, MasterKind::Categories, category_uuid)
            .await?;
    let sub = match &dto.sub_category_id {
        Field::Val(u) if !u.is_empty() => Some(
            resolve_active_master_ref(&mut tx, company_id, MasterKind::SubCategories, u).await?,
        ),
        _ => None,
    };
    assert_sub_category(sub.as_ref(), category.id)?;
    let brand = match &dto.brand_id {
        Field::Val(u) if !u.is_empty() => {
            Some(resolve_active_master_ref(&mut tx, company_id, MasterKind::Brands, u).await?)
        }
        _ => None,
    };
    let principal = match &dto.principal_id {
        Field::Val(u) if !u.is_empty() => {
            Some(resolve_active_master_ref(&mut tx, company_id, MasterKind::Principals, u).await?)
        }
        _ => None,
    };
    let base_uom = match &dto.base_uom_id {
        Field::Val(u) if !u.is_empty() => {
            resolve_active_master_ref(&mut tx, company_id, MasterKind::Uoms, u).await?
        }
        _ => resolve_default_base_uom(&mut tx, company_id).await?,
    };
    let tax = match &dto.tax_rate_id {
        Field::Val(u) if !u.is_empty() => {
            Some(resolve_active_master_ref(&mut tx, company_id, MasterKind::Taxes, u).await?)
        }
        _ => None,
    };
    let uom_inputs: &[UomInput] = match &dto.uoms {
        Field::Val(v) => v,
        _ => &[],
    };
    let uoms = resolve_uom_inputs(&mut tx, company_id, uom_inputs, base_uom.id).await?;
    assert_uom_set_valid(&uoms, base_uom.id)?;
    let barcode_inputs: &[BarcodeInput] = match &dto.barcodes {
        Field::Val(v) => v,
        _ => &[],
    };
    let barcodes = resolve_barcodes(&mut tx, company_id, barcode_inputs).await?;
    assert_barcodes_available(&mut tx, company_id, &barcodes).await?;
    assert_sku_available(&mut tx, company_id, sku, None).await?;
    let custom_fields = match &dto.custom_fields {
        Field::Val(m) => m.clone(),
        _ => Map::new(),
    };
    assert_custom_fields_valid(&mut tx, company_id, &custom_fields).await?;

    let text = |f: &Field<String>| match f {
        Field::Val(s) => Some(s.clone()),
        _ => None,
    };
    let inserted = sqlx::query_scalar::<_, i32>(
        r#"INSERT INTO product ("companyId", sku, name, description, "categoryId", "subCategoryId",
             "brandId", "principalId", "baseUomId", "taxRateId", weight, volume, "isOrderable", "isActive",
             "customFields", "createdById")
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11::numeric, $12::numeric, $13, $14, $15, $16)
           RETURNING id"#,
    )
    .bind(company_id)
    .bind(sku)
    .bind(name)
    .bind(text(&dto.description))
    .bind(category.id)
    .bind(sub.as_ref().map(|r| r.id))
    .bind(brand.as_ref().map(|r| r.id))
    .bind(principal.as_ref().map(|r| r.id))
    .bind(base_uom.id)
    .bind(tax.as_ref().map(|r| r.id))
    .bind(text(&dto.weight))
    .bind(text(&dto.volume))
    .bind(!matches!(dto.is_orderable, Field::Val(false)))
    .bind(!matches!(dto.is_active, Field::Val(false)))
    .bind(Value::Object(custom_fields))
    .bind(user_id)
    .fetch_one(&mut *tx)
    .await;
    let id = match inserted {
        Ok(id) => id,
        Err(e) if mz_core::db::is_unique_violation(&e) => return Err(sku_taken(sku)),
        Err(e) => return Err(e.into()),
    };
    if let Some(r) = audit
        .result_row(&mut *tx, Model::Product, Lookup::Id(id))
        .await
    {
        audit.record_create(Model::Product, &r).await;
    }
    insert_uoms(&mut tx, id, &uoms).await?;
    insert_barcodes(&mut tx, company_id, id, &barcodes).await?;
    let row = row_by_id(&mut tx, id).await?;
    let entity = assemble_entity(state, &mut tx, company_id, &row, true).await?;
    tx.commit().await?;
    Ok(entity)
}

/// Node: src/products/products.service.ts:1206 `update` — read-merge-validate-write satu transaksi.
pub async fn update(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    uuid: &str,
    dto: &ProductDto,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let mut tx = state.db.begin().await?;
    let current = find_live_by_uuid(&mut *tx, company_id, row_uuid(uuid)?)
        .await?
        .ok_or_else(product_not_found)?;

    let category = match &dto.category_id {
        Field::Val(u) => {
            Some(resolve_active_master_ref(&mut tx, company_id, MasterKind::Categories, u).await?)
        }
        Field::Null => return Err(null_500("categoryId")),
        Field::Absent => None,
    };
    let final_category_id = category.as_ref().map_or(current.category_id, |c| c.id);
    let sub = match &dto.sub_category_id {
        Field::Val(u) => Some(
            resolve_active_master_ref(&mut tx, company_id, MasterKind::SubCategories, u).await?,
        ),
        _ => None,
    };
    let final_sub_id = match &dto.sub_category_id {
        Field::Absent => current.sub_category_id,
        Field::Null => None,
        Field::Val(_) => sub.as_ref().map(|s| s.id),
    };
    if dto.category_id.is_defined() || dto.sub_category_id.is_defined() {
        let effective = match (&sub, final_sub_id) {
            (Some(s), _) => Some(s.clone()),
            (None, Some(id)) => {
                masters::find_by_id(&mut *tx, MasterKind::SubCategories, company_id, id).await?
            }
            (None, None) => None,
        };
        assert_sub_category(effective.as_ref(), final_category_id)?;
    }
    let mut resolve_opt =
        async |f: &Field<String>, kind: MasterKind, cur: Option<i32>| -> ApiResult<Option<i32>> {
            Ok(match f {
                Field::Absent => cur,
                Field::Null => None,
                Field::Val(u) => Some(
                    resolve_active_master_ref(&mut tx, company_id, kind, u)
                        .await?
                        .id,
                ),
            })
        };
    let final_brand = resolve_opt(&dto.brand_id, MasterKind::Brands, current.brand_id).await?;
    let final_principal = resolve_opt(
        &dto.principal_id,
        MasterKind::Principals,
        current.principal_id,
    )
    .await?;
    let base_uom = match &dto.base_uom_id {
        Field::Val(u) => {
            Some(resolve_active_master_ref(&mut tx, company_id, MasterKind::Uoms, u).await?)
        }
        Field::Null => return Err(null_500("baseUomId")),
        Field::Absent => None,
    };
    let final_base = base_uom.as_ref().map_or(current.base_uom_id, |b| b.id);
    let final_tax = match &dto.tax_rate_id {
        Field::Absent => current.tax_rate_id,
        Field::Null => None,
        Field::Val(u) => Some(
            resolve_active_master_ref(&mut tx, company_id, MasterKind::Taxes, u)
                .await?
                .id,
        ),
    };

    let resolved_uoms = match &dto.uoms {
        Field::Val(v) => Some(resolve_uom_inputs(&mut tx, company_id, v, final_base).await?),
        Field::Null => return Err(null_500("uoms")),
        Field::Absent => None,
    };
    if dto.base_uom_id.is_defined() || dto.uoms.is_defined() {
        let for_validation = match &resolved_uoms {
            Some(v) => v.clone(),
            None => uom_rows_to_resolved(&uoms_of(&mut *tx, &[current.id]).await?),
        };
        assert_uom_set_valid(&for_validation, final_base)?;
    }
    if let Some(resolved) = &resolved_uoms {
        let existing = uoms_of(&mut *tx, &[current.id]).await?;
        let keep: HashSet<i32> = resolved.iter().map(|u| u.uom_id).collect();
        let removed: Vec<i32> = existing
            .iter()
            .map(|u| u.uom_id)
            .filter(|id| !keep.contains(id))
            .collect();
        if !removed.is_empty() {
            let count: i64 = sqlx::query_scalar(
                r#"SELECT COUNT(*) FROM "priceListItem"
                   WHERE "productId" = $1 AND "uomId" = ANY($2) AND "deletedAt" IS NULL"#,
            )
            .bind(current.id)
            .bind(&removed)
            .fetch_one(&mut *tx)
            .await?;
            if count > 0 {
                return Err(unprocessable(
                    "UOM_HAS_PRICES",
                    format!(
                        "Cannot remove this UOM: {count} active price list item(s) still use it"
                    ),
                ));
            }
        }
    }
    let resolved_barcodes = match &dto.barcodes {
        Field::Val(v) => Some(resolve_barcodes(&mut tx, company_id, v).await?),
        Field::Null => return Err(null_500("barcodes")),
        Field::Absent => None,
    };
    match &dto.sku {
        Field::Val(s) if *s != current.sku => {
            assert_sku_available(&mut tx, company_id, s, Some(current.id)).await?
        }
        Field::Null => return Err(null_500("sku")),
        _ => {}
    }
    let merged = match &dto.custom_fields {
        Field::Val(patch) => {
            let merged = merge_custom_fields(&current.custom_fields, patch);
            assert_custom_fields_valid(&mut tx, company_id, &merged).await?;
            Some(merged)
        }
        Field::Null => return Err(null_500("customFields")),
        Field::Absent => None,
    };

    // UPDATE product
    let pre = audit.pre_read(Model::Product, Lookup::Id(current.id)).await;
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new("UPDATE product SET ");
    let set_text =
        |qb: &mut QueryBuilder<'_, Postgres>, col: &str, f: &Field<String>, cast: &str| match f {
            Field::Val(s) => {
                qb.push(format!(r#""{col}" = "#))
                    .push_bind(s.clone())
                    .push(cast)
                    .push(", ");
            }
            Field::Null => {
                qb.push(format!(r#""{col}" = NULL, "#));
            }
            Field::Absent => {}
        };
    set_text(&mut qb, "sku", &dto.sku, "");
    set_text(&mut qb, "name", &dto.name, "");
    set_text(&mut qb, "description", &dto.description, "");
    let set_id = |qb: &mut QueryBuilder<'_, Postgres>, col: &str, defined: bool, v: Option<i32>| {
        if defined {
            qb.push(format!(r#""{col}" = "#)).push_bind(v).push(", ");
        }
    };
    set_id(
        &mut qb,
        "categoryId",
        dto.category_id.is_defined(),
        Some(final_category_id),
    );
    set_id(
        &mut qb,
        "subCategoryId",
        dto.sub_category_id.is_defined(),
        final_sub_id,
    );
    set_id(&mut qb, "brandId", dto.brand_id.is_defined(), final_brand);
    set_id(
        &mut qb,
        "principalId",
        dto.principal_id.is_defined(),
        final_principal,
    );
    set_id(
        &mut qb,
        "baseUomId",
        dto.base_uom_id.is_defined(),
        Some(final_base),
    );
    set_id(
        &mut qb,
        "taxRateId",
        dto.tax_rate_id.is_defined(),
        final_tax,
    );
    set_text(&mut qb, "weight", &dto.weight, "::numeric");
    set_text(&mut qb, "volume", &dto.volume, "::numeric");
    for (col, f) in [
        ("isOrderable", &dto.is_orderable),
        ("isActive", &dto.is_active),
    ] {
        match f {
            Field::Val(b) => {
                qb.push(format!(r#""{col}" = "#)).push_bind(*b).push(", ");
            }
            Field::Null => return Err(null_500(col)),
            Field::Absent => {}
        }
    }
    if let Some(m) = &merged {
        qb.push(r#""customFields" = "#)
            .push_bind(Value::Object(m.clone()))
            .push(", ");
    }
    qb.push(r#""updatedAt" = "#)
        .push_bind(now_naive())
        .push(" WHERE id = ")
        .push_bind(current.id);
    match qb.build().execute(&mut *tx).await {
        Ok(_) => {}
        Err(e) if mz_core::db::is_unique_violation(&e) => {
            let sku = match &dto.sku {
                Field::Val(s) => s.clone(),
                _ => current.sku.clone(),
            };
            return Err(sku_taken(&sku));
        }
        Err(e) => return Err(e.into()),
    }
    if let Some(r) = audit
        .result_row(&mut *tx, Model::Product, Lookup::Id(current.id))
        .await
    {
        audit.record_update(Model::Product, pre, &r).await;
    }
    if let Some(uoms) = &resolved_uoms {
        sqlx::query(r#"DELETE FROM "productUom" WHERE "productId" = $1"#)
            .bind(current.id)
            .execute(&mut *tx)
            .await?;
        insert_uoms(&mut tx, current.id, uoms).await?;
    }
    if let Some(barcodes) = &resolved_barcodes {
        sqlx::query(r#"DELETE FROM "productBarcode" WHERE "productId" = $1"#)
            .bind(current.id)
            .execute(&mut *tx)
            .await?;
        assert_barcodes_available(&mut tx, company_id, barcodes).await?;
        insert_barcodes(&mut tx, company_id, current.id, barcodes).await?;
    }
    let row = row_by_id(&mut tx, current.id).await?;
    let entity = assemble_entity(state, &mut tx, company_id, &row, true).await?;
    tx.commit().await?;
    Ok(entity)
}

/// Node: src/products/products.service.ts:1418 `remove` — soft delete + cascade harga + lepas barcode.
pub async fn remove(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    uuid: &str,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let mut tx = state.db.begin().await?;
    let current = find_live_by_uuid(&mut *tx, company_id, row_uuid(uuid)?)
        .await?
        .ok_or_else(product_not_found)?;
    let now = now_naive();
    let pid = current.id;
    let lookup = Lookup::filter(move |qb| {
        qb.push(r#""productId" = "#)
            .push_bind(pid)
            .push(r#" AND "deletedAt" IS NULL"#);
    });
    let pre = audit.pre_read_many(Model::PriceListItem, lookup).await;
    let items = sqlx::query(
        r#"UPDATE "priceListItem" SET "deletedAt" = $2, "updatedAt" = $2
           WHERE "productId" = $1 AND "deletedAt" IS NULL"#,
    )
    .bind(current.id)
    .bind(now)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    audit
        .record_update_many(
            Model::PriceListItem,
            pre,
            items,
            &Row::new()
                .with("deletedAt", json!(mz_core::time::JsDate(now)))
                .with("updatedAt", json!(mz_core::time::JsDate(now))),
            json!({ "productId": current.id, "deletedAt": null }),
        )
        .await;
    let barcodes = sqlx::query(r#"DELETE FROM "productBarcode" WHERE "productId" = $1"#)
        .bind(current.id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    let pre = audit.pre_read(Model::Product, Lookup::Id(current.id)).await;
    sqlx::query(r#"UPDATE product SET "deletedAt" = $2, "isActive" = false, "updatedAt" = $2 WHERE id = $1"#)
        .bind(current.id)
        .bind(now)
        .execute(&mut *tx)
        .await?;
    if let Some(r) = audit
        .result_row(&mut *tx, Model::Product, Lookup::Id(current.id))
        .await
    {
        audit.record_update(Model::Product, pre, &r).await;
    }
    tx.commit().await?;
    Ok(json!({
        "id": current.uuid,
        "deleted": true,
        "priceItemsDeleted": items,
        "barcodesReleased": barcodes,
    }))
}

// ───────────────────────── handler ─────────────────────────

async fn censored(state: &AppState, ctx: &RequestCtx) -> ApiResult<Arc<CensoredColumns>> {
    let table = mz_table_fields::registry::table_by_key(TABLE_KEY)
        .ok_or_else(|| internal("tabel products tidak terdaftar"))?;
    mz_table_fields::policy::censored_columns(state, ctx, table).await
}

/// `@TableFields('products', { path: 'data' })`.
async fn with_policy(state: &AppState, ctx: &RequestCtx, mut body: Value) -> ApiResult<Value> {
    let c = censored(state, ctx).await?;
    mz_table_fields::apply::censor_response_body(&mut body, TABLE_KEY, "data", &c, None);
    Ok(body)
}

/// Node: src/products/admin-products.controller.ts:168 `findAll`
async fn h_find_all(
    State(state): State<AppState>,
    ctx: RequestCtx,
    RawQuery(raw): RawQuery,
) -> ApiResult<Json<Value>> {
    let q = parse_list(raw.as_deref().unwrap_or_default(), false).map_err(ApiError::Validation)?;
    let c = censored(&state, &ctx).await?;
    let mut body = find_all(&state, ctx.require_user_id()?, &q, &c).await?;
    mz_table_fields::apply::censor_response_body(&mut body, TABLE_KEY, "data", &c, None);
    Ok(Json(body))
}

/// Node: src/products/admin-products.controller.ts:204 `customFieldSummary`
async fn h_summary(
    State(state): State<AppState>,
    ctx: RequestCtx,
    RawQuery(raw): RawQuery,
) -> ApiResult<Json<Value>> {
    let parsed = qs::parse(raw.as_deref().unwrap_or_default());
    let mut e = Vec::new();
    let key = check_matches(
        &mut e,
        "key",
        qs::get(&parsed, "key").map(qv_to_json).as_ref(),
        &CUSTOM_FIELD_KEY,
        "key must match /^[a-z][a-z0-9_]{0,39}$/ regular expression",
    );
    let Some(key) = key.filter(|_| e.is_empty()) else {
        return Err(ApiError::Validation(e));
    };
    let data = custom_field_summary(&state, ctx.require_user_id()?, &key).await?;
    Ok(Json(json!({ "data": data })))
}

/// Node: src/products/admin-products.controller.ts:232 `create`
async fn h_create(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    ValidatedJson(body): ValidatedJson<Raw>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let dto = parse_product(&body.0, true).map_err(ApiError::Validation)?;
    let data = create(&state, &audit, ctx.require_user_id()?, &dto).await?;
    let body = with_policy(&state, &ctx, json!({ "data": data })).await?;
    Ok((StatusCode::CREATED, Json(body)))
}

/// Node: src/products/admin-products.controller.ts:337 `export`
async fn h_export(
    State(state): State<AppState>,
    ctx: RequestCtx,
    RawQuery(raw): RawQuery,
) -> ApiResult<Response> {
    let q = parse_list(raw.as_deref().unwrap_or_default(), true).map_err(ApiError::Validation)?;
    let c = censored(&state, &ctx).await?;
    let buf = export_products(&state, ctx.require_user_id()?, &q, &c).await?;
    Ok(xlsx_response(buf, "products-export.xlsx"))
}

/// Respons xlsx (`Content-Type`, `Content-Disposition` attachment, `Content-Length`).
pub fn xlsx_response(buf: Vec<u8>, filename: &str) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        ),
    );
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\"")) {
        headers.insert(header::CONTENT_DISPOSITION, v);
    }
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(buf.len()));
    (StatusCode::OK, headers, buf).into_response()
}

/// Node: src/products/admin-products.controller.ts:369 `findOne`
async fn h_find_one(
    State(state): State<AppState>,
    ctx: RequestCtx,
    SegmentId(id): SegmentId,
) -> ApiResult<Json<Value>> {
    let company_id = require_company_id(&state.db, ctx.require_user_id()?).await?;
    let row = find_live_by_uuid(&state.db, company_id, row_uuid(&id)?)
        .await?
        .ok_or_else(product_not_found)?;
    let mut conn = state.db.acquire().await?;
    let data = assemble_entity(&state, &mut conn, company_id, &row, true).await?;
    drop(conn);
    Ok(Json(
        with_policy(&state, &ctx, json!({ "data": data })).await?,
    ))
}

/// Node: src/products/admin-products.controller.ts:381 `update`
async fn h_update(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    SegmentId(id): SegmentId,
    ValidatedJson(body): ValidatedJson<Raw>,
) -> ApiResult<Json<Value>> {
    let dto = parse_product(&body.0, false).map_err(ApiError::Validation)?;
    let data = update(&state, &audit, ctx.require_user_id()?, &id, &dto).await?;
    Ok(Json(
        with_policy(&state, &ctx, json!({ "data": data })).await?,
    ))
}

/// Node: src/products/admin-products.controller.ts:406 `remove`
async fn h_remove(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    SegmentId(id): SegmentId,
) -> ApiResult<Json<Value>> {
    let data = remove(&state, &audit, ctx.require_user_id()?, &id).await?;
    Ok(Json(json!({ "data": data })))
}

/// `@Param('id')` — atau, untuk path statis yang dipanggil dengan method milik `:id`
/// (Express mencocokkan `:id` dengan kata `export`/`imports`/...), segmen terakhir path.
pub struct SegmentId(pub String);

impl axum::extract::FromRequestParts<AppState> for SegmentId {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &AppState,
    ) -> Result<Self, ApiError> {
        if let Ok(Path(id)) = Path::<String>::from_request_parts(parts, state).await {
            return Ok(Self(id));
        }
        let last = parts.uri.path().rsplit('/').next().unwrap_or_default();
        Ok(Self(last.to_owned()))
    }
}

/// 7 route `/v1/admin/products` (tanpa gambar & import) + method `:id` pada path statis
/// (`GET import`, `PATCH|DELETE custom-field-summary|export|import|imports`) yang di Node jatuh
/// ke `findOne`/`update`/`remove` dengan id bukan UUID (→ 500).
pub fn routes(state: &AppState) -> Router<AppState> {
    let s = crate::PRODUCTS;
    let mut r = Router::new();
    for p in ["custom-field-summary", "export", "import", "imports"] {
        let path = format!("/v1/admin/products/{p}");
        r = r
            .route(&path, guard(state, s, patch(h_update)))
            .route(&path, guard(state, s, delete(h_remove)));
    }
    r.route(
        "/v1/admin/products/import",
        guard(state, s, get(h_find_one)),
    )
    .route("/v1/admin/products", guard(state, s, get(h_find_all)))
    .route("/v1/admin/products", guard(state, s, post(h_create)))
    .route(
        "/v1/admin/products/custom-field-summary",
        guard(state, s, get(h_summary)),
    )
    .route("/v1/admin/products/export", guard(state, s, get(h_export)))
    .route("/v1/admin/products/{id}", guard(state, s, get(h_find_one)))
    .route("/v1/admin/products/{id}", guard(state, s, patch(h_update)))
    .route("/v1/admin/products/{id}", guard(state, s, delete(h_remove)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_list() {
        let q = parse_list(
            "categoryId=a,b&sortBy=cf:shelf&cf%5Bshelf%5D=12&cf[x][y]=1",
            false,
        )
        .unwrap_err();
        assert_eq!(q, vec!["each value in categoryId must be a UUID"]);
        let q = parse_list(
            "sortBy=cf:shelf&cf%5Bshelf%5D=12&cf[x][y]=1&sortDir=desc&page=2",
            false,
        )
        .unwrap();
        assert_eq!(
            q.cf,
            vec![
                ("shelf".to_owned(), Some("12".to_owned())),
                ("x".to_owned(), None)
            ]
        );
        assert!(q.sort_desc);
        assert_eq!(q.page, 2);
        assert_eq!(
            parse_list("cf=1", false).unwrap_err(),
            vec!["cf must be an object"]
        );
        assert_eq!(
            parse_list("sortBy=cf:a", true).unwrap_err(),
            vec!["sortBy must be one of the following values: sku, name, createdAt, updatedAt"]
        );
        assert_eq!(
            parse_list("sortBy=price", false).unwrap_err(),
            vec![
                "sortBy must match /^(sku|name|createdAt|updatedAt|cf:[a-z][a-z0-9_]{0,39})$/ regular expression"
            ]
        );
    }

    #[test]
    fn body_nested() {
        let m = json!({"sku": "A-1", "name": "x", "categoryId": "bad",
            "uoms": [{"uomId": "x", "conversionRate": "12"}], "barcodes": "x"});
        let e = parse_product(m.as_object().unwrap(), true).unwrap_err();
        assert_eq!(
            e,
            vec![
                "categoryId must be a UUID",
                "uoms.0.uomId must be a UUID",
                "uoms.0.conversionRate is no longer accepted — send operator and factor instead",
                "barcodes must be an array",
                "nested property barcodes must be either object or array",
            ]
        );
    }

    #[test]
    fn uom_set() {
        let base = ResolvedUom {
            uom_id: 1,
            conversion_rate: "1".into(),
            operator: "multiply".into(),
            factor: "1".into(),
            is_base: true,
            is_sales_uom: true,
            is_purchase_uom: false,
        };
        let mut box_ = base.clone();
        box_.uom_id = 2;
        box_.is_base = false;
        box_.factor = "24".into();
        assert!(assert_uom_set_valid(&[base.clone(), box_.clone()], 1).is_ok());
        assert!(assert_uom_set_valid(&[base.clone(), box_.clone()], 3).is_err());
        let mut two = box_.clone();
        two.is_base = true;
        assert!(assert_uom_set_valid(&[base.clone(), two], 1).is_err());
        let mut bad = base;
        bad.factor = "2".into();
        assert!(assert_uom_set_valid(&[bad], 1).is_err());
    }

    #[test]
    fn merge_cf() {
        let merged = merge_custom_fields(
            &json!({"a": 1, "b": 2}),
            json!({"b": null, "c": 3, "__proto__": 1})
                .as_object()
                .unwrap(),
        );
        assert_eq!(Value::Object(merged), json!({"a": 1, "c": 3}));
    }
}
