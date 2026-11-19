//! Serializer order (`OrdersService.toOrderEntity` / `toItemEntity` / `withUuidRefs` /
//! `findDeliveryHistory`) dan pembaca baris dengan relasi (`ORDER_DETAIL_INCLUDE`,
//! `ORDER_ITEM_DETAIL_INCLUDE`). Satu serializer untuk semua jalur (admin, mobile, delivery).

use std::collections::HashMap;

use chrono::{NaiveDate, NaiveDateTime};
use mz_core::ApiResult;
use rust_decimal::Decimal;
use serde_json::{Map, Value, json};
use sqlx::{PgExecutor, Postgres, QueryBuilder};

use crate::common::{date_str, ms, ms_opt};
use crate::pricing::{compute_due_date, compute_effective_discount_percent, dstr, sub};

/// Kolom `order` + relasi `ORDER_DETAIL_INCLUDE` + uuid `storeVisit`/`storeGroup`/`priceList`.
pub const ORDER_SELECT: &str = r#"SELECT o.id, o.uuid::text AS uuid, o."companyId", o."orderNumber",
    o."clientRequestId"::text AS "clientRequestId", o."orderTypeId", o."customerId", o."storeId",
    o."storeVisitId", o."storeGroupId", o."ownerId", o.status::text AS status, o.source::text AS source,
    o."orderDate", o."requestedDeliveryDate", o.note, o."currencyCode", o."priceListId",
    o."orderPaymentTermId", o."paymentTermDays", o."dueDate", o."orderPaymentMethodId", o."paymentReference",
    o.subtotal, o."discountTotal", o."taxTotal", o."grandTotal", o."itemCount",
    o."submittedAt", o."approvedAt", o."rejectedAt", o."rejectNote", o."cancelledAt", o."cancelNote",
    o."deliveredAt", o."deliveryRecordedAt", o."deliveryRevision",
    o."finalSubtotal", o."finalDiscountTotal", o."finalTaxTotal", o."finalGrandTotal",
    o."customFields", o."createdAt", o."updatedAt",
    ot.uuid::text AS ot_uuid, ot.code AS ot_code, ot.name AS ot_name,
    c.uuid::text AS c_uuid, c.code AS c_code, c.name AS c_name,
    s.uuid::text AS s_uuid, s.code AS s_code, s.name AS s_name,
    pt.uuid::text AS pt_uuid, pt.code AS pt_code, pt.name AS pt_name,
    pm.uuid::text AS pm_uuid, pm.code AS pm_code, pm.name AS pm_name, pm."requiresReference" AS pm_requires_reference,
    ow.uuid::text AS ow_uuid, ow.name AS ow_name,
    cb.uuid::text AS cb_uuid, cb.name AS cb_name,
    sb.uuid::text AS sb_uuid, sb.name AS sb_name,
    ab.uuid::text AS ab_uuid, ab.name AS ab_name,
    rb.uuid::text AS rb_uuid, rb.name AS rb_name,
    xb.uuid::text AS xb_uuid, xb.name AS xb_name,
    cr.uuid::text AS cr_uuid, cr.code AS cr_code, cr.name AS cr_name,
    db.uuid::text AS db_uuid, db.name AS db_name,
    sv.uuid::text AS sv_uuid, sg.uuid::text AS sg_uuid, pl.uuid::text AS pl_uuid
  FROM "order" o
  LEFT JOIN "orderType" ot ON ot.id = o."orderTypeId"
  LEFT JOIN customer c ON c.id = o."customerId"
  LEFT JOIN store s ON s.id = o."storeId"
  LEFT JOIN "orderPaymentTerm" pt ON pt.id = o."orderPaymentTermId"
  LEFT JOIN "orderPaymentMethod" pm ON pm.id = o."orderPaymentMethodId"
  LEFT JOIN users ow ON ow.id = o."ownerId"
  LEFT JOIN users cb ON cb.id = o."createdById"
  LEFT JOIN users sb ON sb.id = o."submittedById"
  LEFT JOIN users ab ON ab.id = o."approvedById"
  LEFT JOIN users rb ON rb.id = o."rejectedById"
  LEFT JOIN users xb ON xb.id = o."cancelledById"
  LEFT JOIN "orderReason" cr ON cr.id = o."cancelReasonId"
  LEFT JOIN users db ON db.id = o."deliveredById"
  LEFT JOIN "storeVisit" sv ON sv.id = o."storeVisitId"
  LEFT JOIN "storeGroup" sg ON sg.id = o."storeGroupId"
  LEFT JOIN "priceList" pl ON pl.id = o."priceListId""#;

