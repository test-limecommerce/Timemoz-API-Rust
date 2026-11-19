//! Port `products/admin-price-lists.controller.ts` + `products/price-lists.service.ts`:
//! `/v1/admin/price-lists` (11 route) — price list CRUD, item tier (bulk upsert), dan assignment
//! customer (§0.27). Fungsi `pub` di bawah juga dipakai modul order (S3): baca price list aktif,
//! tier harga per (produk, uom), serializer item.

use std::collections::{HashMap, HashSet};

use axum::extract::{Path, State};
use axum::routing::{delete, get, patch, post, put};
use axum::{Json, Router, http::StatusCode};
use chrono::{NaiveDate, NaiveDateTime, Utc};
use mz_audit::{AuditHandle, Lookup, Model, Row};
use mz_core::company::require_company_id;
use mz_core::extract::{ValidatedJson, ValidatedQuery};
use mz_core::gate::guard;
use mz_core::time::{JsDate, now_naive};
use mz_core::{ApiError, ApiResult, AppState, RequestCtx};
use rust_decimal::Decimal;
use serde_json::{Map, Value, json};
use sqlx::{PgExecutor, Postgres, QueryBuilder};

use crate::PRICE_LISTS;
use crate::common::{
    bad_request, conflict, dec_opt, dec_str, internal, like_pattern, ms, ms_opt, not_found, offset,
    page_meta, row_uuid, unique_constraint, unprocessable,
};
use crate::dto::{
    CURRENCY_CODE, DATE_ONLY, Field, MASTER_CODE, PRICE, QTY, Raw, check_bool, check_length_string,
    check_matches, check_uuid4, opt_max_string, page_limit_max, present, query_bool,
};
use crate::masters::{self, MasterKind, MasterRow};
use crate::util::{
    QtyRange, compare_decimal_strings, is_price_list_active_today, qty_ranges_overlap,
};

/// `MAX_PRICE_LIST_ITEMS_PER_REQUEST` (upsert-price-list-items.dto.ts:4).
pub const MAX_PRICE_LIST_ITEMS_PER_REQUEST: usize = 1000;

// ───────────────────────── baris ─────────────────────────

const PL_COLS: &str = r#"id, uuid::text AS uuid, "companyId", code, name, "currencyCode", "startDate", "endDate", "isDefault", "isActive", "createdById", "deletedAt", "createdAt", "updatedAt""#;

/// Baris `priceList` (product.delegate.ts:300 `PriceListRow`).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PriceListRow {
    pub id: i32,
    pub uuid: String,
    #[sqlx(rename = "companyId")]
    pub company_id: i32,
    pub code: String,
    pub name: String,
    #[sqlx(rename = "currencyCode")]
    pub currency_code: String,
    #[sqlx(rename = "startDate")]
    pub start_date: Option<NaiveDate>,
    #[sqlx(rename = "endDate")]
    pub end_date: Option<NaiveDate>,
    #[sqlx(rename = "isDefault")]
    pub is_default: bool,
    #[sqlx(rename = "isActive")]
    pub is_active: bool,
    #[sqlx(rename = "createdById")]
    pub created_by_id: i32,
    #[sqlx(rename = "deletedAt")]
    pub deleted_at: Option<NaiveDateTime>,
    #[sqlx(rename = "createdAt")]
    pub created_at: NaiveDateTime,
    #[sqlx(rename = "updatedAt")]
    pub updated_at: Option<NaiveDateTime>,
}

impl PriceListRow {
    /// `isPriceListActiveToday(row, timeZone)` (price-list-active.util.ts:15) per sekarang.
    pub fn is_active_today(&self, time_zone: &str) -> bool {
        is_price_list_active_today(
            self.is_active,
            self.start_date,
            self.end_date,
            time_zone,
            Utc::now(),
        )
    }
}

const ITEM_COLS: &str = r#"id, uuid::text AS uuid, "companyId", "priceListId", "productId", "uomId", "minQty", "maxQty", price, "deletedAt", "createdAt", "updatedAt""#;

/// Baris `priceListItem` (product.delegate.ts:226 `PriceListItemRow`).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PriceListItemRow {
    pub id: i32,
    pub uuid: String,
    #[sqlx(rename = "companyId")]
    pub company_id: i32,
    #[sqlx(rename = "priceListId")]
    pub price_list_id: i32,
    #[sqlx(rename = "productId")]
    pub product_id: i32,
    #[sqlx(rename = "uomId")]
    pub uom_id: i32,
    #[sqlx(rename = "minQty")]
    pub min_qty: Decimal,
    #[sqlx(rename = "maxQty")]
    pub max_qty: Option<Decimal>,
    pub price: Decimal,
    #[sqlx(rename = "deletedAt")]
    pub deleted_at: Option<NaiveDateTime>,
    #[sqlx(rename = "createdAt")]
    pub created_at: NaiveDateTime,
    #[sqlx(rename = "updatedAt")]
    pub updated_at: Option<NaiveDateTime>,
}

impl PriceListItemRow {
    /// `QtyRange` dari kolom desimal (`String(row.minQty)`).
    pub fn range(&self) -> QtyRange {
        QtyRange {
            min: dec_str(&self.min_qty),
            max: self.max_qty.as_ref().map(dec_str),
        }
    }
}

/// Ringkas produk untuk ref item (`{ uuid, sku, name }`).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ProductRef {
    pub id: i32,
    pub uuid: String,
    pub sku: String,
    pub name: String,
}

// ───────────────────────── pembacaan pub (S3) ─────────────────────────

/// `findFirst({ where: { uuid, companyId, deletedAt: null } })` (price-lists.service.ts:265).
pub async fn find_by_uuid<'e>(
    exec: impl PgExecutor<'e>,
    company_id: i32,
    uuid: uuid::Uuid,
) -> ApiResult<Option<PriceListRow>> {
    Ok(sqlx::query_as(&format!(
        r#"SELECT {PL_COLS} FROM "priceList" WHERE uuid = $1 AND "companyId" = $2 AND "deletedAt" IS NULL LIMIT 1"#
    ))
    .bind(uuid)
    .bind(company_id)
    .fetch_optional(exec)
    .await?)
}

/// Price list hidup (belum dihapus) berdasarkan id internal.
pub async fn find_by_id<'e>(
    exec: impl PgExecutor<'e>,
    company_id: i32,
    id: i32,
) -> ApiResult<Option<PriceListRow>> {
    Ok(sqlx::query_as(&format!(
        r#"SELECT {PL_COLS} FROM "priceList" WHERE id = $1 AND "companyId" = $2 AND "deletedAt" IS NULL LIMIT 1"#
    ))
    .bind(id)
    .bind(company_id)
    .fetch_optional(exec)
    .await?)
}

/// Price list default company (§0.27 fallback; partial unique: paling banyak satu).
pub async fn find_default<'e>(
    exec: impl PgExecutor<'e>,
    company_id: i32,
) -> ApiResult<Option<PriceListRow>> {
    Ok(sqlx::query_as(&format!(
        r#"SELECT {PL_COLS} FROM "priceList" WHERE "companyId" = $1 AND "isDefault" AND "deletedAt" IS NULL ORDER BY id LIMIT 1"#
    ))
    .bind(company_id)
    .fetch_optional(exec)
    .await?)
}

/// Tier aktif satu (price list, produk, uom), urut `minQty` (algoritma resolusi §7.5 langkah 4).
pub async fn items_for_product_uom<'e>(
    exec: impl PgExecutor<'e>,
    price_list_id: i32,
    product_id: i32,
    uom_id: i32,
) -> ApiResult<Vec<PriceListItemRow>> {
    Ok(sqlx::query_as(&format!(
        r#"SELECT {ITEM_COLS} FROM "priceListItem"
           WHERE "priceListId" = $1 AND "productId" = $2 AND "uomId" = $3 AND "deletedAt" IS NULL
           ORDER BY "minQty" ASC, id ASC"#
    ))
    .bind(price_list_id)
    .bind(product_id)
    .bind(uom_id)
    .fetch_all(exec)
    .await?)
}

