//! Port `src/orders/orders.service.ts` — order `draft` (create, PATCH header, REPLACE item),
//! detail/list/owner picker, filter bersama list+export (`buildWhere`). Transaksi, kunci, dan
//! compare-and-set (`UPDATE … WHERE status = 'draft'`) sama dengan Node.

use std::collections::{HashMap, HashSet};

use chrono::{NaiveDate, NaiveDateTime, Utc};
use mz_audit::{AuditHandle, Lookup, Model, Row};
use mz_core::company::require_company_id;
use mz_core::time::{JsDate, now_naive};
use mz_core::{ApiError, ApiResult, AppState};
use mz_products::custom_fields_validator::{
    CfDefinition, CfEntity, CfIssue, js_keys, validate_custom_field_values,
};
use mz_products::dto::Field;
use mz_table_fields::policy::CensoredColumns;
use rust_decimal::Decimal;
use serde_json::{Map, Value, json};
use sqlx::{PgConnection, Postgres, QueryBuilder};

use crate::common::{
    assert_order_date_in_range, assert_valid_calendar_date, bad_request, company_tz_currency,
    conflict, date_str, internal, like_pattern, not_found, offset, order_immutable,
    order_not_found, page_meta, unprocessable, unprocessable_details,
};
use crate::custom_fields::definitions;
use crate::dto::{CreateOrder, ItemInput, OrderFilter, UpdateOrder};
use crate::entity::{self, OrderRow, items_of, to_order_entity};
use crate::masters::{self, MasterKind, MasterRow};
use crate::pricing::{
    self, CarriedDiscount, IncomingItemKey, LineInput, PriceItemCandidate, PriceListSnap,
    StoredItemDiscount, carry_censored_discounts, compute_line, compute_totals, dec, dstr,
    resolve_effective_price_list, resolve_uom_price,
};

/// Langkah `beforeCommit` di dalam transaksi tulis item (§10.2 mobile: toleransi harga + submit).
pub enum BeforeCommit<'a> {
    None,
    MobileSubmit {
        user_id: i32,
        items: &'a [ItemInput],
    },
}

impl BeforeCommit<'_> {
    fn is_some(&self) -> bool {
        !matches!(self, Self::None)
    }
}

/// `buildDraftOrderNumber` — `DRAFT-` + 24 hex (96 bit).
pub fn build_draft_order_number() -> String {
    let hex = uuid::Uuid::new_v4().simple().to_string();
    format!("DRAFT-{}", hex.get(..24).unwrap_or(&hex))
}

fn js_date(d: NaiveDate) -> NaiveDateTime {
    d.and_hms_opt(0, 0, 0).unwrap_or_default()
}

// ───────────────────────── resolusi master & jangkar ─────────────────────────

async fn resolve_master(
    tx: &mut PgConnection,
    kind: MasterKind,
    company_id: i32,
    uuid: &str,
) -> ApiResult<MasterRow> {
    let row = masters::find_by_uuid(&mut *tx, kind, company_id, uuid)
        .await?
        .ok_or_else(|| {
            not_found(
                "ORDER_MASTER_NOT_FOUND",
                format!("{} not found", kind.label()),
            )
        })?;
    if !row.is_active {
        return Err(unprocessable(
            "ORDER_MASTER_ARCHIVED",
            format!("{} is archived", kind.label()),
        ));
    }
    Ok(row)
}

/// `resolveOptionalPaymentTerm/Method` (`null`/absen → `None`).
async fn resolve_optional(
    tx: &mut PgConnection,
    kind: MasterKind,
    company_id: i32,
    uuid: Option<&str>,
) -> ApiResult<Option<MasterRow>> {
    match uuid {
        None => Ok(None),
        Some(u) => resolve_master(tx, kind, company_id, u).await.map(Some),
    }
}

/// `resolveOwner` — anggota aktif company (`userCompany.status = 1`).
async fn resolve_owner(tx: &mut PgConnection, company_id: i32, uuid: &str) -> ApiResult<i32> {
    let id: Option<i32> = sqlx::query_scalar(
        r#"SELECT u.id FROM "userCompany" uc JOIN users u ON u.id = uc."userId"
            WHERE uc."companyId" = $1 AND uc.status = 1 AND u.uuid = $2 ORDER BY uc.id LIMIT 1"#,
    )
    .bind(company_id)
    .bind(crate::common::row_uuid(uuid)?)
    .fetch_optional(&mut *tx)
    .await?;
    id.ok_or_else(|| {
        not_found(
            "ORDER_OWNER_NOT_FOUND",
            "Owner is not an active employee of this company",
        )
    })
}

/// Jangkar §0.9 hasil `resolveAnchor`.
#[derive(Debug, Clone, Copy)]
pub struct Anchor {
    pub customer_id: i32,
    pub store_id: Option<i32>,
    pub store_group_id: Option<i32>,
    pub store_visit_id: Option<i32>,
}

/// Node: src/orders/orders.service.ts:330 `resolveAnchor` (nilai kosong/`null` = tidak dikirim).
async fn resolve_anchor(
    tx: &mut PgConnection,
    company_id: i32,
    customer: Option<&str>,
    store: Option<&str>,
    visit: Option<&str>,
    owner_id: i32,
) -> ApiResult<Anchor> {
    if customer.is_none() && store.is_none() {
        return Err(unprocessable(
            "CUSTOMER_OR_STORE_REQUIRED",
            "Either customerId or storeId is required (§0.9)",
        ));
    }
    if visit.is_some() && store.is_none() {
        return Err(unprocessable(
            "STORE_VISIT_REQUIRES_STORE",
            "storeVisitId requires storeId (§13.9)",
        ));
    }
    let (customer_id, store_id, store_group_id) = if let Some(s) = store {
        let row: Option<(i32, Option<i32>, Option<i32>)> = sqlx::query_as(
            r#"SELECT id, "groupId", "customerId" FROM store
                WHERE uuid = $1 AND "companyId" = $2 AND "deletedAt" IS NULL LIMIT 1"#,
        )
        .bind(crate::common::row_uuid(s)?)
        .bind(company_id)
        .fetch_optional(&mut *tx)
        .await?;
        let (id, group, cust) =
            row.ok_or_else(|| not_found("STORE_NOT_FOUND", "Store not found"))?;
        let cust = cust.ok_or_else(|| {
            unprocessable(
                "STORE_HAS_NO_CUSTOMER",
                "Store is not linked to a customer account (§0.9)",
            )
        })?;
        (cust, Some(id), group)
    } else {
        let id: Option<i32> = sqlx::query_scalar(
            r#"SELECT id FROM customer WHERE uuid = $1 AND "companyId" = $2 AND "deletedAt" IS NULL LIMIT 1"#,
        )
        .bind(crate::common::row_uuid(customer.unwrap_or_default())?)
        .bind(company_id)
        .fetch_optional(&mut *tx)
        .await?;
        (
            id.ok_or_else(|| not_found("CUSTOMER_NOT_FOUND", "Customer not found"))?,
            None,
            None,
        )
    };
    let mut store_visit_id = None;
    if let Some(v) = visit {
        let row: Option<(i32, i32, i32)> = sqlx::query_as(
            r#"SELECT id, "userId", "storeId" FROM "storeVisit" WHERE uuid = $1 AND "companyId" = $2 LIMIT 1"#,
        )
        .bind(crate::common::row_uuid(v)?)
        .bind(company_id)
        .fetch_optional(&mut *tx)
        .await?;
        let (id, user_id, visit_store) =
            row.ok_or_else(|| not_found("STORE_VISIT_NOT_FOUND", "Store visit not found"))?;
        if user_id != owner_id {
            return Err(unprocessable(
                "STORE_VISIT_OWNERSHIP_MISMATCH",
                "Store visit does not belong to the order owner (§13.9)",
            ));
        }
        if Some(visit_store) != store_id {
            return Err(unprocessable(
                "STORE_VISIT_STORE_MISMATCH",
                "storeVisit.storeId does not match storeId (§13.9)",
            ));
        }
        store_visit_id = Some(id);
    }
    Ok(Anchor {
        customer_id,
        store_id,
        store_group_id,
        store_visit_id,
    })
}