/// Baris `order` + relasi (`OrderWithRefs`).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct OrderRow {
    pub id: i32,
    pub uuid: String,
    #[sqlx(rename = "companyId")]
    pub company_id: i32,
    #[sqlx(rename = "orderNumber")]
    pub order_number: String,
    #[sqlx(rename = "clientRequestId")]
    pub client_request_id: String,
    #[sqlx(rename = "orderTypeId")]
    pub order_type_id: i32,
    #[sqlx(rename = "customerId")]
    pub customer_id: i32,
    #[sqlx(rename = "storeId")]
    pub store_id: Option<i32>,
    #[sqlx(rename = "storeVisitId")]
    pub store_visit_id: Option<i32>,
    #[sqlx(rename = "storeGroupId")]
    pub store_group_id: Option<i32>,
    #[sqlx(rename = "ownerId")]
    pub owner_id: i32,
    pub status: String,
    pub source: String,
    #[sqlx(rename = "orderDate")]
    pub order_date: NaiveDate,
    #[sqlx(rename = "requestedDeliveryDate")]
    pub requested_delivery_date: Option<NaiveDate>,
    pub note: Option<String>,
    #[sqlx(rename = "currencyCode")]
    pub currency_code: String,
    #[sqlx(rename = "priceListId")]
    pub price_list_id: Option<i32>,
    #[sqlx(rename = "orderPaymentTermId")]
    pub order_payment_term_id: Option<i32>,
    #[sqlx(rename = "paymentTermDays")]
    pub payment_term_days: Option<i32>,
    #[sqlx(rename = "dueDate")]
    pub due_date: Option<NaiveDate>,
    #[sqlx(rename = "orderPaymentMethodId")]
    pub order_payment_method_id: Option<i32>,
    #[sqlx(rename = "paymentReference")]
    pub payment_reference: Option<String>,
    pub subtotal: Decimal,
    #[sqlx(rename = "discountTotal")]
    pub discount_total: Decimal,
    #[sqlx(rename = "taxTotal")]
    pub tax_total: Decimal,
    #[sqlx(rename = "grandTotal")]
    pub grand_total: Decimal,
    #[sqlx(rename = "itemCount")]
    pub item_count: i32,
    #[sqlx(rename = "submittedAt")]
    pub submitted_at: Option<NaiveDateTime>,
    #[sqlx(rename = "approvedAt")]
    pub approved_at: Option<NaiveDateTime>,
    #[sqlx(rename = "rejectedAt")]
    pub rejected_at: Option<NaiveDateTime>,
    #[sqlx(rename = "rejectNote")]
    pub reject_note: Option<String>,
    #[sqlx(rename = "cancelledAt")]
    pub cancelled_at: Option<NaiveDateTime>,
    #[sqlx(rename = "cancelNote")]
    pub cancel_note: Option<String>,
    #[sqlx(rename = "deliveredAt")]
    pub delivered_at: Option<NaiveDateTime>,
    #[sqlx(rename = "deliveryRecordedAt")]
    pub delivery_recorded_at: Option<NaiveDateTime>,
    #[sqlx(rename = "deliveryRevision")]
    pub delivery_revision: i32,
    #[sqlx(rename = "finalSubtotal")]
    pub final_subtotal: Option<Decimal>,
    #[sqlx(rename = "finalDiscountTotal")]
    pub final_discount_total: Option<Decimal>,
    #[sqlx(rename = "finalTaxTotal")]
    pub final_tax_total: Option<Decimal>,
    #[sqlx(rename = "finalGrandTotal")]
    pub final_grand_total: Option<Decimal>,
    #[sqlx(rename = "customFields")]
    pub custom_fields: Value,
    #[sqlx(rename = "createdAt")]
    pub created_at: NaiveDateTime,
    #[sqlx(rename = "updatedAt")]
    pub updated_at: Option<NaiveDateTime>,
    pub ot_uuid: Option<String>,
    pub ot_code: Option<String>,
    pub ot_name: Option<String>,
    pub c_uuid: Option<String>,
    pub c_code: Option<String>,
    pub c_name: Option<String>,
    pub s_uuid: Option<String>,
    pub s_code: Option<String>,
    pub s_name: Option<String>,
    pub pt_uuid: Option<String>,
    pub pt_code: Option<String>,
    pub pt_name: Option<String>,
    pub pm_uuid: Option<String>,
    pub pm_code: Option<String>,
    pub pm_name: Option<String>,
    pub pm_requires_reference: Option<bool>,
    pub ow_uuid: Option<String>,
    pub ow_name: Option<String>,
    pub cb_uuid: Option<String>,
    pub cb_name: Option<String>,
    pub sb_uuid: Option<String>,
    pub sb_name: Option<String>,
    pub ab_uuid: Option<String>,
    pub ab_name: Option<String>,
    pub rb_uuid: Option<String>,
    pub rb_name: Option<String>,
    pub xb_uuid: Option<String>,
    pub xb_name: Option<String>,
    pub cr_uuid: Option<String>,
    pub cr_code: Option<String>,
    pub cr_name: Option<String>,
    pub db_uuid: Option<String>,
    pub db_name: Option<String>,
    pub sv_uuid: Option<String>,
    pub sg_uuid: Option<String>,
    pub pl_uuid: Option<String>,
}