/// Tier yang cocok untuk `qty` (`minQty <= qty <= maxQty|∞`), atau `None`.
pub fn pick_tier<'a>(items: &'a [PriceListItemRow], qty: &Decimal) -> Option<&'a PriceListItemRow> {
    items
        .iter()
        .find(|i| i.min_qty <= *qty && i.max_qty.is_none_or(|m| *qty <= m))
}

async fn products_by<'e>(
    exec: impl PgExecutor<'e>,
    company_id: i32,
    ids: &[i32],
) -> ApiResult<Vec<ProductRef>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    Ok(sqlx::query_as(
        r#"SELECT id, uuid::text AS uuid, sku, name FROM product WHERE "companyId" = $1 AND id = ANY($2)"#,
    )
    .bind(company_id)
    .bind(ids)
    .fetch_all(exec)
    .await?)
}

// ───────────────────────── serializer ─────────────────────────

fn date_str(d: Option<NaiveDate>) -> Value {
    d.map_or(Value::Null, |d| {
        Value::String(d.format("%Y-%m-%d").to_string())
    })
}

/// `toEntity(row, timeZone)` (price-lists.service.ts:197).
pub fn to_entity(row: &PriceListRow, time_zone: &str) -> Value {
    json!({
        "id": row.uuid,
        "code": row.code,
        "name": row.name,
        "currencyCode": row.currency_code,
        "startDate": date_str(row.start_date),
        "endDate": date_str(row.end_date),
        "isDefault": row.is_default,
        "isActive": row.is_active,
        "isActiveToday": row.is_active_today(time_zone),
        "createdAt": ms(row.created_at),
        "updatedAt": ms_opt(row.updated_at),
    })
}

/// `toItemEntity(row, product, uom)` (price-lists.service.ts:213).
pub fn to_item_entity(row: &PriceListItemRow, product: &ProductRef, uom: &MasterRow) -> Value {
    json!({
        "id": row.uuid,
        "product": { "id": product.uuid, "sku": product.sku, "name": product.name },
        "uom": { "id": uom.uuid, "code": uom.code, "name": uom.name },
        "minQty": dec_str(&row.min_qty),
        "maxQty": dec_opt(row.max_qty.as_ref()),
        "price": dec_str(&row.price),
        "createdAt": ms(row.created_at),
        "updatedAt": ms_opt(row.updated_at),
    })
}

/// `toItemEntities` (price-lists.service.ts:230): produk & uom diambil sekali untuk semua baris.
pub async fn to_item_entities(
    conn: &mut sqlx::PgConnection,
    company_id: i32,
    rows: &[PriceListItemRow],
) -> ApiResult<Vec<Value>> {
    let mut pids: Vec<i32> = rows.iter().map(|r| r.product_id).collect();
    pids.sort_unstable();
    pids.dedup();
    let products: HashMap<i32, ProductRef> = products_by(&mut *conn, company_id, &pids)
        .await?
        .into_iter()
        .map(|p| (p.id, p))
        .collect();
    let uoms = masters::map_by_ids(
        &mut *conn,
        MasterKind::Uoms,
        company_id,
        rows.iter().map(|r| r.uom_id),
    )
    .await?;
    rows.iter()
        .map(
            |r| match (products.get(&r.product_id), uoms.get(&r.uom_id)) {
                (Some(p), Some(u)) => Ok(to_item_entity(r, p, u)),
                _ => Err(internal(&format!(
                    "Data integrity: price list item {} references a missing product/uom",
                    r.uuid
                ))),
            },
        )
        .collect()
}

// ───────────────────────── galat ─────────────────────────

fn pl_not_found() -> ApiError {
    not_found("PRICE_LIST_NOT_FOUND", "Price list not found")
}

fn item_not_found() -> ApiError {
    not_found("PRICE_LIST_ITEM_NOT_FOUND", "Price list item not found")
}

fn product_not_found() -> ApiError {
    not_found("PRODUCT_NOT_FOUND", "Product not found")
}

fn tier_overlap() -> ApiError {
    conflict(
        "PRICE_TIER_OVERLAP",
        "This qty range overlaps an existing active tier for the same product/uom",
    )
}

fn invalid_date_range(msg: &str) -> ApiError {
    bad_request("INVALID_DATE_RANGE", msg)
}

/// `rethrowPriceListP2002`: index ber-"code" → `PRICE_LIST_CODE_TAKEN`, lainnya → default conflict.
fn map_pl_write_error(e: sqlx::Error, code: &str) -> ApiError {
    match unique_constraint(&e) {
        Some(c) if c.contains("code") => conflict(
            "PRICE_LIST_CODE_TAKEN",
            format!("Price list code {code} is already used in this company"),
        ),
        Some(_) => conflict(
            "PRICE_LIST_DEFAULT_CONFLICT",
            "Another price list is already the default for this company; unset it first",
        ),
        None => e.into(),
    }
}

// ───────────────────────── tanggal ─────────────────────────

fn assert_calendar_date(value: &str, field: &str) -> ApiResult<NaiveDate> {
    NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .ok()
        .filter(|_| DATE_ONLY.is_match(value))
        .ok_or_else(|| invalid_date_range(&format!("{field} is not a valid calendar date")))
}

/// `assertDateRangeValid` (price-lists.service.ts:177).
fn assert_date_range(start: Option<&str>, end: Option<&str>) -> ApiResult<()> {
    if let Some(s) = start.filter(|s| !s.is_empty()) {
        assert_calendar_date(s, "startDate")?;
    }
    if let Some(e) = end.filter(|s| !s.is_empty()) {
        assert_calendar_date(e, "endDate")?;
    }
    if let (Some(s), Some(e)) = (
        start.filter(|s| !s.is_empty()),
        end.filter(|s| !s.is_empty()),
    ) && s > e
    {
        return Err(invalid_date_range("startDate must be on or before endDate"));
    }
    Ok(())
}

fn to_date(v: Option<&str>) -> Option<NaiveDate> {
    v.filter(|s| !s.is_empty())
        .and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok())
}

// ───────────────────────── company ─────────────────────────

/// `getCompany`: `timeZone || 'UTC'`, `currency || 'USD'`.
pub async fn company_tz_currency(
    db: &sqlx::PgPool,
    company_id: i32,
) -> ApiResult<(String, String)> {
    let row: Option<(Option<String>, Option<String>)> =
        sqlx::query_as(r#"SELECT "timeZone", currency FROM company WHERE id = $1"#)
            .bind(company_id)
            .fetch_optional(db)
            .await?;
    let (tz, cur) = row.unwrap_or((None, None));
    Ok((
        tz.filter(|t| !t.is_empty()).unwrap_or_else(|| "UTC".into()),
        cur.filter(|c| !c.is_empty())
            .unwrap_or_else(|| "USD".into()),
    ))
}

async fn find_row_or_throw<'e>(
    exec: impl PgExecutor<'e>,
    company_id: i32,
    raw: &str,
) -> ApiResult<PriceListRow> {
    find_by_uuid(exec, company_id, row_uuid(raw)?)
        .await?
        .ok_or_else(pl_not_found)
}

// ───────────────────────── DTO ─────────────────────────

const CODE_MSG: &str =
    "code must be 1-32 characters of uppercase letters, digits, underscore, or hyphen";

fn upper(v: Option<&Value>) -> Option<Value> {
    v.map(|v| match v {
        Value::String(s) => Value::String(s.to_uppercase()),
        o => o.clone(),
    })
}

/// `CreatePriceListDto` / `UpdatePriceListDto`.
#[derive(Debug, Default)]
pub struct PriceListDto {
    pub code: Field<String>,
    pub name: Field<String>,
    pub currency_code: Field<String>,
    pub start_date: Field<String>,
    pub end_date: Field<String>,
    pub is_default: Field<bool>,
    pub is_active: Field<bool>,
}