/// `assertPaymentReference`.
fn assert_payment_reference(requires_reference: bool, reference: Option<&str>) -> ApiResult<()> {
    if requires_reference && reference.is_none_or(str::is_empty) {
        return Err(unprocessable(
            "PAYMENT_REFERENCE_REQUIRED",
            "paymentReference is required for this payment method",
        ));
    }
    Ok(())
}

// ───────────────────────── custom field ─────────────────────────

/// Galat 422 isu pertama + `details.issues`.
pub fn issues_error(issues: &[CfIssue], prefix: Option<&str>) -> ApiError {
    let first = issues.first();
    let message = first.map(|i| i.message.clone()).unwrap_or_default();
    unprocessable_details(
        first.map(|i| i.code.as_str()).unwrap_or_default(),
        match prefix {
            Some(p) => format!("{p}: {message}"),
            None => message,
        },
        json!({ "issues": issues }),
    )
}

/// Node: src/orders/orders.service.ts:410 `assertCustomFieldsValid` (tanpa `CUSTOM_FIELD_REQUIRED`;
/// `written` = kunci yang dikirim, `UNKNOWN_KEY` hanya untuk kunci itu).
async fn assert_custom_fields_valid(
    tx: &mut PgConnection,
    company_id: i32,
    entity: &str,
    values: &Map<String, Value>,
    written: Option<&HashSet<String>>,
) -> ApiResult<()> {
    let defs = definitions(&mut *tx, company_id, entity).await?;
    let issues: Vec<CfIssue> =
        validate_custom_field_values(crate::custom_fields::cf_entity(entity), &defs, values)
            .into_iter()
            .filter(|i| {
                i.code != "CUSTOM_FIELD_REQUIRED"
                    && !(i.code == "CUSTOM_FIELD_UNKNOWN_KEY"
                        && written.is_some_and(|w| !w.contains(&i.key)))
            })
            .collect();
    if issues.is_empty() {
        Ok(())
    } else {
        Err(issues_error(&issues, None))
    }
}

/// Node: src/orders/orders.service.ts:433 `mergeCustomFields` — kunci `null` = dihapus.
pub fn merge_custom_fields(existing: &Value, patch: &Map<String, Value>) -> Map<String, Value> {
    let mut result = existing.as_object().cloned().unwrap_or_default();
    for key in js_keys(patch) {
        if matches!(key.as_str(), "__proto__" | "constructor" | "prototype") {
            continue;
        }
        match patch.get(key.as_str()) {
            Some(Value::Null) | None => {
                result.shift_remove(key.as_str());
            }
            Some(v) => {
                result.insert(key.clone(), v.clone());
            }
        }
    }
    result
}

// ───────────────────────── harga ─────────────────────────

#[derive(sqlx::FromRow)]
struct PriceListRowLite {
    id: i32,
    #[sqlx(rename = "isActive")]
    is_active: bool,
    #[sqlx(rename = "startDate")]
    start_date: Option<NaiveDate>,
    #[sqlx(rename = "endDate")]
    end_date: Option<NaiveDate>,
    #[sqlx(rename = "currencyCode")]
    currency_code: String,
}

impl From<PriceListRowLite> for PriceListSnap {
    fn from(r: PriceListRowLite) -> Self {
        Self {
            id: r.id,
            is_active: r.is_active,
            start_date: r.start_date,
            end_date: r.end_date,
            currency_code: r.currency_code,
        }
    }
}

/// `resolveOrderPriceLists` → `(customer, default)`.
async fn resolve_order_price_lists(
    tx: &mut PgConnection,
    company_id: i32,
    customer_id: i32,
) -> ApiResult<(Option<PriceListSnap>, Option<PriceListSnap>)> {
    let pl_id: Option<Option<i32>> =
        sqlx::query_scalar(r#"SELECT "priceListId" FROM customer WHERE id = $1"#)
            .bind(customer_id)
            .fetch_optional(&mut *tx)
            .await?;
    let pl_id = pl_id.ok_or_else(|| internal("No customer found"))?;
    const COLS: &str = r#"id, "isActive", "startDate", "endDate", "currencyCode""#;
    let customer = match pl_id {
        Some(id) => sqlx::query_as::<_, PriceListRowLite>(&format!(
            r#"SELECT {COLS} FROM "priceList" WHERE id = $1"#
        ))
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .map(Into::into),
        None => None,
    };
    let default = sqlx::query_as::<_, PriceListRowLite>(&format!(
        r#"SELECT {COLS} FROM "priceList" WHERE "companyId" = $1 AND "isDefault" = true AND "isActive" = true ORDER BY id LIMIT 1"#
    ))
    .bind(company_id)
    .fetch_optional(&mut *tx)
    .await?
    .map(Into::into);
    Ok((customer, default))
}

/// `resolveOrderCurrency`: currency list yang berlaku, else `company.currency`.
async fn resolve_order_currency(
    tx: &mut PgConnection,
    company_id: i32,
    customer_id: i32,
    tz: &str,
    company_currency: &str,
) -> ApiResult<String> {
    let (c, d) = resolve_order_price_lists(tx, company_id, customer_id).await?;
    Ok(
        resolve_effective_price_list(c.as_ref(), d.as_ref(), tz, Utc::now())
            .map_or_else(|| company_currency.to_owned(), |p| p.currency_code.clone()),
    )
}

// ───────────────────────── create ─────────────────────────

/// Tulis `order` (header saja) di dalam transaksi pemanggil. `owner_ref` diisi sesegera owner
/// diketahui (untuk 409 `ORDER_CLIENT_REQUEST_CONFLICT`).
#[allow(clippy::too_many_arguments)]
async fn create_draft_in_tx(
    state: &AppState,
    tx: &mut PgConnection,
    audit: &AuditHandle,
    user_id: i32,
    company_id: i32,
    dto: &CreateOrder,
    source: &str,
) -> ApiResult<(OrderRow, String)> {
    let (tz, currency) = company_tz_currency(&mut *tx, company_id).await?;
    assert_order_date_in_range(&dto.order_date, &tz)?;
    let order_type = resolve_master(tx, MasterKind::Types, company_id, &dto.order_type_id).await?;
    let owner_id = match &dto.owner_id {
        Some(o) => resolve_owner(tx, company_id, o).await?,
        None => user_id,
    };
    let anchor = resolve_anchor(
        tx,
        company_id,
        dto.customer_id.as_deref(),
        dto.store_id.as_deref(),
        dto.store_visit_id.as_deref(),
        owner_id,
    )
    .await?;
    let term = resolve_optional(
        tx,
        MasterKind::PaymentTerms,
        company_id,
        dto.order_payment_term_id.as_deref(),
    )
    .await?;
    let method = resolve_optional(
        tx,
        MasterKind::PaymentMethods,
        company_id,
        dto.order_payment_method_id.as_deref(),
    )
    .await?;
    assert_payment_reference(
        method
            .as_ref()
            .and_then(|m| m.requires_reference)
            .unwrap_or(false),
        dto.payment_reference.as_deref(),
    )?;
    if let Some(cf) = dto.custom_fields.as_ref() {
        assert_custom_fields_valid(tx, company_id, "order", cf, None).await?;
    }
    let order_date = crate::common::parse_calendar_date(&dto.order_date)
        .ok_or_else(|| internal("Invalid Date"))?;
    let rdd = dto
        .requested_delivery_date
        .as_deref()
        .and_then(crate::common::parse_calendar_date);
    let currency_code =
        resolve_order_currency(tx, company_id, anchor.customer_id, &tz, &currency).await?;
    let custom_fields = Value::Object(dto.custom_fields.clone().unwrap_or_default());
    let inserted = sqlx::query_scalar::<_, i32>(
        r#"INSERT INTO "order" ("companyId", "orderNumber", "clientRequestId", "orderTypeId", "customerId", "storeId",
              "storeVisitId", "storeGroupId", "ownerId", status, source, "orderDate", "requestedDeliveryDate", note,
              "currencyCode", "orderPaymentTermId", "paymentTermDays", "orderPaymentMethodId", "paymentReference",
              "customFields", "createdById")
           VALUES ($1, $2, $3::uuid, $4, $5, $6, $7, $8, $9, 'draft', $10::"OrderSource", $11, $12, $13, $14, $15, $16,
              $17, $18, $19, $20) RETURNING id"#,
    )
    .bind(company_id)
    .bind(build_draft_order_number())
    .bind(&dto.client_request_id)
    .bind(order_type.id)
    .bind(anchor.customer_id)
    .bind(anchor.store_id)
    .bind(anchor.store_visit_id)
    .bind(anchor.store_group_id)
    .bind(owner_id)
    .bind(source)
    .bind(order_date)
    .bind(rdd)
    .bind(&dto.note)
    .bind(&currency_code)
    .bind(term.as_ref().map(|t| t.id))
    .bind(term.as_ref().and_then(|t| t.due_days))
    .bind(method.as_ref().map(|m| m.id))
    .bind(&dto.payment_reference)
    .bind(&custom_fields)
    .bind(user_id)
    .fetch_one(&mut *tx)
    .await;
    let id = match inserted {
        Ok(id) => id,
        Err(e) => {
            return Err(rethrow_create_conflict(
                state,
                e,
                company_id,
                owner_id,
                &dto.client_request_id,
            )
            .await);
        }
    };
    if let Some(r) = audit
        .result_row(&mut *tx, Model::Order, Lookup::Id(id))
        .await
    {
        audit.record_create(Model::Order, &r).await;
    }
    let row = entity::find_row_by_id(&mut *tx, id).await?;
    Ok((row, tz))
}