/// Kolom `orderItem` + relasi `ORDER_ITEM_DETAIL_INCLUDE`.
pub const ITEM_SELECT: &str = r#"SELECT i.id, i.uuid::text AS uuid, i."orderId", i."lineNo",
    i."productId", i."uomId", p.uuid::text AS product_uuid, i."productSku", i."productName",
    u.uuid::text AS uom_uuid, i."uomCode", i.operator::text AS operator, i.factor, i."conversionRate",
    pli.uuid::text AS pli_uuid, i."listUnitPrice", i."unitPrice", i.qty, i."qtyInBaseUom",
    i."discountPercent", i."discountAmount", i."isFreeGood", tr.uuid::text AS tr_uuid, i."taxRate",
    i."taxIsInclusive", i."grossAmount", i."discountValue", i."netAmount", i."taxAmount", i."lineTotal",
    i."deliveredQty", i."deliveredQtyInBaseUom", i."finalGrossAmount", i."finalDiscountValue",
    i."finalNetAmount", i."finalTaxAmount", i."finalLineTotal", i."deliveryReasonId",
    dr.uuid::text AS dr_uuid, dr.code AS dr_code, dr.name AS dr_name, i."deliveryNote", i.note, i."customFields"
  FROM "orderItem" i
  JOIN product p ON p.id = i."productId"
  JOIN uom u ON u.id = i."uomId"
  LEFT JOIN "priceListItem" pli ON pli.id = i."priceListItemId"
  LEFT JOIN "taxRate" tr ON tr.id = i."taxRateId"
  LEFT JOIN "orderReason" dr ON dr.id = i."deliveryReasonId""#;