fn field<T>(
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

pub fn parse_price_list(m: &Map<String, Value>, create: bool) -> Result<PriceListDto, Vec<String>> {
    let mut e = Vec::new();
    let mut d = PriceListDto {
        code: field(m, "code", create, |v| {
            check_matches(&mut e, "code", upper(v).as_ref(), &MASTER_CODE, CODE_MSG)
        }),
        ..PriceListDto::default()
    };
    d.name = field(m, "name", create, |v| {
        check_length_string(&mut e, "name", v, 1, 150)
    });
    d.currency_code = field(m, "currencyCode", false, |v| {
        check_matches(
            &mut e,
            "currencyCode",
            upper(v).as_ref(),
            &CURRENCY_CODE,
            "currencyCode must be 3-10 uppercase letters",
        )
    });
    for (key, slot) in [
        ("startDate", &mut d.start_date),
        ("endDate", &mut d.end_date),
    ] {
        *slot = field(m, key, false, |v| {
            check_matches(
                &mut e,
                key,
                v,
                &DATE_ONLY,
                &format!("{key} must be a YYYY-MM-DD date"),
            )
        });
    }
    d.is_default = field(m, "isDefault", false, |v| {
        check_bool(&mut e, "isDefault", v)
    });
    d.is_active = field(m, "isActive", false, |v| check_bool(&mut e, "isActive", v));
    if e.is_empty() { Ok(d) } else { Err(e) }
}

fn opt_str(f: &Field<String>) -> Option<&str> {
    match f {
        Field::Val(s) => Some(s.as_str()),
        _ => None,
    }
}

/// Satu baris `PriceListItemInputDto`.
#[derive(Debug, Clone)]
pub struct ItemInput {
    pub id: Option<String>,
    pub product_id: String,
    pub uom_id: String,
    pub min_qty: Option<String>,
    pub max_qty: Option<String>,
    pub price: String,
}

/// `UpsertPriceListItemsDto` (ArrayMinSize 1 / ArrayMaxSize 1000 / ValidateNested).
pub fn parse_upsert(m: &Map<String, Value>) -> Result<Vec<ItemInput>, Vec<String>> {
    let mut e = Vec::new();
    let mut out = Vec::new();
    match m.get("items") {
        Some(Value::Array(items)) => {
            let mut child_errs = Vec::new();
            for (i, item) in items.iter().enumerate() {
                let Value::Object(o) = item else {
                    child_errs.push(format!(
                        "items.nested property {i} must be either object or array"
                    ));
                    continue;
                };
                let p = format!("items.{i}.");
                let mut ce = Vec::new();
                let id =
                    present(o, "id").and_then(|v| check_uuid4(&mut ce, &format!("{p}id"), Some(v)));
                let product_id = check_uuid4(&mut ce, &format!("{p}productId"), o.get("productId"));
                let uom_id = check_uuid4(&mut ce, &format!("{p}uomId"), o.get("uomId"));
                let min_qty = present(o, "minQty").and_then(|v| {
                    check_matches(
                        &mut ce,
                        &format!("{p}minQty"),
                        Some(v),
                        &QTY,
                        &format!("{p}minQty must be a positive number with up to 4 decimal places"),
                    )
                });
                let max_qty = present(o, "maxQty").and_then(|v| {
                    check_matches(
                        &mut ce,
                        &format!("{p}maxQty"),
                        Some(v),
                        &QTY,
                        &format!("{p}maxQty must be a positive number with up to 4 decimal places"),
                    )
                });
                let price = check_matches(
                    &mut ce,
                    &format!("{p}price"),
                    o.get("price"),
                    &PRICE,
                    &format!("{p}price must be a non-negative number with up to 2 decimal places"),
                );
                if ce.is_empty()
                    && let (Some(product_id), Some(uom_id), Some(price)) =
                        (product_id, uom_id, price)
                {
                    out.push(ItemInput {
                        id,
                        product_id,
                        uom_id,
                        min_qty,
                        max_qty,
                        price,
                    });
                }
                child_errs.extend(ce);
            }
            if !child_errs.is_empty() {
                // Nest membuang constraint induk bila ada galat anak.
                e.extend(child_errs);
            } else {
                if items.len() > MAX_PRICE_LIST_ITEMS_PER_REQUEST {
                    e.push(format!(
                        "items must contain no more than {MAX_PRICE_LIST_ITEMS_PER_REQUEST} elements"
                    ));
                }
                if items.is_empty() {
                    e.push("items must contain at least 1 elements".into());
                }
            }
        }
        Some(Value::Object(_)) => {
            e.push(format!(
                "items must contain no more than {MAX_PRICE_LIST_ITEMS_PER_REQUEST} elements"
            ));
            e.push("items must contain at least 1 elements".into());
        }
        _ => {
            e.push(format!(
                "items must contain no more than {MAX_PRICE_LIST_ITEMS_PER_REQUEST} elements"
            ));
            e.push("items must contain at least 1 elements".into());
            e.push("nested property items must be either object or array".into());
        }
    }
    if e.is_empty() { Ok(out) } else { Err(e) }
}

/// `AssignPriceListCustomersDto`: `@IsArray @ArrayMaxSize(500) @IsUUID('4', { each })`.
pub fn parse_assign(m: &Map<String, Value>) -> Result<Vec<String>, Vec<String>> {
    let mut e = Vec::new();
    let v = m.get("customerIds");
    let arr = v.and_then(Value::as_array);
    let all_uuid = match (arr, v) {
        (Some(a), _) => a
            .iter()
            .all(|x| x.as_str().is_some_and(mz_core::uuid_rules::is_uuid_v4)),
        (None, Some(Value::String(s))) => mz_core::uuid_rules::is_uuid_v4(s),
        _ => false,
    };
    if !all_uuid {
        e.push("each value in customerIds must be a UUID".into());
    }
    if arr.is_none_or(|a| a.len() > 500) {
        e.push("customerIds must contain no more than 500 elements".into());
    }
    if arr.is_none() {
        e.push("customerIds must be an array".into());
    }
    if !e.is_empty() {
        return Err(e);
    }
    Ok(arr
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default())
}

// ───────────────────────── service: price list ─────────────────────────

fn row_snapshot_audit_model() -> Model {
    Model::PriceList
}

async fn record_pl_create(state: &AppState, audit: &AuditHandle, id: i32) {
    if let Some(r) = audit
        .result_row(&state.db, row_snapshot_audit_model(), Lookup::Id(id))
        .await
    {
        audit.record_create(Model::PriceList, &r).await;
    }
}

/// Node: src/products/price-lists.service.ts:275 `create`
pub async fn create(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    dto: &PriceListDto,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    assert_date_range(opt_str(&dto.start_date), opt_str(&dto.end_date))?;
    let (tz, currency) = company_tz_currency(&state.db, company_id).await?;
    let code = opt_str(&dto.code).unwrap_or_default().to_owned();
    let name = opt_str(&dto.name).unwrap_or_default().to_owned();
    let currency = opt_str(&dto.currency_code).map_or(currency, str::to_owned);
    let is_default = matches!(dto.is_default, Field::Val(true));
    let is_active = !matches!(dto.is_active, Field::Val(false));
    let res: Result<PriceListRow, sqlx::Error> = sqlx::query_as(&format!(
        r#"INSERT INTO "priceList" ("companyId", code, name, "currencyCode", "startDate", "endDate", "isDefault", "isActive", "createdById")
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) RETURNING {PL_COLS}"#
    ))
    .bind(company_id)
    .bind(&code)
    .bind(&name)
    .bind(&currency)
    .bind(to_date(opt_str(&dto.start_date)))
    .bind(to_date(opt_str(&dto.end_date)))
    .bind(is_default)
    .bind(is_active)
    .bind(user_id)
    .fetch_one(&state.db)
    .await;
    let row = res.map_err(|e| map_pl_write_error(e, &code))?;
    record_pl_create(state, audit, row.id).await;
    Ok(to_entity(&row, &tz))
}