/// Node: src/orders/orders.service.ts:745 `rethrowCreateConflict` (P2002 → 409 ber-`code`).
async fn rethrow_create_conflict(
    state: &AppState,
    e: sqlx::Error,
    company_id: i32,
    owner_id: i32,
    client_request_id: &str,
) -> ApiError {
    match crate::common::unique_constraint(&e).as_deref() {
        Some("order_client_request_unique") => {
            let existing: Option<(String, String, String)> = sqlx::query_as(
                r#"SELECT uuid::text, "orderNumber", status::text FROM "order"
                    WHERE "companyId" = $1 AND "ownerId" = $2 AND "clientRequestId" = $3::uuid LIMIT 1"#,
            )
            .bind(company_id)
            .bind(owner_id)
            .bind(client_request_id)
            .fetch_optional(&state.db)
            .await
            .ok()
            .flatten();
            let mut body = json!({
                "code": "ORDER_CLIENT_REQUEST_CONFLICT",
                "message": "clientRequestId already used by another order for this owner",
            });
            if let (Some((uuid, number, status)), Some(o)) = (existing, body.as_object_mut()) {
                o.insert(
                    "details".into(),
                    json!({ "orderId": uuid, "orderNumber": number, "status": status }),
                );
            }
            ApiError::body(axum::http::StatusCode::CONFLICT, body)
        }
        Some("order_number_unique_per_company") => conflict(
            "ORDER_NUMBER_CONFLICT",
            "Generated draft number collided — retry the request",
        ),
        _ => e.into(),
    }
}

/// Node: src/orders/orders.service.ts:652 `create` — header saja (`source` bawaan `web`).
pub async fn create(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    dto: &CreateOrder,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    assert_valid_calendar_date(&dto.order_date, "orderDate")?;
    if let Some(r) = dto
        .requested_delivery_date
        .as_deref()
        .filter(|s| !s.is_empty())
    {
        assert_valid_calendar_date(r, "requestedDeliveryDate")?;
    }
    let mut tx = state.db.begin().await?;
    let (row, tz) =
        create_draft_in_tx(state, &mut tx, audit, user_id, company_id, dto, "web").await?;
    tx.commit().await?;
    Ok(Value::Object(to_order_entity(&row, &tz, None)))
}

/// Node: src/orders/orders.service.ts:681 `createWithItems` — header + item (+ `beforeCommit`) dalam
/// SATU transaksi. Dengan `beforeCommit` respons = `findOne` (header bisa berubah, mis. submit).
pub async fn create_with_items(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    dto: &CreateOrder,
    items: &[ItemInput],
    source: &str,
    before_commit: BeforeCommit<'_>,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    assert_valid_calendar_date(&dto.order_date, "orderDate")?;
    if let Some(r) = dto
        .requested_delivery_date
        .as_deref()
        .filter(|s| !s.is_empty())
    {
        assert_valid_calendar_date(r, "requestedDeliveryDate")?;
    }
    let mut tx = state.db.begin().await?;
    let (created, _) =
        create_draft_in_tx(state, &mut tx, audit, user_id, company_id, dto, source).await?;
    let written =
        write_items_in_tx(&mut tx, audit, company_id, &created, items, &HashSet::new()).await?;
    run_before_commit(&mut tx, audit, company_id, written.0.id, &before_commit).await?;
    tx.commit().await?;
    entity_after_write(state, company_id, written, before_commit.is_some()).await
}

async fn run_before_commit(
    tx: &mut PgConnection,
    audit: &AuditHandle,
    company_id: i32,
    order_id: i32,
    before: &BeforeCommit<'_>,
) -> ApiResult<()> {
    if let BeforeCommit::MobileSubmit { user_id, items } = before {
        crate::mobile::assert_price_tolerance(tx, order_id, company_id, items).await?;
        crate::transitions::submit_in_tx(tx, audit, *user_id, company_id, order_id).await?;
    }
    Ok(())
}

/// `entityAfterWrite`: tanpa `beforeCommit` → entitas + item; dengan → `findOne` (baca ulang).
async fn entity_after_write(
    state: &AppState,
    company_id: i32,
    written: (OrderRow, String),
    reread: bool,
) -> ApiResult<Value> {
    if reread {
        return find_one_in(state, company_id, &written.0.uuid).await;
    }
    let items = items_of(&state.db, written.0.id).await?;
    Ok(Value::Object(to_order_entity(
        &written.0,
        &written.1,
        Some(&items),
    )))
}

// ───────────────────────── read ─────────────────────────

/// `findRowOrThrow`.
pub async fn find_row_or_throw(
    state: &AppState,
    company_id: i32,
    uuid: &str,
) -> ApiResult<OrderRow> {
    entity::find_row(&state.db, company_id, uuid)
        .await?
        .ok_or_else(order_not_found)
}

/// `findOne` dengan company yang sudah diketahui.
pub async fn find_one_in(state: &AppState, company_id: i32, uuid: &str) -> ApiResult<Value> {
    entity::detail(&state.db, company_id, uuid)
        .await?
        .map(|(_, _, v)| v)
        .ok_or_else(order_not_found)
}

/// Node: src/orders/orders.service.ts:765 `findOne` — detail + item + `deliveryHistory`.
pub async fn find_one(state: &AppState, user_id: i32, uuid: &str) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    find_one_in(state, company_id, uuid).await
}