/// Baris `orderItem` + relasi.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ItemRow {
    pub id: i32,
    pub uuid: String,
    #[sqlx(rename = "orderId")]
    pub order_id: i32,
    #[sqlx(rename = "lineNo")]
    pub line_no: i32,
    #[sqlx(rename = "productId")]
    pub product_id: i32,
    #[sqlx(rename = "uomId")]
    pub uom_id: i32,
    pub product_uuid: String,
    #[sqlx(rename = "productSku")]
    pub product_sku: String,
    #[sqlx(rename = "productName")]
    pub product_name: String,
    pub uom_uuid: String,
    #[sqlx(rename = "uomCode")]
    pub uom_code: String,
    pub operator: String,
    pub factor: Decimal,
    #[sqlx(rename = "conversionRate")]
    pub conversion_rate: Decimal,
    pub pli_uuid: Option<String>,
    #[sqlx(rename = "listUnitPrice")]
    pub list_unit_price: Option<Decimal>,
    #[sqlx(rename = "unitPrice")]
    pub unit_price: Decimal,
    pub qty: Decimal,
    #[sqlx(rename = "qtyInBaseUom")]
    pub qty_in_base_uom: Decimal,
    #[sqlx(rename = "discountPercent")]
    pub discount_percent: Decimal,
    #[sqlx(rename = "discountAmount")]
    pub discount_amount: Decimal,
    #[sqlx(rename = "isFreeGood")]
    pub is_free_good: bool,
    pub tr_uuid: Option<String>,
    #[sqlx(rename = "taxRate")]
    pub tax_rate: Decimal,
    #[sqlx(rename = "taxIsInclusive")]
    pub tax_is_inclusive: bool,
    #[sqlx(rename = "grossAmount")]
    pub gross_amount: Decimal,
    #[sqlx(rename = "discountValue")]
    pub discount_value: Decimal,
    #[sqlx(rename = "netAmount")]
    pub net_amount: Decimal,
    #[sqlx(rename = "taxAmount")]
    pub tax_amount: Decimal,
    #[sqlx(rename = "lineTotal")]
    pub line_total: Decimal,
    #[sqlx(rename = "deliveredQty")]
    pub delivered_qty: Option<Decimal>,
    #[sqlx(rename = "deliveredQtyInBaseUom")]
    pub delivered_qty_in_base_uom: Option<Decimal>,
    #[sqlx(rename = "finalGrossAmount")]
    pub final_gross_amount: Option<Decimal>,
    #[sqlx(rename = "finalDiscountValue")]
    pub final_discount_value: Option<Decimal>,
    #[sqlx(rename = "finalNetAmount")]
    pub final_net_amount: Option<Decimal>,
    #[sqlx(rename = "finalTaxAmount")]
    pub final_tax_amount: Option<Decimal>,
    #[sqlx(rename = "finalLineTotal")]
    pub final_line_total: Option<Decimal>,
    #[sqlx(rename = "deliveryReasonId")]
    pub delivery_reason_id: Option<i32>,
    pub dr_uuid: Option<String>,
    pub dr_code: Option<String>,
    pub dr_name: Option<String>,
    #[sqlx(rename = "deliveryNote")]
    pub delivery_note: Option<String>,
    pub note: Option<String>,
    #[sqlx(rename = "customFields")]
    pub custom_fields: Value,
}

/// `String(decimal)`.
pub fn d(v: &Decimal) -> Value {
    Value::String(dstr(*v))
}

/// `toMoneyOrNull`.
pub fn d_opt(v: Option<&Decimal>) -> Value {
    v.map_or(Value::Null, d)
}

/// `toMasterRef` → `{ id, code, name }`.
fn master_ref(uuid: &Option<String>, code: &Option<String>, name: &Option<String>) -> Value {
    match (uuid, code, name) {
        (Some(u), Some(c), Some(n)) => json!({ "id": u, "code": c, "name": n }),
        _ => Value::Null,
    }
}

/// `toUserRef` → `{ id, name }`.
fn user_ref(uuid: &Option<String>, name: &Option<String>) -> Value {
    match uuid {
        Some(u) => json!({ "id": u, "name": name }),
        None => Value::Null,
    }
}

/// Node: src/orders/orders.service.ts:504 `computeDueDatePreview`.
fn due_date_preview(row: &OrderRow, time_zone: &str) -> Value {
    match (
        row.status.as_str(),
        row.order_payment_term_id,
        row.payment_term_days,
    ) {
        ("draft", Some(_), Some(days)) => {
            Value::String(compute_due_date(row.order_date, days, time_zone))
        }
        _ => Value::Null,
    }
}