/// `ListPriceListsQueryDto`.
pub struct ListQuery {
    pub search: Option<String>,
    pub is_active: Option<bool>,
    pub is_default: Option<bool>,
    pub page: i64,
    pub limit: i64,
}

pub fn parse_list(m: &Map<String, Value>) -> Result<ListQuery, Vec<String>> {
    let mut e = Vec::new();
    let search = opt_max_string(m, &mut e, "search", 120);
    let is_active = query_bool(m, &mut e, "isActive");
    let is_default = query_bool(m, &mut e, "isDefault");
    let (page, limit) = page_limit_max(m, &mut e, 100);
    if e.is_empty() {
        Ok(ListQuery {
            search,
            is_active,
            is_default,
            page,
            limit,
        })
    } else {
        Err(e)
    }
}

/// Node: src/products/price-lists.service.ts:302 `findAll`
pub async fn find_all(state: &AppState, user_id: i32, q: &ListQuery) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let (tz, _) = company_tz_currency(&state.db, company_id).await?;
    let filter = |qb: &mut QueryBuilder<'_, Postgres>| {
        qb.push(r#" WHERE "companyId" = "#)
            .push_bind(company_id)
            .push(r#" AND "deletedAt" IS NULL"#);
        if let Some(a) = q.is_active {
            qb.push(r#" AND "isActive" = "#).push_bind(a);
        }
        if let Some(d) = q.is_default {
            qb.push(r#" AND "isDefault" = "#).push_bind(d);
        }
        if let Some(s) = q.search.as_deref().filter(|s| !s.is_empty()) {
            let p = like_pattern(s);
            qb.push(" AND (code ILIKE ")
                .push_bind(p.clone())
                .push(" OR name ILIKE ")
                .push_bind(p)
                .push(")");
        }
    };
    let mut qb: QueryBuilder<Postgres> =
        QueryBuilder::new(format!(r#"SELECT {PL_COLS} FROM "priceList""#));
    filter(&mut qb);
    qb.push(" ORDER BY name ASC, id ASC LIMIT ")
        .push_bind(q.limit)
        .push(" OFFSET ")
        .push_bind(offset(q.page, q.limit));
    let rows: Vec<PriceListRow> = qb.build_query_as().fetch_all(&state.db).await?;
    let mut cq: QueryBuilder<Postgres> = QueryBuilder::new(r#"SELECT COUNT(*) FROM "priceList""#);
    filter(&mut cq);
    let total: i64 = cq.build_query_scalar().fetch_one(&state.db).await?;
    let data: Vec<Value> = rows.iter().map(|r| to_entity(r, &tz)).collect();
    Ok(json!({ "data": data, "meta": page_meta(total, q.page, q.limit) }))
}

/// Node: src/products/price-lists.service.ts:333 `findOne`
pub async fn find_one(state: &AppState, user_id: i32, id: &str) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let row = find_row_or_throw(&state.db, company_id, id).await?;
    let (tz, _) = company_tz_currency(&state.db, company_id).await?;
    Ok(to_entity(&row, &tz))
}

/// Node: src/products/price-lists.service.ts:340 `update`
pub async fn update(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    id: &str,
    dto: &PriceListDto,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let (tz, _) = company_tz_currency(&state.db, company_id).await?;
    let mut tx = state.db.begin().await?;
    let current = find_row_or_throw(&mut *tx, company_id, id).await?;
    if dto.start_date.is_defined() || dto.end_date.is_defined() {
        let cur_start = current.start_date.map(|d| d.format("%Y-%m-%d").to_string());
        let cur_end = current.end_date.map(|d| d.format("%Y-%m-%d").to_string());
        let fs = match &dto.start_date {
            Field::Absent => cur_start.as_deref(),
            Field::Null => None,
            Field::Val(s) => Some(s.as_str()),
        };
        let fe = match &dto.end_date {
            Field::Absent => cur_end.as_deref(),
            Field::Null => None,
            Field::Val(s) => Some(s.as_str()),
        };
        assert_date_range(fs, fe)?;
    }
    let code_for_err = opt_str(&dto.code).unwrap_or(&current.code).to_owned();
    let pre = audit
        .pre_read(Model::PriceList, Lookup::Id(current.id))
        .await;
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(r#"UPDATE "priceList" SET "#);
    let set_text = |qb: &mut QueryBuilder<'_, Postgres>, col: &str, f: &Field<String>| match f {
        Field::Val(s) => {
            qb.push(format!(r#""{col}" = "#))
                .push_bind(s.clone())
                .push(", ");
        }
        Field::Null => {
            qb.push(format!(r#""{col}" = NULL, "#));
        }
        Field::Absent => {}
    };
    set_text(&mut qb, "code", &dto.code);
    set_text(&mut qb, "name", &dto.name);
    set_text(&mut qb, "currencyCode", &dto.currency_code);
    for (col, f) in [("startDate", &dto.start_date), ("endDate", &dto.end_date)] {
        match f {
            Field::Val(s) if !s.is_empty() => {
                qb.push(format!(r#""{col}" = "#))
                    .push_bind(to_date(Some(s)))
                    .push(", ");
            }
            Field::Absent => {}
            _ => {
                qb.push(format!(r#""{col}" = NULL, "#));
            }
        }
    }
    for (col, f) in [("isDefault", &dto.is_default), ("isActive", &dto.is_active)] {
        match f {
            Field::Val(b) => {
                qb.push(format!(r#""{col}" = "#)).push_bind(*b).push(", ");
            }
            Field::Null => {
                qb.push(format!(r#""{col}" = NULL, "#));
            }
            Field::Absent => {}
        }
    }
    qb.push(r#""updatedAt" = "#)
        .push_bind(now_naive())
        .push(" WHERE id = ")
        .push_bind(current.id)
        .push(format!(" RETURNING {PL_COLS}"));
    let updated: PriceListRow = qb
        .build_query_as()
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| map_pl_write_error(e, &code_for_err))?;
    if let Some(r) = audit
        .result_row(&mut *tx, Model::PriceList, Lookup::Id(current.id))
        .await
    {
        audit.record_update(Model::PriceList, pre, &r).await;
    }
    tx.commit().await?;
    Ok(to_entity(&updated, &tz))
}

/// Node: src/products/price-lists.service.ts:395 `remove` — soft delete, tanpa cascade item.
pub async fn remove(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    id: &str,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let row = find_row_or_throw(&state.db, company_id, id).await?;
    let now = now_naive();
    let pre = audit.pre_read(Model::PriceList, Lookup::Id(row.id)).await;
    sqlx::query(
        r#"UPDATE "priceList" SET "deletedAt" = $2, "isActive" = false, "updatedAt" = $2 WHERE id = $1"#,
    )
    .bind(row.id)
    .bind(now)
    .execute(&state.db)
    .await?;
    if let Some(r) = audit
        .result_row(&state.db, Model::PriceList, Lookup::Id(row.id))
        .await
    {
        audit.record_update(Model::PriceList, pre, &r).await;
    }
    Ok(json!({ "id": row.uuid, "deleted": true }))
}

// ───────────────────────── service: customers ─────────────────────────

#[derive(Debug, sqlx::FromRow)]
struct CustomerRow {
    uuid: String,
    code: String,
    name: String,
    #[sqlx(rename = "taxNumber")]
    tax_number: Option<String>,
    phone: Option<String>,
    #[sqlx(rename = "isActive")]
    is_active: bool,
    #[sqlx(rename = "createdAt")]
    created_at: NaiveDateTime,
    #[sqlx(rename = "updatedAt")]
    updated_at: Option<NaiveDateTime>,
    t_uuid: Option<String>,
    t_code: Option<String>,
    t_name: Option<String>,
    g_uuid: Option<String>,
    g_code: Option<String>,
    g_name: Option<String>,
    c_uuid: Option<String>,
    c_code: Option<String>,
    c_name: Option<String>,
    s_uuid: Option<String>,
    s_code: Option<String>,
    s_name: Option<String>,
    p_uuid: Option<String>,
    p_code: Option<String>,
    p_name: Option<String>,
    #[sqlx(rename = "storeCount")]
    store_count: i64,
}

fn cref(u: &Option<String>, c: &Option<String>, n: &Option<String>) -> Value {
    match (u, c, n) {
        (Some(u), Some(c), Some(n)) => json!({ "id": u, "code": c, "name": n }),
        _ => Value::Null,
    }
}

/// `toCustomerEntity` (price-lists.service.ts:418).
fn to_customer_entity(r: &CustomerRow) -> Value {
    json!({
        "id": r.uuid,
        "code": r.code,
        "name": r.name,
        "type": cref(&r.t_uuid, &r.t_code, &r.t_name),
        "group": cref(&r.g_uuid, &r.g_code, &r.g_name),
        "channel": cref(&r.c_uuid, &r.c_code, &r.c_name),
        "subChannel": cref(&r.s_uuid, &r.s_code, &r.s_name),
        "priceList": cref(&r.p_uuid, &r.p_code, &r.p_name),
        "taxNumber": r.tax_number,
        "phone": r.phone,
        "isActive": r.is_active,
        "storeCount": r.store_count,
        "createdAt": ms(r.created_at),
        "updatedAt": ms_opt(r.updated_at),
    })
}

/// `ListPriceListCustomersQueryDto` (limit maks 500).
pub fn parse_customers_query(m: &Map<String, Value>) -> Result<ListQuery, Vec<String>> {
    let mut e = Vec::new();
    let search = opt_max_string(m, &mut e, "search", 120);
    let is_active = query_bool(m, &mut e, "isActive");
    let (page, limit) = page_limit_max(m, &mut e, 500);
    if e.is_empty() {
        Ok(ListQuery {
            search,
            is_active,
            is_default: None,
            page,
            limit,
        })
    } else {
        Err(e)
    }
}

/// Node: src/products/price-lists.service.ts:437 `listCustomers`
pub async fn list_customers(
    state: &AppState,
    user_id: i32,
    id: &str,
    q: &ListQuery,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let pl = find_row_or_throw(&state.db, company_id, id).await?;
    let filter = |qb: &mut QueryBuilder<'_, Postgres>| {
        qb.push(r#" WHERE c."companyId" = "#)
            .push_bind(company_id)
            .push(r#" AND c."priceListId" = "#)
            .push_bind(pl.id)
            .push(r#" AND c."deletedAt" IS NULL"#);
        if let Some(a) = q.is_active {
            qb.push(r#" AND c."isActive" = "#).push_bind(a);
        }
        if let Some(s) = q.search.as_deref().filter(|s| !s.is_empty()) {
            let p = like_pattern(s);
            qb.push(" AND (c.code ILIKE ")
                .push_bind(p.clone())
                .push(" OR c.name ILIKE ")
                .push_bind(p)
                .push(")");
        }
    };
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
        r#"SELECT c.uuid::text AS uuid, c.code, c.name, c."taxNumber", c.phone, c."isActive", c."createdAt", c."updatedAt",
             t.uuid::text AS t_uuid, t.code AS t_code, t.name AS t_name,
             g.uuid::text AS g_uuid, g.code AS g_code, g.name AS g_name,
             ch.uuid::text AS c_uuid, ch.code AS c_code, ch.name AS c_name,
             sc.uuid::text AS s_uuid, sc.code AS s_code, sc.name AS s_name,
             pl.uuid::text AS p_uuid, pl.code AS p_code, pl.name AS p_name,
             (SELECT COUNT(*) FROM store st WHERE st."customerId" = c.id AND st."deletedAt" IS NULL) AS "storeCount"
           FROM customer c
           LEFT JOIN "customerType" t ON t.id = c."typeId"
           LEFT JOIN "customerGroup" g ON g.id = c."groupId"
           LEFT JOIN "customerChannel" ch ON ch.id = c."channelId"
           LEFT JOIN "customerSubChannel" sc ON sc.id = c."subChannelId"
           LEFT JOIN "priceList" pl ON pl.id = c."priceListId""#,
    );
    filter(&mut qb);
    qb.push(" ORDER BY c.name ASC, c.id ASC LIMIT ")
        .push_bind(q.limit)
        .push(" OFFSET ")
        .push_bind(offset(q.page, q.limit));
    let rows: Vec<CustomerRow> = qb.build_query_as().fetch_all(&state.db).await?;
    let mut cq: QueryBuilder<Postgres> = QueryBuilder::new("SELECT COUNT(*) FROM customer c");
    filter(&mut cq);
    let total: i64 = cq.build_query_scalar().fetch_one(&state.db).await?;
    let data: Vec<Value> = rows.iter().map(to_customer_entity).collect();
    Ok(json!({ "data": data, "meta": page_meta(total, q.page, q.limit) }))
}

/// Node: src/products/price-lists.service.ts:488 `assignCustomers` — full-replace, satu transaksi.
pub async fn assign_customers(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    id: &str,
    customer_ids: &[String],
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let uuids: Vec<uuid::Uuid> = customer_ids
        .iter()
        .map(|u| row_uuid(u))
        .collect::<ApiResult<_>>()?;
    let mut tx = state.db.begin().await?;
    let pl = find_row_or_throw(&mut *tx, company_id, id).await?;
    let matched: Vec<String> = if uuids.is_empty() {
        Vec::new()
    } else {
        sqlx::query_scalar(
            r#"SELECT uuid::text FROM customer WHERE "companyId" = $1 AND uuid = ANY($2) AND "deletedAt" IS NULL"#,
        )
        .bind(company_id)
        .bind(&uuids)
        .fetch_all(&mut *tx)
        .await?
    };
    let matched_set: HashSet<String> = matched.iter().map(|s| s.to_lowercase()).collect();
    let now = now_naive();

    let (pl_id, uuids_un) = (pl.id, uuids.clone());
    let pre = audit
        .pre_read_many(
            Model::Customer,
            Lookup::filter(move |qb| {
                qb.push(r#""companyId" = "#)
                    .push_bind(company_id)
                    .push(r#" AND "priceListId" = "#)
                    .push_bind(pl_id)
                    .push(r#" AND NOT (uuid = ANY("#)
                    .push_bind(uuids_un.clone())
                    .push(r#")) AND "deletedAt" IS NULL"#);
            }),
        )
        .await;
    let unassigned = sqlx::query(
        r#"UPDATE customer SET "priceListId" = NULL, "updatedAt" = $4
           WHERE "companyId" = $1 AND "priceListId" = $2 AND NOT (uuid = ANY($3)) AND "deletedAt" IS NULL"#,
    )
    .bind(company_id)
    .bind(pl.id)
    .bind(&uuids)
    .bind(now)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    audit
        .record_update_many(
            Model::Customer,
            pre,
            unassigned,
            &Row::new()
                .with("priceListId", Value::Null)
                .with("updatedAt", json!(JsDate(now))),
            json!({
                "companyId": company_id,
                "priceListId": pl.id,
                "uuid": { "notIn": customer_ids },
                "deletedAt": null,
            }),
        )
        .await;

    let assigned = if matched.is_empty() {
        0
    } else {
        let matched_uuids: Vec<uuid::Uuid> =
            matched.iter().filter_map(|u| row_uuid(u).ok()).collect();
        let mu = matched_uuids.clone();
        let pre = audit
            .pre_read_many(
                Model::Customer,
                Lookup::filter(move |qb| {
                    qb.push(r#""companyId" = "#)
                        .push_bind(company_id)
                        .push(" AND uuid = ANY(")
                        .push_bind(mu.clone())
                        .push(r#") AND "deletedAt" IS NULL"#);
                }),
            )
            .await;
        let n = sqlx::query(
            r#"UPDATE customer SET "priceListId" = $3, "updatedAt" = $4
               WHERE "companyId" = $1 AND uuid = ANY($2) AND "deletedAt" IS NULL"#,
        )
        .bind(company_id)
        .bind(&matched_uuids)
        .bind(pl.id)
        .bind(now)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        audit
            .record_update_many(
                Model::Customer,
                pre,
                n,
                &Row::new()
                    .with("priceListId", pl.id)
                    .with("updatedAt", json!(JsDate(now))),
                json!({ "companyId": company_id, "uuid": { "in": matched }, "deletedAt": null }),
            )
            .await;
        n
    };
    tx.commit().await?;
    let not_found: Vec<&String> = customer_ids
        .iter()
        .filter(|u| !matched_set.contains(&u.to_lowercase()))
        .collect();
    Ok(json!({ "assigned": assigned, "unassigned": unassigned, "notFound": not_found }))
}

// ───────────────────────── service: items ─────────────────────────

/// Query `ListPriceListItemsQueryDto`.
pub struct ItemsQuery {
    pub product_id: Option<String>,
    pub page: i64,
    pub limit: i64,
}

pub fn parse_items_query(m: &Map<String, Value>) -> Result<ItemsQuery, Vec<String>> {
    let mut e = Vec::new();
    let product_id =
        present(m, "productId").and_then(|v| check_uuid4(&mut e, "productId", Some(v)));
    let (page, limit) = page_limit_max(m, &mut e, 100);
    if e.is_empty() {
        Ok(ItemsQuery {
            product_id,
            page,
            limit,
        })
    } else {
        Err(e)
    }
}

/// Node: src/products/price-lists.service.ts:533 `listItems`
pub async fn list_items(
    state: &AppState,
    user_id: i32,
    id: &str,
    q: &ItemsQuery,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let pl = find_row_or_throw(&state.db, company_id, id).await?;
    let product_filter = match &q.product_id {
        Some(p) => Some(
            sqlx::query_scalar::<_, i32>(
                r#"SELECT id FROM product WHERE uuid = $1 AND "companyId" = $2 AND "deletedAt" IS NULL LIMIT 1"#,
            )
            .bind(row_uuid(p)?)
            .bind(company_id)
            .fetch_optional(&state.db)
            .await?
            .ok_or_else(product_not_found)?,
        ),
        None => None,
    };
    let rows: Vec<PriceListItemRow> = sqlx::query_as(&format!(
        r#"SELECT {ITEM_COLS} FROM "priceListItem"
           WHERE "priceListId" = $1 AND "deletedAt" IS NULL AND ($2::int IS NULL OR "productId" = $2)
           ORDER BY id ASC LIMIT $3 OFFSET $4"#
    ))
    .bind(pl.id)
    .bind(product_filter)
    .bind(q.limit)
    .bind(offset(q.page, q.limit))
    .fetch_all(&state.db)
    .await?;
    let total: i64 = sqlx::query_scalar(
        r#"SELECT COUNT(*) FROM "priceListItem"
           WHERE "priceListId" = $1 AND "deletedAt" IS NULL AND ($2::int IS NULL OR "productId" = $2)"#,
    )
    .bind(pl.id)
    .bind(product_filter)
    .fetch_one(&state.db)
    .await?;
    let mut conn = state.db.acquire().await?;
    let data = to_item_entities(&mut conn, company_id, &rows).await?;
    Ok(json!({ "data": data, "meta": page_meta(total, q.page, q.limit) }))
}

async fn find_item<'e>(
    exec: impl PgExecutor<'e>,
    price_list_id: i32,
    item: &str,
) -> ApiResult<PriceListItemRow> {
    sqlx::query_as(&format!(
        r#"SELECT {ITEM_COLS} FROM "priceListItem" WHERE uuid = $1 AND "priceListId" = $2 AND "deletedAt" IS NULL LIMIT 1"#
    ))
    .bind(row_uuid(item)?)
    .bind(price_list_id)
    .fetch_optional(exec)
    .await?
    .ok_or_else(item_not_found)
}

/// Node: src/products/price-lists.service.ts:565 `getItem`
pub async fn get_item(state: &AppState, user_id: i32, id: &str, item: &str) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let pl = find_row_or_throw(&state.db, company_id, id).await?;
    let row = find_item(&state.db, pl.id, item).await?;
    let mut conn = state.db.acquire().await?;
    to_item_entities(&mut conn, company_id, std::slice::from_ref(&row))
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| internal("item hilang"))
}

/// Node: src/products/price-lists.service.ts:869 `removeItem`
pub async fn remove_item(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    id: &str,
    item: &str,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let mut tx = state.db.begin().await?;
    let pl = find_row_or_throw(&mut *tx, company_id, id).await?;
    let row = find_item(&mut *tx, pl.id, item).await?;
    let now = now_naive();
    let pre = audit
        .pre_read(Model::PriceListItem, Lookup::Id(row.id))
        .await;
    sqlx::query(r#"UPDATE "priceListItem" SET "deletedAt" = $2, "updatedAt" = $2 WHERE id = $1"#)
        .bind(row.id)
        .bind(now)
        .execute(&mut *tx)
        .await?;
    if let Some(r) = audit
        .result_row(&mut *tx, Model::PriceListItem, Lookup::Id(row.id))
        .await
    {
        audit.record_update(Model::PriceListItem, pre, &r).await;
    }
    tx.commit().await?;
    Ok(json!({ "id": row.uuid, "deleted": true }))
}

struct Resolved<'a> {
    existing: Option<&'a PriceListItemRow>,
    product: &'a ProductRef,
    uom: &'a MasterRow,
    min_qty: String,
    max_qty: Option<String>,
    price: String,
}

fn assert_item_qty(min: &str, max: Option<&str>) -> ApiResult<()> {
    let invalid = |m: &str| unprocessable("PRICE_LIST_ITEM_INVALID_QTY", m);
    if compare_decimal_strings(min, "0").is_le() {
        return Err(invalid("minQty must be greater than 0"));
    }
    if max.is_some_and(|m| compare_decimal_strings(m, min).is_lt()) {
        return Err(invalid("maxQty must be greater than or equal to minQty"));
    }
    Ok(())
}

fn uniq<T: Clone + Eq + std::hash::Hash>(it: impl IntoIterator<Item = T>) -> Vec<T> {
    let mut seen = HashSet::new();
    it.into_iter().filter(|x| seen.insert(x.clone())).collect()
}

/// Node: src/products/price-lists.service.ts:637 `upsertItems` — bulk upsert satu transaksi.
pub async fn upsert_items(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    id: &str,
    items: &[ItemInput],
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let mut tx = state.db.begin().await?;
    let pl = find_row_or_throw(&mut *tx, company_id, id).await?;

    let parse = |s: &String| row_uuid(s);
    let product_uuids: Vec<uuid::Uuid> = uniq(
        items
            .iter()
            .map(|i| parse(&i.product_id))
            .collect::<ApiResult<Vec<_>>>()?,
    );
    let uom_uuids: Vec<uuid::Uuid> = uniq(
        items
            .iter()
            .map(|i| parse(&i.uom_id))
            .collect::<ApiResult<Vec<_>>>()?,
    );
    let item_uuids: Vec<uuid::Uuid> = uniq(
        items
            .iter()
            .filter_map(|i| i.id.as_ref().filter(|s| !s.is_empty()))
            .map(parse)
            .collect::<ApiResult<Vec<_>>>()?,
    );

    let products: Vec<ProductRef> = sqlx::query_as(
        r#"SELECT id, uuid::text AS uuid, sku, name FROM product
           WHERE "companyId" = $1 AND uuid = ANY($2) AND "deletedAt" IS NULL"#,
    )
    .bind(company_id)
    .bind(&product_uuids)
    .fetch_all(&mut *tx)
    .await?;
    let uoms = masters::find_by_uuids(&mut *tx, MasterKind::Uoms, company_id, &uom_uuids).await?;
    let existing_items: Vec<PriceListItemRow> = if item_uuids.is_empty() {
        Vec::new()
    } else {
        sqlx::query_as(&format!(
            r#"SELECT {ITEM_COLS} FROM "priceListItem" WHERE "priceListId" = $1 AND uuid = ANY($2) AND "deletedAt" IS NULL"#
        ))
        .bind(pl.id)
        .bind(&item_uuids)
        .fetch_all(&mut *tx)
        .await?
    };
    let key = |s: &str| s.to_lowercase();
    let product_by: HashMap<String, &ProductRef> =
        products.iter().map(|p| (key(&p.uuid), p)).collect();
    let uom_by: HashMap<String, &MasterRow> = uoms.iter().map(|u| (key(&u.uuid), u)).collect();
    let item_by: HashMap<String, &PriceListItemRow> =
        existing_items.iter().map(|i| (key(&i.uuid), i)).collect();

    let mut resolved: Vec<Resolved> = Vec::with_capacity(items.len());
    for it in items {
        let product = *product_by
            .get(&key(&it.product_id))
            .ok_or_else(product_not_found)?;
        let uom = *uom_by
            .get(&key(&it.uom_id))
            .ok_or_else(|| not_found("PRODUCT_MASTER_NOT_FOUND", "Unit of measure not found"))?;
        let existing = match it.id.as_deref().filter(|s| !s.is_empty()) {
            Some(i) => Some(*item_by.get(&key(i)).ok_or_else(item_not_found)?),
            None => None,
        };
        let min_qty = it.min_qty.clone().unwrap_or_else(|| "1".into());
        let max_qty = it.max_qty.clone();
        assert_item_qty(&min_qty, max_qty.as_deref())?;
        resolved.push(Resolved {
            existing,
            product,
            uom,
            min_qty,
            max_qty,
            price: it.price.clone(),
        });
    }

    // assertUomsBelongToProducts (price-lists.service.ts:595)
    let batch_products: Vec<i32> = uniq(resolved.iter().map(|r| r.product.id));
    let batch_uoms: Vec<i32> = uniq(resolved.iter().map(|r| r.uom.id));
    let pairs: HashSet<(i32, i32)> = sqlx::query_as::<_, (i32, i32)>(
        r#"SELECT "productId", "uomId" FROM "productUom" WHERE "productId" = ANY($1)"#,
    )
    .bind(&batch_products)
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .collect();
    for r in &resolved {
        if !pairs.contains(&(r.product.id, r.uom.id)) {
            return Err(unprocessable(
                "UOM_NOT_IN_PRODUCT",
                format!(
                    "UOM {} is not one of SKU {}'s units of measure",
                    r.uom.code, r.product.sku
                ),
            ));
        }
    }

    let existing_active: Vec<PriceListItemRow> = sqlx::query_as(&format!(
        r#"SELECT {ITEM_COLS} FROM "priceListItem"
           WHERE "priceListId" = $1 AND "productId" = ANY($2) AND "uomId" = ANY($3) AND "deletedAt" IS NULL
           ORDER BY id"#
    ))
    .bind(pl.id)
    .bind(&batch_products)
    .bind(&batch_uoms)
    .fetch_all(&mut *tx)
    .await?;

    // Pemeriksaan tumpang tindih per grup (productId, uomId).
    let mut group_keys: Vec<(i32, i32)> = Vec::new();
    let mut groups: HashMap<(i32, i32), Vec<&Resolved>> = HashMap::new();
    for r in &resolved {
        let k = (r.product.id, r.uom.id);
        if !groups.contains_key(&k) {
            group_keys.push(k);
        }
        groups.entry(k).or_default().push(r);
    }
    for k in &group_keys {
        let group = groups.get(k).map(Vec::as_slice).unwrap_or_default();
        let updating: HashSet<i32> = group
            .iter()
            .filter_map(|g| g.existing.map(|e| e.id))
            .collect();
        let mut candidates: Vec<QtyRange> = existing_active
            .iter()
            .filter(|row| (row.product_id, row.uom_id) == *k && !updating.contains(&row.id))
            .map(PriceListItemRow::range)
            .collect();
        candidates.extend(group.iter().map(|g| QtyRange {
            min: g.min_qty.clone(),
            max: g.max_qty.clone(),
        }));
        for (i, a) in candidates.iter().enumerate() {
            for b in candidates.iter().skip(i + 1) {
                if qty_ranges_overlap(a, b) {
                    return Err(tier_overlap());
                }
            }
        }
    }

    // writeResolvedItems (price-lists.service.ts:801)
    let mut written: Vec<Option<PriceListItemRow>> = vec![None; resolved.len()];
    let to_create: Vec<(usize, &Resolved)> = resolved
        .iter()
        .enumerate()
        .filter(|(_, r)| r.existing.is_none())
        .collect();
    if !to_create.is_empty() {
        let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
            r#"INSERT INTO "priceListItem" ("companyId", "priceListId", "productId", "uomId", "minQty", "maxQty", price) "#,
        );
        qb.push_values(to_create.iter(), |mut b, (_, r)| {
            b.push_bind(company_id)
                .push_bind(pl.id)
                .push_bind(r.product.id)
                .push_bind(r.uom.id)
                .push_bind(r.min_qty.clone())
                .push_unseparated("::numeric")
                .push_bind(r.max_qty.clone())
                .push_unseparated("::numeric")
                .push_bind(r.price.clone())
                .push_unseparated("::numeric");
        });
        qb.push(format!(" RETURNING {ITEM_COLS}"));
        let created: Vec<PriceListItemRow> = match qb.build_query_as().fetch_all(&mut *tx).await {
            Ok(rows) => rows,
            Err(e) if mz_core::db::is_unique_violation(&e) => return Err(tier_overlap()),
            Err(e) => return Err(e.into()),
        };
        let audit_rows: Vec<Row> = to_create
            .iter()
            .map(|(_, r)| {
                Row::new()
                    .with("companyId", company_id)
                    .with("priceListId", pl.id)
                    .with("productId", r.product.id)
                    .with("uomId", r.uom.id)
                    .with("minQty", r.min_qty.clone())
                    .with(
                        "maxQty",
                        r.max_qty.clone().map_or(Value::Null, Value::String),
                    )
                    .with("price", r.price.clone())
            })
            .collect();
        audit
            .record_create_many(Model::PriceListItem, &audit_rows, created.len() as u64)
            .await;
        for (idx, r) in &to_create {
            let m = created.iter().find(|row| {
                row.product_id == r.product.id
                    && row.uom_id == r.uom.id
                    && compare_decimal_strings(&dec_str(&row.min_qty), &r.min_qty).is_eq()
            });
            let Some(m) = m else {
                return Err(internal(&format!(
                    "Data integrity: price list item for product {} / uom {} was created but not found back in the same transaction",
                    r.product.sku, r.uom.code
                )));
            };
            if let Some(slot) = written.get_mut(*idx) {
                *slot = Some(m.clone());
            }
        }
    }
    let now = now_naive();
    for (idx, r) in resolved.iter().enumerate() {
        let Some(ex) = r.existing else { continue };
        let pre = audit
            .pre_read(Model::PriceListItem, Lookup::Id(ex.id))
            .await;
        let res: Result<PriceListItemRow, sqlx::Error> = sqlx::query_as(&format!(
            r#"UPDATE "priceListItem" SET "minQty" = $2::numeric, "maxQty" = $3::numeric, price = $4::numeric, "updatedAt" = $5
               WHERE id = $1 RETURNING {ITEM_COLS}"#
        ))
        .bind(ex.id)
        .bind(&r.min_qty)
        .bind(&r.max_qty)
        .bind(&r.price)
        .bind(now)
        .fetch_one(&mut *tx)
        .await;
        let row = match res {
            Ok(row) => row,
            Err(e) if mz_core::db::is_unique_violation(&e) => return Err(tier_overlap()),
            Err(e) => return Err(e.into()),
        };
        if let Some(a) = audit
            .result_row(&mut *tx, Model::PriceListItem, Lookup::Id(ex.id))
            .await
        {
            audit.record_update(Model::PriceListItem, pre, &a).await;
        }
        if let Some(slot) = written.get_mut(idx) {
            *slot = Some(row);
        }
    }
    let rows: Vec<PriceListItemRow> = written.into_iter().flatten().collect();
    let data = to_item_entities(&mut tx, company_id, &rows).await?;
    tx.commit().await?;
    Ok(json!({ "data": data }))
}

// ───────────────────────── handler ─────────────────────────

/// Node: src/products/admin-price-lists.controller.ts:124 `findAll`
pub async fn h_find_all(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedQuery(q): ValidatedQuery<Raw>,
) -> ApiResult<Json<Value>> {
    let q = parse_list(&q.0).map_err(ApiError::Validation)?;
    Ok(Json(find_all(&state, ctx.require_user_id()?, &q).await?))
}

/// Node: src/products/admin-price-lists.controller.ts:137 `create`
pub async fn h_create(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    ValidatedJson(body): ValidatedJson<Raw>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let dto = parse_price_list(&body.0, true).map_err(ApiError::Validation)?;
    let data = create(&state, &audit, ctx.require_user_id()?, &dto).await?;
    Ok((StatusCode::CREATED, Json(json!({ "data": data }))))
}

/// Node: src/products/admin-price-lists.controller.ts:153 `findOne`
pub async fn h_find_one(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let data = find_one(&state, ctx.require_user_id()?, &id).await?;
    Ok(Json(json!({ "data": data })))
}

/// Node: src/products/admin-price-lists.controller.ts:164 `update`
pub async fn h_update(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    Path(id): Path<String>,
    ValidatedJson(body): ValidatedJson<Raw>,
) -> ApiResult<Json<Value>> {
    let dto = parse_price_list(&body.0, false).map_err(ApiError::Validation)?;
    let data = update(&state, &audit, ctx.require_user_id()?, &id, &dto).await?;
    Ok(Json(json!({ "data": data })))
}

/// Node: src/products/admin-price-lists.controller.ts:182 `remove`
pub async fn h_remove(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let data = remove(&state, &audit, ctx.require_user_id()?, &id).await?;
    Ok(Json(json!({ "data": data })))
}

/// Node: src/products/admin-price-lists.controller.ts:198 `listItems`
pub async fn h_list_items(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(id): Path<String>,
    ValidatedQuery(q): ValidatedQuery<Raw>,
) -> ApiResult<Json<Value>> {
    let q = parse_items_query(&q.0).map_err(ApiError::Validation)?;
    Ok(Json(
        list_items(&state, ctx.require_user_id()?, &id, &q).await?,
    ))
}

/// Node: src/products/admin-price-lists.controller.ts:217 `upsertItems`
pub async fn h_upsert_items(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    Path(id): Path<String>,
    ValidatedJson(body): ValidatedJson<Raw>,
) -> ApiResult<Json<Value>> {
    let items = parse_upsert(&body.0).map_err(ApiError::Validation)?;
    Ok(Json(
        upsert_items(&state, &audit, ctx.require_user_id()?, &id, &items).await?,
    ))
}

/// Node: src/products/admin-price-lists.controller.ts:258 `getItem`
pub async fn h_get_item(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path((id, item)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let data = get_item(&state, ctx.require_user_id()?, &id, &item).await?;
    Ok(Json(json!({ "data": data })))
}

/// Node: src/products/admin-price-lists.controller.ts:275 `removeItem`
pub async fn h_remove_item(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    Path((id, item)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let data = remove_item(&state, &audit, ctx.require_user_id()?, &id, &item).await?;
    Ok(Json(json!({ "data": data })))
}

/// Node: src/products/admin-price-lists.controller.ts:287 `listCustomers`
pub async fn h_list_customers(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(id): Path<String>,
    ValidatedQuery(q): ValidatedQuery<Raw>,
) -> ApiResult<Json<Value>> {
    let q = parse_customers_query(&q.0).map_err(ApiError::Validation)?;
    Ok(Json(
        list_customers(&state, ctx.require_user_id()?, &id, &q).await?,
    ))
}

/// Node: src/products/admin-price-lists.controller.ts:306 `assignCustomers`
pub async fn h_assign_customers(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    Path(id): Path<String>,
    ValidatedJson(body): ValidatedJson<Raw>,
) -> ApiResult<Json<Value>> {
    let ids = parse_assign(&body.0).map_err(ApiError::Validation)?;
    let data = assign_customers(&state, &audit, ctx.require_user_id()?, &id, &ids).await?;
    Ok(Json(json!({ "data": data })))
}

/// 11 route `/v1/admin/price-lists*`.
pub fn routes(state: &AppState) -> Router<AppState> {
    const P: &str = "/v1/admin/price-lists";
    const PI: &str = "/v1/admin/price-lists/{id}";
    const PIT: &str = "/v1/admin/price-lists/{id}/items";
    const PITI: &str = "/v1/admin/price-lists/{id}/items/{itemId}";
    const PC: &str = "/v1/admin/price-lists/{id}/customers";
    let s = PRICE_LISTS;
    Router::new()
        .route(P, guard(state, s, get(h_find_all)))
        .route(P, guard(state, s, post(h_create)))
        .route(PI, guard(state, s, get(h_find_one)))
        .route(PI, guard(state, s, patch(h_update)))
        .route(PI, guard(state, s, delete(h_remove)))
        .route(PIT, guard(state, s, get(h_list_items)))
        .route(PIT, guard(state, s, put(h_upsert_items)))
        .route(PITI, guard(state, s, get(h_get_item)))
        .route(PITI, guard(state, s, delete(h_remove_item)))
        .route(PC, guard(state, s, get(h_list_customers)))
        .route(PC, guard(state, s, patch(h_assign_customers)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dto_price_list() {
        let m = json!({"code": "pl-gt", "name": "GT", "startDate": "2026-1-1"});
        let e = parse_price_list(m.as_object().unwrap(), true).unwrap_err();
        assert_eq!(e, vec!["startDate must be a YYYY-MM-DD date"]);
        let m = json!({"code": "pl-gt", "name": "GT", "endDate": null});
        let d = parse_price_list(m.as_object().unwrap(), true).unwrap();
        assert_eq!(d.code, Field::Val("PL-GT".into()));
        assert_eq!(d.end_date, Field::Null);
        let e = parse_price_list(&Map::new(), true).unwrap_err();
        assert_eq!(e[0], CODE_MSG);
    }

    #[test]
    fn rentang_tanggal() {
        assert!(assert_date_range(Some("2026-02-30"), None).is_err());
        assert!(assert_date_range(Some("2026-03-02"), Some("2026-03-01")).is_err());
        assert!(assert_date_range(Some("2026-03-01"), Some("2026-03-01")).is_ok());
    }

    #[test]
    fn dto_upsert() {
        let e = parse_upsert(&Map::new()).unwrap_err();
        assert_eq!(e.len(), 3);
        let m = json!({"items": []});
        assert_eq!(
            parse_upsert(m.as_object().unwrap()).unwrap_err(),
            vec!["items must contain at least 1 elements"]
        );
        let m = json!({"items": [{"productId": "x", "price": 5}, 3]});
        assert_eq!(
            parse_upsert(m.as_object().unwrap()).unwrap_err(),
            vec![
                "items.0.productId must be a UUID",
                "items.0.uomId must be a UUID",
                "items.0.price must be a non-negative number with up to 2 decimal places",
                "items.0.price must be a string",
                "items.nested property 1 must be either object or array",
            ]
        );
    }

    #[test]
    fn dto_assign() {
        let e = parse_assign(&Map::new()).unwrap_err();
        assert_eq!(e.len(), 3);
        let m = json!({"customerIds": []});
        assert_eq!(
            parse_assign(m.as_object().unwrap()).unwrap(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn qty_item() {
        assert!(assert_item_qty("0", None).is_err());
        assert!(assert_item_qty("2", Some("1")).is_err());
        assert!(assert_item_qty("1", Some("1")).is_ok());
    }
}