/// Node: src/orders/orders.service.ts:787 `findForInvoice` → `(detail, name, address, timeZone)`.
pub async fn find_for_invoice(
    state: &AppState,
    user_id: i32,
    uuid: &str,
) -> ApiResult<(Value, String, Option<String>, String)> {
    let order = find_one(state, user_id, uuid).await?;
    let status = order
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if status != "approved" && status != "delivered" {
        return Err(conflict(
            "ORDER_NOT_INVOICEABLE",
            "Only an approved or delivered order can be invoiced",
        ));
    }
    let company_id = require_company_id(&state.db, user_id).await?;
    let row: Option<(String, Option<String>, String)> =
        sqlx::query_as(r#"SELECT name, address, "timeZone" FROM company WHERE id = $1"#)
            .bind(company_id)
            .fetch_optional(&state.db)
            .await?;
    let (name, address, tz) = row.ok_or_else(|| internal("No company found"))?;
    Ok((order, name, address, tz))
}

/// Node: src/orders/orders.service.ts:1013 `findOwners`.
pub async fn find_owners(
    state: &AppState,
    user_id: i32,
    search: Option<&str>,
    page: i64,
    limit: i64,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let search = search
        .map(mz_core::jsfmt::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let filter = |qb: &mut QueryBuilder<'_, Postgres>| {
        qb.push(r#" WHERE u.status = 1 AND EXISTS (SELECT 1 FROM "userCompany" uc WHERE uc."userId" = u.id AND uc."companyId" = "#)
            .push_bind(company_id)
            .push(" AND uc.status = 1)");
        if let Some(s) = &search {
            let p = like_pattern(s);
            qb.push(" AND (u.name ILIKE ")
                .push_bind(p.clone())
                .push(" OR u.username ILIKE ")
                .push_bind(p)
                .push(")");
        }
    };
    let mut qb: QueryBuilder<Postgres> =
        QueryBuilder::new("SELECT u.uuid::text, u.name, u.username FROM users u");
    filter(&mut qb);
    qb.push(" ORDER BY u.name ASC, u.id ASC LIMIT ")
        .push_bind(limit)
        .push(" OFFSET ")
        .push_bind(offset(page, limit));
    let rows: Vec<(String, String, String)> = qb.build_query_as().fetch_all(&state.db).await?;
    let mut cq: QueryBuilder<Postgres> = QueryBuilder::new("SELECT COUNT(*) FROM users u");
    filter(&mut cq);
    let total: i64 = cq.build_query_scalar().fetch_one(&state.db).await?;
    Ok(json!({
        "data": rows.iter().map(|(id, name, username)| json!({ "id": id, "name": name, "username": username })).collect::<Vec<_>>(),
        "meta": page_meta(total, page, limit),
    }))
}

/// `where` hasil `buildWhere` (id yang sudah di-resolve + filter mentah).
#[derive(Debug, Clone, Default)]
pub struct Where {
    pub company_id: i32,
    pub filter: OrderFilter,
    pub store_visit_id: Option<i32>,
    pub order_type_id: Option<i32>,
    pub store_group_id: Option<i32>,
    pub cf: Vec<(String, String)>,
}

/// Node: src/orders/orders.service.ts:1043 `buildWhere` — uuid tak dikenal untuk visit/type/region
/// tidak memfilter apa pun (§9.1).
pub async fn build_where(state: &AppState, company_id: i32, f: &OrderFilter) -> ApiResult<Where> {
    async fn lookup(
        state: &AppState,
        table: &str,
        company_id: i32,
        uuid: Option<&str>,
    ) -> ApiResult<Option<i32>> {
        let Some(u) = uuid else { return Ok(None) };
        Ok(sqlx::query_scalar(&format!(
            r#"SELECT id FROM "{table}" WHERE uuid = $1::uuid AND "companyId" = $2 LIMIT 1"#
        ))
        .bind(u)
        .bind(company_id)
        .fetch_optional(&state.db)
        .await?)
    }
    let store_visit_id =
        lookup(state, "storeVisit", company_id, f.store_visit_id.as_deref()).await?;
    let order_type_id = lookup(state, "orderType", company_id, f.order_type_id.as_deref()).await?;
    let store_group_id =
        lookup(state, "storeGroup", company_id, f.store_group_id.as_deref()).await?;
    let mut cf = Vec::new();
    for (key, value) in &f.cf {
        let Some(v) = value else {
            return Err(bad_request(
                "CUSTOM_FIELD_TYPE_MISMATCH",
                format!("cf[{key}] must be a single string value"),
            ));
        };
        cf.push((key.clone(), v.clone()));
    }
    Ok(Where {
        company_id,
        filter: f.clone(),
        store_visit_id,
        order_type_id,
        store_group_id,
        cf,
    })
}

fn parse_date(s: &str) -> ApiResult<NaiveDate> {
    crate::common::parse_calendar_date(s).ok_or_else(|| internal("Invalid Date"))
}

/// Klausa `WHERE` (alias tabel `o`).
pub fn push_where(qb: &mut QueryBuilder<'_, Postgres>, w: &Where) -> ApiResult<()> {
    let f = &w.filter;
    qb.push(r#" WHERE o."companyId" = "#)
        .push_bind(w.company_id);
    if let Some(st) = f.status.as_ref().filter(|s| !s.is_empty()) {
        qb.push(" AND o.status::text = ANY(")
            .push_bind(st.clone())
            .push(")");
    }
    if let Some(c) = &f.customer_id {
        qb.push(r#" AND o."customerId" IN (SELECT id FROM customer WHERE uuid = "#)
            .push_bind(c.clone())
            .push("::uuid)");
    }
    if let Some(s) = &f.store_id {
        qb.push(r#" AND o."storeId" IN (SELECT id FROM store WHERE uuid = "#)
            .push_bind(s.clone())
            .push("::uuid)");
    }
    if let Some(v) = w.store_visit_id {
        qb.push(r#" AND o."storeVisitId" = "#).push_bind(v);
    }
    if let Some(o) = &f.owner_id {
        qb.push(r#" AND o."ownerId" IN (SELECT id FROM users WHERE uuid = "#)
            .push_bind(o.clone())
            .push("::uuid)");
    }
    if let Some(t) = w.order_type_id {
        qb.push(r#" AND o."orderTypeId" = "#).push_bind(t);
    }
    if let Some(g) = w.store_group_id {
        qb.push(r#" AND o."storeGroupId" = "#).push_bind(g);
    }
    if let Some(d) = &f.date_from {
        qb.push(r#" AND o."orderDate" >= "#)
            .push_bind(parse_date(d)?);
    }
    if let Some(d) = &f.date_to {
        qb.push(r#" AND o."orderDate" <= "#)
            .push_bind(parse_date(d)?);
    }
    if let Some(d) = &f.due_before {
        qb.push(r#" AND o."dueDate" <= "#).push_bind(parse_date(d)?);
    }
    if let Some(s) = &f.source {
        qb.push(" AND o.source::text = ").push_bind(s.clone());
    }
    if let Some(q) = f.q.as_deref().filter(|q| !q.is_empty()) {
        let p = like_pattern(q);
        qb.push(r#" AND (o."orderNumber" ILIKE "#)
            .push_bind(p.clone())
            .push(r#" OR EXISTS (SELECT 1 FROM customer qc WHERE qc.id = o."customerId" AND qc.name ILIKE "#)
            .push_bind(p.clone())
            .push(r#") OR EXISTS (SELECT 1 FROM store qs WHERE qs.id = o."storeId" AND qs.code ILIKE "#)
            .push_bind(p.clone())
            .push(r#") OR EXISTS (SELECT 1 FROM store qs WHERE qs.id = o."storeId" AND qs.name ILIKE "#)
            .push_bind(p)
            .push("))");
    }
    for (k, v) in &w.cf {
        qb.push(r#" AND (o."customFields" #> ARRAY["#)
            .push_bind(k.clone())
            .push("]::text[]) = to_jsonb(")
            .push_bind(v.clone())
            .push("::text)");
    }
    Ok(())
}

/// `buildOrderBy` — `[{ sortBy: dir }, { id: dir }]`.
pub fn order_by_sql(sort_by: &str, sort_dir: &str) -> String {
    let col = match sort_by {
        "grandTotal" => r#"o."grandTotal""#,
        "dueDate" => r#"o."dueDate""#,
        "createdAt" => r#"o."createdAt""#,
        _ => r#"o."orderDate""#,
    };
    let dir = if sort_dir == "asc" { "ASC" } else { "DESC" };
    format!(" ORDER BY {col} {dir}, o.id {dir}")
}

/// Node: src/orders/orders.service.ts:1131 `findAll` (status bisa dipaksa pemanggil).
pub async fn find_all(
    state: &AppState,
    user_id: i32,
    q: &crate::dto::ListQuery,
    censored: &CensoredColumns,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    mz_table_fields::search_sort::assert_sortable_column("orders", Some(&q.sort_by), censored)?;
    let w = build_where(state, company_id, &q.filter).await?;
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(entity::ORDER_SELECT);
    push_where(&mut qb, &w)?;
    qb.push(order_by_sql(&q.sort_by, &q.sort_dir))
        .push(" LIMIT ")
        .push_bind(q.limit)
        .push(" OFFSET ")
        .push_bind(offset(q.page, q.limit));
    let rows: Vec<OrderRow> = qb.build_query_as().fetch_all(&state.db).await?;
    let mut cq: QueryBuilder<Postgres> = QueryBuilder::new(r#"SELECT COUNT(*) FROM "order" o"#);
    push_where(&mut cq, &w)?;
    let total: i64 = cq.build_query_scalar().fetch_one(&state.db).await?;
    let tz = crate::common::company_time_zone(&state.db, company_id).await?;
    Ok(json!({
        "data": rows.iter().map(|r| Value::Object(to_order_entity(r, &tz, None))).collect::<Vec<_>>(),
        "meta": page_meta(total, q.page, q.limit),
    }))
}

// ───────────────────────── update header ─────────────────────────

/// Satu `(kolom, nilai SQL, nilai audit)` yang ditulis `updateMany`.
enum Set {
    Int(Option<i32>),
    Text(Option<String>),
    Date(Option<NaiveDate>),
    Json(Value),
}

/// Node: src/orders/orders.service.ts:1271 `update` — PATCH header `draft` (CAS status).
pub async fn update(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    uuid: &str,
    dto: &UpdateOrder,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let existing = find_row_or_throw(state, company_id, uuid).await?;
    if existing.status != "draft" {
        return Err(order_immutable());
    }
    let mut tx = state.db.begin().await?;
    let mut data: Vec<(&'static str, Set)> = Vec::new();
    if dto.order_type_id.is_defined() {
        let Field::Val(t) = &dto.order_type_id else {
            return Err(internal("Argument uuid must not be null"));
        };
        let ot = resolve_master(&mut tx, MasterKind::Types, company_id, t).await?;
        data.push(("orderTypeId", Set::Int(Some(ot.id))));
    }
    let mut new_owner = None;
    if dto.owner_id.is_defined() {
        let Field::Val(o) = &dto.owner_id else {
            return Err(internal("Argument uuid must not be null"));
        };
        let id = resolve_owner(&mut tx, company_id, o).await?;
        new_owner = Some(id);
        data.push(("ownerId", Set::Int(Some(id))));
    }
    let anchor_sent = dto.customer_id.is_defined()
        || dto.store_id.is_defined()
        || dto.store_visit_id.is_defined();
    if let (Some(owner), false, Some(visit_id)) = (new_owner, anchor_sent, existing.store_visit_id)
        && owner != existing.owner_id
    {
        let visit_user: Option<i32> = sqlx::query_scalar(
            r#"SELECT "userId" FROM "storeVisit" WHERE id = $1 AND "companyId" = $2"#,
        )
        .bind(visit_id)
        .bind(company_id)
        .fetch_optional(&mut *tx)
        .await?;
        if visit_user.is_some_and(|u| u != owner) {
            return Err(unprocessable(
                "STORE_VISIT_OWNERSHIP_MISMATCH",
                "Store visit does not belong to the new owner (§13.9)",
            ));
        }
    }
    if anchor_sent {
        let val = |f: &Field<String>| match f {
            Field::Val(v) if !v.is_empty() => Some(v.clone()),
            _ => None,
        };
        let anchor = resolve_anchor(
            &mut tx,
            company_id,
            val(&dto.customer_id).as_deref(),
            val(&dto.store_id).as_deref(),
            val(&dto.store_visit_id).as_deref(),
            new_owner.unwrap_or(existing.owner_id),
        )
        .await?;
        data.push(("customerId", Set::Int(Some(anchor.customer_id))));
        data.push(("storeId", Set::Int(anchor.store_id)));
        data.push(("storeGroupId", Set::Int(anchor.store_group_id)));
        data.push(("storeVisitId", Set::Int(anchor.store_visit_id)));
    }
    let field_str = |f: &Field<String>| match f {
        Field::Val(v) => Some(v.clone()),
        _ => None,
    };
    if dto.order_payment_method_id.is_defined() {
        let pm = resolve_optional(
            &mut tx,
            MasterKind::PaymentMethods,
            company_id,
            field_str(&dto.order_payment_method_id).as_deref(),
        )
        .await?;
        data.push(("orderPaymentMethodId", Set::Int(pm.as_ref().map(|m| m.id))));
        let final_ref = if dto.payment_reference.is_defined() {
            field_str(&dto.payment_reference)
        } else {
            existing.payment_reference.clone()
        };
        assert_payment_reference(
            pm.as_ref()
                .and_then(|m| m.requires_reference)
                .unwrap_or(false),
            final_ref.as_deref(),
        )?;
    } else if dto.payment_reference.is_defined() {
        assert_payment_reference(
            existing.pm_requires_reference.unwrap_or(false),
            field_str(&dto.payment_reference).as_deref(),
        )?;
    }
    if dto.payment_reference.is_defined() {
        data.push((
            "paymentReference",
            Set::Text(field_str(&dto.payment_reference)),
        ));
    }
    if dto.order_payment_term_id.is_defined() {
        let pt = resolve_optional(
            &mut tx,
            MasterKind::PaymentTerms,
            company_id,
            field_str(&dto.order_payment_term_id).as_deref(),
        )
        .await?;
        data.push(("orderPaymentTermId", Set::Int(pt.as_ref().map(|t| t.id))));
        data.push((
            "paymentTermDays",
            Set::Int(pt.as_ref().and_then(|t| t.due_days)),
        ));
    }
    if dto.order_date.is_defined() {
        let raw = field_str(&dto.order_date).unwrap_or_default();
        let date = assert_valid_calendar_date(&raw, "orderDate")?;
        let tz = crate::common::company_time_zone(&mut *tx, company_id).await?;
        assert_order_date_in_range(&raw, &tz)?;
        data.push(("orderDate", Set::Date(Some(date))));
    }
    if dto.requested_delivery_date.is_defined() {
        let raw = field_str(&dto.requested_delivery_date).filter(|s| !s.is_empty());
        let date = match raw {
            Some(r) => Some(assert_valid_calendar_date(&r, "requestedDeliveryDate")?),
            None => None,
        };
        data.push(("requestedDeliveryDate", Set::Date(date)));
    }
    if dto.note.is_defined() {
        data.push(("note", Set::Text(field_str(&dto.note))));
    }
    if dto.custom_fields.is_defined() {
        let Field::Val(patch) = &dto.custom_fields else {
            return Err(internal("Cannot convert undefined or null to object"));
        };
        let merged = merge_custom_fields(&existing.custom_fields, patch);
        let written: HashSet<String> = patch.keys().cloned().collect();
        assert_custom_fields_valid(&mut tx, company_id, "order", &merged, Some(&written)).await?;
        data.push(("customFields", Set::Json(Value::Object(merged))));
    }

    let now = now_naive();
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(r#"UPDATE "order" SET "updatedAt" = "#);
    qb.push_bind(now);
    let mut audit_data = Row::new().with("updatedAt", json!(JsDate(now)));
    for (col, v) in &data {
        qb.push(format!(r#", "{col}" = "#));
        match v {
            Set::Int(i) => {
                qb.push_bind(*i);
                audit_data.set(col, json!(i));
            }
            Set::Text(t) => {
                qb.push_bind(t.clone());
                audit_data.set(col, json!(t));
            }
            Set::Date(d) => {
                qb.push_bind(*d);
                audit_data.set(col, json!(d.map(|d| JsDate(js_date(d)))));
            }
            Set::Json(j) => {
                qb.push_bind(j.clone());
                audit_data.set(col, j.clone());
            }
        }
    }
    qb.push(" WHERE id = ")
        .push_bind(existing.id)
        .push(" AND status = 'draft'");
    let order_id = existing.id;
    let pre = audit
        .pre_read_many(
            Model::Order,
            Lookup::filter(move |qb| {
                qb.push("id = ")
                    .push_bind(order_id)
                    .push(" AND status = 'draft'");
            }),
        )
        .await;
    let n = qb.build().execute(&mut *tx).await?.rows_affected();
    audit
        .record_update_many(
            Model::Order,
            pre,
            n,
            &audit_data,
            json!({ "id": order_id, "status": "draft" }),
        )
        .await;
    if n == 0 {
        return Err(order_immutable());
    }
    let updated = entity::find_row_by_id(&mut *tx, existing.id).await?;
    tx.commit().await?;
    let tz = crate::common::company_time_zone(&state.db, company_id).await?;
    Ok(Value::Object(to_order_entity(&updated, &tz, None)))
}

// ───────────────────────── replace items ─────────────────────────

/// Node: src/orders/orders.service.ts:1488 `replaceItems` — REPLACE penuh (draft saja).
#[allow(clippy::too_many_arguments)]
pub async fn replace_items(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    uuid: &str,
    items: &[ItemInput],
    censored_item_columns: &HashSet<&str>,
    before_commit: BeforeCommit<'_>,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let existing = find_row_or_throw(state, company_id, uuid).await?;
    if existing.status != "draft" {
        return Err(order_immutable());
    }
    let mut tx = state.db.begin().await?;
    let written = write_items_in_tx(
        &mut tx,
        audit,
        company_id,
        &existing,
        items,
        censored_item_columns,
    )
    .await?;
    run_before_commit(&mut tx, audit, company_id, written.0.id, &before_commit).await?;
    tx.commit().await?;
    entity_after_write(state, company_id, written, before_commit.is_some()).await
}

#[derive(sqlx::FromRow)]
struct ProductLite {
    id: i32,
    uuid: String,
    sku: String,
    name: String,
    #[sqlx(rename = "isActive")]
    is_active: bool,
    #[sqlx(rename = "isOrderable")]
    is_orderable: bool,
    #[sqlx(rename = "baseUomId")]
    base_uom_id: i32,
    #[sqlx(rename = "taxRateId")]
    tax_rate_id: Option<i32>,
    tax_rate: Option<Decimal>,
    tax_inclusive: Option<bool>,
}

#[derive(sqlx::FromRow)]
struct UomLite {
    id: i32,
    uuid: String,
    code: String,
}

#[derive(sqlx::FromRow)]
struct ProductUomLite {
    #[sqlx(rename = "productId")]
    product_id: i32,
    #[sqlx(rename = "uomId")]
    uom_id: i32,
    operator: String,
    factor: Decimal,
    #[sqlx(rename = "conversionRate")]
    conversion_rate: Decimal,
    #[sqlx(rename = "isSalesUom")]
    is_sales_uom: bool,
}

#[derive(sqlx::FromRow)]
struct PriceItemLite {
    id: i32,
    #[sqlx(rename = "priceListId")]
    price_list_id: i32,
    #[sqlx(rename = "productId")]
    product_id: i32,
    #[sqlx(rename = "uomId")]
    uom_id: i32,
    #[sqlx(rename = "minQty")]
    min_qty: Decimal,
    #[sqlx(rename = "maxQty")]
    max_qty: Option<Decimal>,
    price: Decimal,
}

/// Satu baris `orderItem` yang akan ditulis.
struct NewItem {
    line_no: i32,
    product_id: i32,
    uom_id: i32,
    product_sku: String,
    product_name: String,
    uom_code: String,
    operator: String,
    factor: Decimal,
    conversion_rate: Decimal,
    price_list_item_id: Option<i32>,
    list_unit_price: Option<String>,
    unit_price: String,
    qty: String,
    discount_percent: String,
    discount_amount: String,
    is_free_good: bool,
    tax_rate_id: Option<i32>,
    tax_rate: String,
    tax_is_inclusive: bool,
    line: pricing::LineResult,
    note: Option<String>,
    custom_fields: Value,
}

fn uuids_of(items: &[ItemInput], pick: impl Fn(&ItemInput) -> &str) -> Vec<uuid::Uuid> {
    let mut seen = HashSet::new();
    items
        .iter()
        .filter_map(|i| uuid::Uuid::parse_str(pick(i)).ok())
        .filter(|u| seen.insert(*u))
        .collect()
}

/// Node: src/orders/orders.service.ts:1537 `writeItemsInTx` — validasi + harga + REPLACE + total
/// header (CAS `status = 'draft'`). Kembalikan baris header terbaru + `company.timeZone`.
pub async fn write_items_in_tx(
    tx: &mut PgConnection,
    audit: &AuditHandle,
    company_id: i32,
    existing: &OrderRow,
    items: &[ItemInput],
    censored_item_columns: &HashSet<&str>,
) -> ApiResult<(OrderRow, String)> {
    let order_type =
        masters::find_by_id(&mut *tx, MasterKind::Types, existing.order_type_id).await?;
    let allow_discount = order_type.allow_discount.unwrap_or(true);
    let allow_free = order_type.allow_free_item.unwrap_or(false);

    let products: Vec<ProductLite> = sqlx::query_as(
        r#"SELECT p.id, p.uuid::text AS uuid, p.sku, p.name, p."isActive", p."isOrderable", p."baseUomId",
                  p."taxRateId", t.rate AS tax_rate, t."isInclusive" AS tax_inclusive
             FROM product p LEFT JOIN "taxRate" t ON t.id = p."taxRateId"
            WHERE p."companyId" = $1 AND p.uuid = ANY($2) AND p."deletedAt" IS NULL"#,
    )
    .bind(company_id)
    .bind(uuids_of(items, |i| &i.product_id))
    .fetch_all(&mut *tx)
    .await?;
    let uoms: Vec<UomLite> = sqlx::query_as(
        r#"SELECT id, uuid::text AS uuid, code FROM uom WHERE "companyId" = $1 AND uuid = ANY($2)"#,
    )
    .bind(company_id)
    .bind(uuids_of(items, |i| &i.uom_id))
    .fetch_all(&mut *tx)
    .await?;
    let product_ids: Vec<i32> = products.iter().map(|p| p.id).collect();
    let uom_ids: Vec<i32> = uoms.iter().map(|u| u.id).collect();
    let product_uoms: Vec<ProductUomLite> = if product_ids.is_empty() || uom_ids.is_empty() {
        Vec::new()
    } else {
        sqlx::query_as(
            r#"SELECT "productId", "uomId", operator::text AS operator, factor, "conversionRate", "isSalesUom"
                 FROM "productUom" WHERE "productId" = ANY($1) AND "uomId" = ANY($2) ORDER BY id"#,
        )
        .bind(&product_ids)
        .bind(&uom_ids)
        .fetch_all(&mut *tx)
        .await?
    };
    let product_by_uuid: HashMap<&str, &ProductLite> =
        products.iter().map(|p| (p.uuid.as_str(), p)).collect();
    let uom_by_uuid: HashMap<&str, &UomLite> = uoms.iter().map(|u| (u.uuid.as_str(), u)).collect();
    let pu_by_pair: HashMap<(i32, i32), &ProductUomLite> = product_uoms
        .iter()
        .map(|r| ((r.product_id, r.uom_id), r))
        .collect();

    // `resolveItemCandidates`.
    let mut candidates: Vec<(&ItemInput, &ProductLite, &UomLite, &ProductUomLite)> = Vec::new();
    for (index, input) in items.iter().enumerate() {
        let n = index + 1;
        let product = product_by_uuid
            .get(input.product_id.as_str())
            .copied()
            .ok_or_else(|| {
                not_found(
                    "PRODUCT_NOT_FOUND",
                    format!("Product not found for item {n}"),
                )
            })?;
        if !product.is_active || !product.is_orderable {
            return Err(unprocessable(
                "PRODUCT_NOT_ORDERABLE",
                format!("Product is not orderable for item {n}"),
            ));
        }
        let uom = uom_by_uuid
            .get(input.uom_id.as_str())
            .copied()
            .ok_or_else(|| not_found("PRODUCT_NOT_FOUND", format!("UOM not found for item {n}")))?;
        let pu = pu_by_pair
            .get(&(product.id, uom.id))
            .copied()
            .filter(|pu| pu.is_sales_uom)
            .ok_or_else(|| {
                unprocessable(
                    "UOM_NOT_FOR_PRODUCT",
                    format!("uomId is not a sales UOM of the product for item {n}"),
                )
            })?;
        candidates.push((input, product, uom, pu));
    }

    let censored: HashSet<&str> = pricing::CENSORABLE_DISCOUNT_COLUMNS
        .into_iter()
        .filter(|c| censored_item_columns.contains(c))
        .collect();
    let stored: Vec<StoredItemDiscount> = if censored.is_empty() {
        Vec::new()
    } else {
        let rows: Vec<(i32, i32, i32, bool, Decimal, Decimal)> = sqlx::query_as(
            r#"SELECT "lineNo", "productId", "uomId", "isFreeGood", "discountPercent", "discountAmount"
                 FROM "orderItem" WHERE "orderId" = $1 ORDER BY id"#,
        )
        .bind(existing.id)
        .fetch_all(&mut *tx)
        .await?;
        rows.into_iter()
            .map(
                |(line_no, product_id, uom_id, is_free_good, p, a)| StoredItemDiscount {
                    line_no,
                    product_id,
                    uom_id,
                    is_free_good,
                    discount_percent: dstr(p),
                    discount_amount: dstr(a),
                },
            )
            .collect()
    };
    let carried: Vec<CarriedDiscount> = carry_censored_discounts(
        &candidates
            .iter()
            .map(|(i, p, u, _)| IncomingItemKey {
                product_id: p.id,
                uom_id: u.id,
                is_free_good: i.is_free_good.unwrap_or(false),
            })
            .collect::<Vec<_>>(),
        &stored,
        &censored,
        allow_discount,
    );

    let (customer_pl, default_pl) =
        resolve_order_price_lists(tx, company_id, existing.customer_id).await?;
    let (tz, company_currency) = company_tz_currency(&mut *tx, company_id).await?;
    let now = Utc::now();
    let effective =
        resolve_effective_price_list(customer_pl.as_ref(), default_pl.as_ref(), &tz, now).cloned();

    let price_list_ids: Vec<i32> = [
        customer_pl.as_ref().map(|p| p.id),
        default_pl.as_ref().map(|p| p.id),
    ]
    .into_iter()
    .flatten()
    .collect();
    let mut uom_and_base: Vec<i32> = uom_ids.clone();
    for p in &products {
        if !uom_and_base.contains(&p.base_uom_id) {
            uom_and_base.push(p.base_uom_id);
        }
    }
    let mut price_items: HashMap<(i32, i32), Vec<PriceItemCandidate>> = HashMap::new();
    if !price_list_ids.is_empty() && !product_ids.is_empty() && !uom_and_base.is_empty() {
        let rows: Vec<PriceItemLite> = sqlx::query_as(
            r#"SELECT id, "priceListId", "productId", "uomId", "minQty", "maxQty", price FROM "priceListItem"
                WHERE "companyId" = $1 AND "priceListId" = ANY($2) AND "productId" = ANY($3) AND "uomId" = ANY($4)
                  AND "deletedAt" IS NULL ORDER BY id"#,
        )
        .bind(company_id)
        .bind(&price_list_ids)
        .bind(&product_ids)
        .bind(&uom_and_base)
        .fetch_all(&mut *tx)
        .await?;
        for r in rows {
            price_items
                .entry((r.product_id, r.uom_id))
                .or_default()
                .push(PriceItemCandidate {
                    id: r.id,
                    price_list_id: r.price_list_id,
                    min_qty: dstr(r.min_qty),
                    max_qty: r.max_qty.map(dstr),
                    price: dstr(r.price),
                });
        }
    }
    let item_defs: Vec<CfDefinition> = definitions(&mut *tx, company_id, "orderItem").await?;

    let mut rows: Vec<NewItem> = Vec::with_capacity(candidates.len());
    for (index, ((input, product, uom, pu), carried)) in
        candidates.iter().zip(carried.iter()).enumerate()
    {
        let n = index + 1;
        if dec(&input.qty) <= Decimal::ZERO {
            return Err(unprocessable(
                "ITEM_QTY_INVALID",
                format!("qty must be greater than zero for item {n}"),
            ));
        }
        let is_free = input.is_free_good.unwrap_or(false);
        let discount_percent = carried
            .discount_percent
            .clone()
            .or_else(|| input.discount_percent.clone())
            .unwrap_or_else(|| "0".into());
        let discount_amount = carried
            .discount_amount
            .clone()
            .or_else(|| input.discount_amount.clone())
            .unwrap_or_else(|| "0".into());
        let has_discount = !dec(&discount_percent).is_zero() || !dec(&discount_amount).is_zero();
        if has_discount && !allow_discount {
            return Err(unprocessable(
                "DISCOUNT_NOT_ALLOWED",
                format!("Discount is not allowed for this order type (item {n})"),
            ));
        }
        if is_free && !allow_free {
            return Err(unprocessable(
                "FREE_ITEM_NOT_ALLOWED",
                format!("Free goods are not allowed for this order type (item {n})"),
            ));
        }
        let rate_str = dstr(pu.conversion_rate);
        let (mut unit_price, mut list_unit_price, mut pli_id) = ("0".to_owned(), None, None);
        if !is_free {
            let empty = Vec::new();
            let uom_items = price_items.get(&(product.id, uom.id)).unwrap_or(&empty);
            let base_items = (uom.id != product.base_uom_id).then(|| {
                price_items
                    .get(&(product.id, product.base_uom_id))
                    .map_or(&[][..], Vec::as_slice)
            });
            let found = resolve_uom_price(
                customer_pl.as_ref(),
                default_pl.as_ref(),
                uom_items,
                base_items,
                &input.qty,
                &rate_str,
                &tz,
                now,
            )
            .ok_or_else(|| {
                unprocessable("PRICE_NOT_FOUND", format!("No price found for item {n}"))
            })?;
            unit_price = found.unit_price.clone();
            list_unit_price = Some(found.unit_price);
            pli_id = Some(found.price_list_item_id);
        }
        if let Some(cf) = &input.custom_fields {
            let issues: Vec<CfIssue> =
                validate_custom_field_values(CfEntity::OrderItem, &item_defs, cf)
                    .into_iter()
                    .filter(|i| i.code != "CUSTOM_FIELD_REQUIRED")
                    .collect();
            if !issues.is_empty() {
                return Err(issues_error(&issues, Some(&format!("item {n}"))));
            }
        }
        let tax_rate = product.tax_rate.map_or_else(|| "0".to_owned(), dstr);
        let tax_inclusive = product.tax_inclusive.unwrap_or(false);
        let line = compute_line(&LineInput {
            qty: input.qty.clone(),
            unit_price: unit_price.clone(),
            conversion_rate: rate_str,
            discount_percent: discount_percent.clone(),
            discount_amount: discount_amount.clone(),
            tax_rate: tax_rate.clone(),
            tax_is_inclusive: tax_inclusive,
        })
        .map_err(|_| {
            unprocessable(
                "DISCOUNT_EXCEEDS_LINE",
                format!("discountValue exceeds grossAmount for item {n}"),
            )
        })?;
        rows.push(NewItem {
            line_no: i32::try_from(n).unwrap_or(i32::MAX),
            product_id: product.id,
            uom_id: uom.id,
            product_sku: product.sku.clone(),
            product_name: product.name.clone(),
            uom_code: uom.code.clone(),
            operator: pu.operator.clone(),
            factor: pu.factor,
            conversion_rate: pu.conversion_rate,
            price_list_item_id: pli_id,
            list_unit_price,
            unit_price,
            qty: input.qty.clone(),
            discount_percent,
            discount_amount,
            is_free_good: is_free,
            tax_rate_id: product.tax_rate_id,
            tax_rate,
            tax_is_inclusive: tax_inclusive,
            line,
            note: input.note.clone(),
            custom_fields: Value::Object(input.custom_fields.clone().unwrap_or_default()),
        });
    }
    let totals = compute_totals(&rows.iter().map(|r| r.line.clone()).collect::<Vec<_>>());

    sqlx::query(r#"DELETE FROM "orderItem" WHERE "orderId" = $1"#)
        .bind(existing.id)
        .execute(&mut *tx)
        .await?;
    if !rows.is_empty() {
        let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
            r#"INSERT INTO "orderItem" ("companyId", "orderId", "lineNo", "productId", "uomId", "productSku",
                "productName", "uomCode", operator, factor, "conversionRate", "priceListItemId", "listUnitPrice",
                "unitPrice", qty, "qtyInBaseUom", "discountPercent", "discountAmount", "isFreeGood", "taxRateId",
                "taxRate", "taxIsInclusive", "grossAmount", "discountValue", "netAmount", "taxAmount", "lineTotal",
                note, "customFields") "#,
        );
        qb.push_values(&rows, |mut b, r| {
            b.push_bind(company_id)
                .push_bind(existing.id)
                .push_bind(r.line_no)
                .push_bind(r.product_id)
                .push_bind(r.uom_id)
                .push_bind(r.product_sku.clone())
                .push_bind(r.product_name.clone())
                .push_bind(r.uom_code.clone())
                .push_bind(r.operator.clone())
                .push_unseparated(r#"::"UomOperator""#)
                .push_bind(r.factor)
                .push_bind(r.conversion_rate)
                .push_bind(r.price_list_item_id)
                .push_bind(r.list_unit_price.as_deref().map(dec))
                .push_bind(dec(&r.unit_price))
                .push_bind(dec(&r.qty))
                .push_bind(dec(&r.line.qty_in_base_uom))
                .push_bind(dec(&r.discount_percent))
                .push_bind(dec(&r.discount_amount))
                .push_bind(r.is_free_good)
                .push_bind(r.tax_rate_id)
                .push_bind(dec(&r.tax_rate))
                .push_bind(r.tax_is_inclusive)
                .push_bind(dec(&r.line.gross_amount))
                .push_bind(dec(&r.line.discount_value))
                .push_bind(dec(&r.line.net_amount))
                .push_bind(dec(&r.line.tax_amount))
                .push_bind(dec(&r.line.line_total))
                .push_bind(r.note.clone())
                .push_bind(r.custom_fields.clone());
        });
        qb.build().execute(&mut *tx).await?;
    }

    let price_list_id = effective.as_ref().map(|p| p.id);
    let currency = effective
        .as_ref()
        .map_or(company_currency, |p| p.currency_code.clone());
    let now_n = now_naive();
    let order_id = existing.id;
    let pre = audit
        .pre_read_many(
            Model::Order,
            Lookup::filter(move |qb| {
                qb.push("id = ")
                    .push_bind(order_id)
                    .push(" AND status = 'draft'");
            }),
        )
        .await;
    let n = sqlx::query(
        r#"UPDATE "order" SET subtotal = $2, "discountTotal" = $3, "taxTotal" = $4, "grandTotal" = $5,
              "itemCount" = $6, "priceListId" = $7, "currencyCode" = $8, "updatedAt" = $9
            WHERE id = $1 AND status = 'draft'"#,
    )
    .bind(order_id)
    .bind(dec(&totals.subtotal))
    .bind(dec(&totals.discount_total))
    .bind(dec(&totals.tax_total))
    .bind(dec(&totals.grand_total))
    .bind(totals.item_count)
    .bind(price_list_id)
    .bind(&currency)
    .bind(now_n)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    audit
        .record_update_many(
            Model::Order,
            pre,
            n,
            &Row::new()
                .with("subtotal", json!(totals.subtotal))
                .with("discountTotal", json!(totals.discount_total))
                .with("taxTotal", json!(totals.tax_total))
                .with("grandTotal", json!(totals.grand_total))
                .with("itemCount", json!(totals.item_count))
                .with("priceListId", json!(price_list_id))
                .with("currencyCode", json!(currency))
                .with("updatedAt", json!(JsDate(now_n))),
            json!({ "id": order_id, "status": "draft" }),
        )
        .await;
    if n == 0 {
        return Err(order_immutable());
    }
    let row = entity::find_row_by_id(&mut *tx, existing.id).await?;
    Ok((row, tz))
}

/// `date_str` re-export untuk modul lain.
pub fn date_only(d: NaiveDate) -> String {
    date_str(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draft_number_shape() {
        let n = build_draft_order_number();
        assert_eq!(n.len(), 30);
        assert!(n.starts_with("DRAFT-"));
        assert_ne!(n, build_draft_order_number());
    }

    #[test]
    fn merge_semantics() {
        let existing = json!({"a": 1, "b": 2});
        let patch = json!({"b": null, "c": 3, "__proto__": 9})
            .as_object()
            .unwrap()
            .clone();
        let merged = merge_custom_fields(&existing, &patch);
        assert_eq!(Value::Object(merged), json!({"a": 1, "c": 3}));
    }

    #[test]
    fn order_by() {
        assert_eq!(
            order_by_sql("grandTotal", "asc"),
            r#" ORDER BY o."grandTotal" ASC, o.id ASC"#
        );
        assert_eq!(
            order_by_sql("orderDate", "desc"),
            r#" ORDER BY o."orderDate" DESC, o.id DESC"#
        );
    }

    #[test]
    fn payment_reference_rule() {
        assert!(assert_payment_reference(true, None).is_err());
        assert!(assert_payment_reference(true, Some("")).is_err());
        assert!(assert_payment_reference(true, Some("G-1")).is_ok());
        assert!(assert_payment_reference(false, None).is_ok());
    }
}