/// Node: src/orders/orders.service.ts:518 `toOrderEntity` + `withUuidRefs` (`items` hanya detail).
pub fn to_order_entity(
    row: &OrderRow,
    time_zone: &str,
    items: Option<&[ItemRow]>,
) -> Map<String, Value> {
    let subtotal = dstr(row.subtotal);
    let discount_total = dstr(row.discount_total);
    let mut m = Map::new();
    let mut put = |k: &str, v: Value| {
        m.insert(k.to_owned(), v);
    };
    put("id", json!(row.uuid));
    put("orderNumber", json!(row.order_number));
    put("status", json!(row.status));
    put("source", json!(row.source));
    put(
        "orderType",
        master_ref(&row.ot_uuid, &row.ot_code, &row.ot_name),
    );
    put(
        "customer",
        master_ref(&row.c_uuid, &row.c_code, &row.c_name),
    );
    put("store", master_ref(&row.s_uuid, &row.s_code, &row.s_name));
    put("storeVisitId", json!(row.sv_uuid));
    put("storeGroupId", json!(row.sg_uuid));
    put("owner", user_ref(&row.ow_uuid, &row.ow_name));
    put("createdBy", user_ref(&row.cb_uuid, &row.cb_name));
    put("orderDate", json!(date_str(row.order_date)));
    put(
        "requestedDeliveryDate",
        json!(row.requested_delivery_date.map(date_str)),
    );
    put("note", json!(row.note));
    put("currencyCode", json!(row.currency_code));
    put("priceListId", json!(row.pl_uuid));
    put(
        "orderPaymentTerm",
        master_ref(&row.pt_uuid, &row.pt_code, &row.pt_name),
    );
    put("paymentTermDays", json!(row.payment_term_days));
    put("dueDate", json!(row.due_date.map(date_str)));
    put("dueDatePreview", due_date_preview(row, time_zone));
    put(
        "orderPaymentMethod",
        master_ref(&row.pm_uuid, &row.pm_code, &row.pm_name),
    );
    put("paymentReference", json!(row.payment_reference));
    put("subtotal", json!(subtotal));
    put("discountTotal", json!(discount_total));
    put("taxTotal", d(&row.tax_total));
    put("grandTotal", d(&row.grand_total));
    put("itemCount", json!(row.item_count));
    put(
        "effectiveDiscountPercent",
        json!(compute_effective_discount_percent(
            &subtotal,
            &discount_total
        )),
    );
    put("customFields", row.custom_fields.clone());
    if let Some(items) = items {
        put(
            "items",
            Value::Array(items.iter().map(to_item_entity).collect()),
        );
    }
    put("submittedAt", ms_opt(row.submitted_at));
    put("submittedBy", user_ref(&row.sb_uuid, &row.sb_name));
    put("approvedAt", ms_opt(row.approved_at));
    put("approvedBy", user_ref(&row.ab_uuid, &row.ab_name));
    put("rejectedAt", ms_opt(row.rejected_at));
    put("rejectedBy", user_ref(&row.rb_uuid, &row.rb_name));
    put("rejectNote", json!(row.reject_note));
    put("cancelledAt", ms_opt(row.cancelled_at));
    put("cancelledBy", user_ref(&row.xb_uuid, &row.xb_name));
    put(
        "cancelReason",
        master_ref(&row.cr_uuid, &row.cr_code, &row.cr_name),
    );
    put("cancelNote", json!(row.cancel_note));
    put("finalSubtotal", d_opt(row.final_subtotal.as_ref()));
    put(
        "finalDiscountTotal",
        d_opt(row.final_discount_total.as_ref()),
    );
    put("finalTaxTotal", d_opt(row.final_tax_total.as_ref()));
    put("finalGrandTotal", d_opt(row.final_grand_total.as_ref()));
    put("deliveredAt", ms_opt(row.delivered_at));
    put("deliveryRecordedAt", ms_opt(row.delivery_recorded_at));
    put("deliveredBy", user_ref(&row.db_uuid, &row.db_name));
    put("deliveryRevision", json!(row.delivery_revision));
    put("createdAt", json!(ms(row.created_at)));
    put("updatedAt", ms_opt(row.updated_at));
    m
}

/// Node: src/orders/orders.service.ts:575 `toItemEntity`.
pub fn to_item_entity(row: &ItemRow) -> Value {
    let short = row
        .delivered_qty
        .map_or(Value::Null, |dq| Value::String(dstr(sub(row.qty, dq))));
    let mut m = Map::new();
    let mut put = |k: &str, v: Value| {
        m.insert(k.to_owned(), v);
    };
    put("id", json!(row.uuid));
    put("lineNo", json!(row.line_no));
    put("productId", json!(row.product_uuid));
    put("productSku", json!(row.product_sku));
    put("productName", json!(row.product_name));
    put("uomId", json!(row.uom_uuid));
    put("uomCode", json!(row.uom_code));
    put("operator", json!(row.operator));
    put("factor", d(&row.factor));
    put("conversionRate", d(&row.conversion_rate));
    put("priceListItemId", json!(row.pli_uuid));
    put("listUnitPrice", d_opt(row.list_unit_price.as_ref()));
    put("unitPrice", d(&row.unit_price));
    put("qty", d(&row.qty));
    put("qtyInBaseUom", d(&row.qty_in_base_uom));
    put("discountPercent", d(&row.discount_percent));
    put("discountAmount", d(&row.discount_amount));
    put("isFreeGood", json!(row.is_free_good));
    put("taxRateId", json!(row.tr_uuid));
    put("taxRate", d(&row.tax_rate));
    put("taxIsInclusive", json!(row.tax_is_inclusive));
    put("grossAmount", d(&row.gross_amount));
    put("discountValue", d(&row.discount_value));
    put("netAmount", d(&row.net_amount));
    put("taxAmount", d(&row.tax_amount));
    put("lineTotal", d(&row.line_total));
    put("deliveredQty", d_opt(row.delivered_qty.as_ref()));
    put(
        "deliveredQtyInBaseUom",
        d_opt(row.delivered_qty_in_base_uom.as_ref()),
    );
    put("shortQty", short);
    put("finalGrossAmount", d_opt(row.final_gross_amount.as_ref()));
    put(
        "finalDiscountValue",
        d_opt(row.final_discount_value.as_ref()),
    );
    put("finalNetAmount", d_opt(row.final_net_amount.as_ref()));
    put("finalTaxAmount", d_opt(row.final_tax_amount.as_ref()));
    put("finalLineTotal", d_opt(row.final_line_total.as_ref()));
    put(
        "deliveryReason",
        master_ref(&row.dr_uuid, &row.dr_code, &row.dr_name),
    );
    put("deliveryNote", json!(row.delivery_note));
    put("note", json!(row.note));
    put("customFields", row.custom_fields.clone());
    Value::Object(m)
}

// ───────────────────────── pembaca ─────────────────────────

/// `order.findFirst({ where: { uuid, companyId }, include })`.
pub async fn find_row<'e>(
    exec: impl PgExecutor<'e>,
    company_id: i32,
    uuid: &str,
) -> ApiResult<Option<OrderRow>> {
    Ok(sqlx::query_as(&format!(
        r#"{ORDER_SELECT} WHERE o.uuid = $1 AND o."companyId" = $2 LIMIT 1"#
    ))
    .bind(crate::common::row_uuid(uuid)?)
    .bind(company_id)
    .fetch_optional(exec)
    .await?)
}

/// `order.findUniqueOrThrow({ where: { id }, include })`.
pub async fn find_row_by_id<'e>(exec: impl PgExecutor<'e>, id: i32) -> ApiResult<OrderRow> {
    let row: Option<OrderRow> = sqlx::query_as(&format!(r#"{ORDER_SELECT} WHERE o.id = $1"#))
        .bind(id)
        .fetch_optional(exec)
        .await?;
    row.ok_or_else(|| crate::common::internal("No order found"))
}

/// Item satu order urut `lineNo`.
pub async fn items_of<'e>(exec: impl PgExecutor<'e>, order_id: i32) -> ApiResult<Vec<ItemRow>> {
    Ok(sqlx::query_as(&format!(
        r#"{ITEM_SELECT} WHERE i."orderId" = $1 ORDER BY i."lineNo" ASC"#
    ))
    .bind(order_id)
    .fetch_all(exec)
    .await?)
}

/// Item beberapa order, dikelompokkan per `orderId` (urut `orderId`, `lineNo`).
pub async fn items_of_many(
    db: &sqlx::PgPool,
    order_ids: &[i32],
) -> ApiResult<HashMap<i32, Vec<ItemRow>>> {
    let mut out: HashMap<i32, Vec<ItemRow>> = HashMap::new();
    if order_ids.is_empty() {
        return Ok(out);
    }
    let rows: Vec<ItemRow> = sqlx::query_as(&format!(
        r#"{ITEM_SELECT} WHERE i."orderId" = ANY($1) ORDER BY i."orderId" ASC, i."lineNo" ASC"#
    ))
    .bind(order_ids)
    .fetch_all(db)
    .await?;
    for r in rows {
        out.entry(r.order_id).or_default().push(r);
    }
    Ok(out)
}

#[derive(sqlx::FromRow)]
struct HistoryRow {
    id: i32,
    uuid: String,
    kind: String,
    revision: i32,
    source: String,
    #[sqlx(rename = "recordedAt")]
    recorded_at: NaiveDateTime,
    #[sqlx(rename = "deliveredAtBefore")]
    delivered_at_before: Option<NaiveDateTime>,
    #[sqlx(rename = "deliveredAtAfter")]
    delivered_at_after: NaiveDateTime,
    #[sqlx(rename = "grandTotalBefore")]
    grand_total_before: Decimal,
    #[sqlx(rename = "grandTotalAfter")]
    grand_total_after: Decimal,
    note: Option<String>,
    rb_uuid: String,
    rb_name: String,
}

#[derive(sqlx::FromRow)]
struct HistoryLineRow {
    #[sqlx(rename = "orderDeliveryId")]
    order_delivery_id: i32,
    #[sqlx(rename = "lineNo")]
    line_no: i32,
    #[sqlx(rename = "orderedQty")]
    ordered_qty: Decimal,
    #[sqlx(rename = "qtyBefore")]
    qty_before: Decimal,
    #[sqlx(rename = "qtyAfter")]
    qty_after: Decimal,
    #[sqlx(rename = "reasonCode")]
    reason_code: Option<String>,
    #[sqlx(rename = "reasonName")]
    reason_name: Option<String>,
    note: Option<String>,
    item_uuid: String,
    reason_uuid: Option<String>,
}

/// Node: src/orders/orders.service.ts:797 `findDeliveryHistory` — urut `revision`.
pub async fn delivery_history(db: &sqlx::PgPool, order_id: i32) -> ApiResult<Vec<Value>> {
    let rows: Vec<HistoryRow> = sqlx::query_as(
        r#"SELECT d.id, d.uuid::text AS uuid, d.kind::text AS kind, d.revision, d.source::text AS source,
                  d."recordedAt", d."deliveredAtBefore", d."deliveredAtAfter", d."grandTotalBefore",
                  d."grandTotalAfter", d.note, u.uuid::text AS rb_uuid, u.name AS rb_name
             FROM "orderDelivery" d JOIN users u ON u.id = d."recordedById"
            WHERE d."orderId" = $1 ORDER BY d.revision ASC"#,
    )
    .bind(order_id)
    .fetch_all(db)
    .await?;
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<i32> = rows.iter().map(|r| r.id).collect();
    let lines: Vec<HistoryLineRow> = sqlx::query_as(
        r#"SELECT l."orderDeliveryId", l."lineNo", l."orderedQty", l."qtyBefore", l."qtyAfter",
                  l."reasonCode", l."reasonName", l.note, i.uuid::text AS item_uuid, r.uuid::text AS reason_uuid
             FROM "orderDeliveryLine" l
             JOIN "orderItem" i ON i.id = l."orderItemId"
             LEFT JOIN "orderReason" r ON r.id = l."reasonId"
            WHERE l."orderDeliveryId" = ANY($1) ORDER BY l."lineNo" ASC, l.id ASC"#,
    )
    .bind(&ids)
    .fetch_all(db)
    .await?;
    Ok(rows
        .iter()
        .map(|r| {
            let ls: Vec<Value> = lines
                .iter()
                .filter(|l| l.order_delivery_id == r.id)
                .map(|l| {
                    let reason = match (&l.reason_uuid, &l.reason_code, &l.reason_name) {
                        (Some(u), Some(c), Some(n)) => json!({ "id": u, "code": c, "name": n }),
                        _ => Value::Null,
                    };
                    json!({
                        "itemId": l.item_uuid,
                        "lineNo": l.line_no,
                        "orderedQty": dstr(l.ordered_qty),
                        "qtyBefore": dstr(l.qty_before),
                        "qtyAfter": dstr(l.qty_after),
                        "reason": reason,
                        "note": l.note,
                    })
                })
                .collect();
            json!({
                "id": r.uuid,
                "kind": r.kind,
                "revision": r.revision,
                "source": r.source,
                "recordedBy": { "id": r.rb_uuid, "name": r.rb_name },
                "recordedAt": ms(r.recorded_at),
                "deliveredAtBefore": ms_opt(r.delivered_at_before),
                "deliveredAtAfter": ms(r.delivered_at_after),
                "grandTotalBefore": dstr(r.grand_total_before),
                "grandTotalAfter": dstr(r.grand_total_after),
                "note": r.note,
                "lines": ls,
            })
        })
        .collect())
}

/// Detail order (`findOne`): baris + item + `deliveryHistory`. `None` = tidak ada.
pub async fn detail(
    db: &sqlx::PgPool,
    company_id: i32,
    uuid: &str,
) -> ApiResult<Option<(OrderRow, Vec<ItemRow>, Value)>> {
    let Some(row) = find_row(db, company_id, uuid).await? else {
        return Ok(None);
    };
    let tz = crate::common::company_time_zone(db, company_id).await?;
    let items = items_of(db, row.id).await?;
    let history = delivery_history(db, row.id).await?;
    let mut m = to_order_entity(&row, &tz, Some(&items));
    m.insert("deliveryHistory".into(), Value::Array(history));
    Ok(Some((row, items, Value::Object(m))))
}

/// Node: src/orders/orders.service.ts:868 `findManyDetailed` — serializer `findOne` tanpa history,
/// urutan `ids`; id yang hilang dilewati.
pub async fn find_many_detailed(
    db: &sqlx::PgPool,
    company_id: i32,
    ids: &[i32],
) -> ApiResult<Vec<Value>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(ORDER_SELECT);
    qb.push(r#" WHERE o.id = ANY("#)
        .push_bind(ids.to_vec())
        .push(r#") AND o."companyId" = "#)
        .push_bind(company_id);
    let rows: Vec<OrderRow> = qb.build_query_as().fetch_all(db).await?;
    let mut items = items_of_many(db, ids).await?;
    let tz = crate::common::company_time_zone(db, company_id).await?;
    let by_id: HashMap<i32, &OrderRow> = rows.iter().map(|r| (r.id, r)).collect();
    Ok(ids
        .iter()
        .filter_map(|id| by_id.get(id))
        .map(|row| {
            let its = items.remove(&row.id).unwrap_or_default();
            Value::Object(to_order_entity(row, &tz, Some(&its)))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refs() {
        assert_eq!(master_ref(&None, &None, &None), Value::Null);
        assert_eq!(
            master_ref(&Some("u".into()), &Some("C".into()), &Some("N".into())),
            json!({"id": "u", "code": "C", "name": "N"})
        );
        assert_eq!(
            user_ref(&Some("u".into()), &Some("A".into())),
            json!({"id": "u", "name": "A"})
        );
    }
}
