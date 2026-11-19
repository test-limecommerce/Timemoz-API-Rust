//! Port `src/orders/order-transitions.service.ts` — submit, approve, reject, cancel, duplicate.
//! Setiap transisi = `UPDATE … WHERE id = ? AND status = <dari>` (compare-and-set §6.2); submit
//! mengunci baris (`FOR UPDATE`) dan mengklaim nomor di transaksi yang sama (§7.3).

use std::collections::HashSet;

use chrono::NaiveDate;
use mz_audit::{AuditHandle, Lookup, Model, Row};
use mz_core::company::require_company_id;
use mz_core::time::{JsDate, now_naive};
use mz_core::{ApiResult, AppState};
use mz_products::custom_fields_validator::{
    CfDefinition, CfIssue, js_keys, validate_custom_field_values,
};
use rust_decimal::Decimal;
use serde_json::{Map, Value, json};
use sqlx::PgConnection;

use crate::common::{
    date_str, internal, not_found, order_not_found, state_conflict, today_in, unprocessable,
    unprocessable_details,
};
use crate::custom_fields::{cf_entity, definitions};
use crate::dto::{CreateOrder, ItemInput};
use crate::masters::{self, MasterKind};
use crate::pricing::{
    DiscountLimitLine, LineResult, check_discount_limit, compute_due_date, compute_totals, dec,
    dstr,
};
use crate::service::{self, BeforeCommit};

/// `findForTransition` → `(id, status)` (404 `ORDER_NOT_FOUND`).
async fn find_for_transition(
    state: &AppState,
    company_id: i32,
    uuid: &str,
) -> ApiResult<(i32, String)> {
    let row: Option<(i32, String)> = sqlx::query_as(
        r#"SELECT id, status::text FROM "order" WHERE uuid = $1 AND "companyId" = $2 LIMIT 1"#,
    )
    .bind(crate::common::row_uuid(uuid)?)
    .bind(company_id)
    .fetch_optional(&state.db)
    .await?;
    row.ok_or_else(order_not_found)
}

/// `assertRequiredFieldsSatisfied` — semua aturan kecuali `CUSTOM_FIELD_UNKNOWN_KEY`.
fn assert_required_fields_satisfied(
    entity: &str,
    defs: &[CfDefinition],
    values: &Map<String, Value>,
    context: Option<&str>,
) -> ApiResult<()> {
    let issues: Vec<CfIssue> = validate_custom_field_values(cf_entity(entity), defs, values)
        .into_iter()
        .filter(|i| i.code != "CUSTOM_FIELD_UNKNOWN_KEY")
        .collect();
    if issues.is_empty() {
        Ok(())
    } else {
        Err(service::issues_error(&issues, context))
    }
}

/// `keepWritableKeys` — hanya kunci yang definisinya masih aktif (urutan `Object.keys`).
fn keep_writable_keys(defs: &[CfDefinition], values: &Value) -> Map<String, Value> {
    let active: HashSet<&str> = defs
        .iter()
        .filter(|d| d.is_active)
        .map(|d| d.key.as_str())
        .collect();
    let mut kept = Map::new();
    if let Some(o) = values.as_object() {
        for k in js_keys(o) {
            if active.contains(k.as_str())
                && let Some(v) = o.get(k.as_str())
            {
                kept.insert(k.clone(), v.clone());
            }
        }
    }
    kept
}

fn obj(v: &Value) -> Map<String, Value> {
    v.as_object().cloned().unwrap_or_default()
}

#[derive(sqlx::FromRow)]
struct SubmitItem {
    #[sqlx(rename = "lineNo")]
    line_no: i32,
    #[sqlx(rename = "qtyInBaseUom")]
    qty_in_base_uom: Decimal,
    #[sqlx(rename = "grossAmount")]
    gross_amount: Decimal,
    #[sqlx(rename = "discountValue")]
    discount_value: Decimal,
    #[sqlx(rename = "netAmount")]
    net_amount: Decimal,
    #[sqlx(rename = "taxAmount")]
    tax_amount: Decimal,
    #[sqlx(rename = "lineTotal")]
    line_total: Decimal,
    #[sqlx(rename = "isFreeGood")]
    is_free_good: bool,
    #[sqlx(rename = "customFields")]
    custom_fields: Value,
}

/// Node: src/orders/order-transitions.service.ts:172 `submitInTx` — seluruh keputusan submit di
/// transaksi milik pemanggil.
pub async fn submit_in_tx(
    tx: &mut PgConnection,
    audit: &AuditHandle,
    user_id: i32,
    company_id: i32,
    order_id: i32,
) -> ApiResult<()> {
    sqlx::query(r#"SELECT "id" FROM "order" WHERE "id" = $1 FOR UPDATE"#)
        .bind(order_id)
        .execute(&mut *tx)
        .await?;
    let current: Option<(String, i32, NaiveDate, Option<i32>, Value)> = sqlx::query_as(
        r#"SELECT status::text, "orderTypeId", "orderDate", "paymentTermDays", "customFields" FROM "order" WHERE id = $1"#,
    )
    .bind(order_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((status, order_type_id, order_date, term_days, custom_fields)) = current else {
        return Err(state_conflict());
    };
    if status != "draft" {
        return Err(state_conflict());
    }
    let order_type = masters::find_by_id(&mut *tx, MasterKind::Types, order_type_id).await?;
    let company: Option<(String, Option<Decimal>)> = sqlx::query_as(
        r#"SELECT "timeZone", "maxOrderDiscountPercent" FROM company WHERE id = $1"#,
    )
    .bind(company_id)
    .fetch_optional(&mut *tx)
    .await?;
    let (tz, max_discount) = company.ok_or_else(|| internal("No company found"))?;

    // §7.3 — nomor diklaim DULU; gagal sesudahnya → rollback ikut mengembalikan counter.
    let prefix = order_type
        .number_prefix
        .clone()
        .unwrap_or_else(|| "ORD".into());
    let number = crate::number::next(
        tx,
        company_id,
        &crate::number::period_of(order_date),
        &prefix,
    )
    .await?;

    let items: Vec<SubmitItem> = sqlx::query_as(
        r#"SELECT "lineNo", "qtyInBaseUom", "grossAmount", "discountValue", "netAmount", "taxAmount", "lineTotal",
                  "isFreeGood", "customFields" FROM "orderItem" WHERE "orderId" = $1 ORDER BY id"#,
    )
    .bind(order_id)
    .fetch_all(&mut *tx)
    .await?;
    if items.is_empty() {
        return Err(unprocessable(
            "ORDER_EMPTY",
            "Cannot submit an order with no items",
        ));
    }
    let order_defs = definitions(&mut *tx, company_id, "order").await?;
    assert_required_fields_satisfied("order", &order_defs, &obj(&custom_fields), None)?;
    let item_defs = definitions(&mut *tx, company_id, "orderItem").await?;
    for item in &items {
        assert_required_fields_satisfied(
            "orderItem",
            &item_defs,
            &obj(&item.custom_fields),
            Some(&format!("item line {}", item.line_no)),
        )?;
    }
    let lines: Vec<LineResult> = items
        .iter()
        .map(|i| LineResult {
            qty_in_base_uom: dstr(i.qty_in_base_uom),
            gross_amount: dstr(i.gross_amount),
            discount_value: dstr(i.discount_value),
            net_amount: dstr(i.net_amount),
            tax_amount: dstr(i.tax_amount),
            line_total: dstr(i.line_total),
        })
        .collect();
    let totals = compute_totals(&lines);
    let max = max_discount.map(dstr);
    let check = check_discount_limit(
        &items
            .iter()
            .map(|i| DiscountLimitLine {
                gross_amount: dstr(i.gross_amount),
                discount_value: dstr(i.discount_value),
                is_free_good: i.is_free_good,
            })
            .collect::<Vec<_>>(),
        max.as_deref(),
    );
    if !check.ok {
        return Err(unprocessable_details(
            "DISCOUNT_LIMIT_EXCEEDED",
            "Order discount exceeds company.maxOrderDiscountPercent",
            json!({ "percentUsed": check.percent_used, "maxDiscountPercent": max }),
        ));
    }
    let due_date = term_days
        .map(|d| compute_due_date(order_date, d, &tz))
        .and_then(|s| crate::common::parse_calendar_date(&s));
    let now = now_naive();
    let new_status = if order_type.requires_approval.unwrap_or(false) {
        "pending_approval"
    } else {
        "approved"
    };
    let approved_at = (new_status == "approved").then_some(now);

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
        r#"UPDATE "order" SET status = $2::"OrderStatus", "orderNumber" = $3, "submittedAt" = $4, "submittedById" = $5,
              "approvedAt" = COALESCE($6, "approvedAt"), subtotal = $7, "discountTotal" = $8, "taxTotal" = $9,
              "grandTotal" = $10, "itemCount" = $11, "dueDate" = $12, "updatedAt" = $4
            WHERE id = $1 AND status = 'draft'"#,
    )
    .bind(order_id)
    .bind(new_status)
    .bind(&number)
    .bind(now)
    .bind(user_id)
    .bind(approved_at)
    .bind(dec(&totals.subtotal))
    .bind(dec(&totals.discount_total))
    .bind(dec(&totals.tax_total))
    .bind(dec(&totals.grand_total))
    .bind(totals.item_count)
    .bind(due_date)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    let mut data = Row::new()
        .with("status", new_status)
        .with("orderNumber", number.clone())
        .with("submittedAt", json!(JsDate(now)))
        .with("submittedById", user_id);
    if let Some(a) = approved_at {
        data.set("approvedAt", json!(JsDate(a)));
    }
    data.set("subtotal", totals.subtotal.clone());
    data.set("discountTotal", totals.discount_total.clone());
    data.set("taxTotal", totals.tax_total.clone());
    data.set("grandTotal", totals.grand_total.clone());
    data.set("itemCount", totals.item_count);
    data.set(
        "dueDate",
        json!(due_date.map(|d| JsDate(d.and_hms_opt(0, 0, 0).unwrap_or_default()))),
    );
    data.set("updatedAt", json!(JsDate(now)));
    audit
        .record_update_many(
            Model::Order,
            pre,
            n,
            &data,
            json!({ "id": order_id, "status": "draft" }),
        )
        .await;
    if n == 0 {
        return Err(state_conflict());
    }
    Ok(())
}

/// Node: src/orders/order-transitions.service.ts:158 `submit`.
pub async fn submit(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    uuid: &str,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let (id, status) = find_for_transition(state, company_id, uuid).await?;
    if status != "draft" {
        return Err(state_conflict());
    }
    let mut tx = state.db.begin().await?;
    submit_in_tx(&mut tx, audit, user_id, company_id, id).await?;
    tx.commit().await?;
    service::find_one_in(state, company_id, uuid).await
}

/// CAS `pending_approval` → `approved`/`rejected` (tanpa transaksi, seperti Node).
async fn decide(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    uuid: &str,
    reject_note: Option<&str>,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let (id, status) = find_for_transition(state, company_id, uuid).await?;
    if status != "pending_approval" {
        return Err(state_conflict());
    }
    let now = now_naive();
    let pre = audit
        .pre_read_many(
            Model::Order,
            Lookup::filter(move |qb| {
                qb.push("id = ")
                    .push_bind(id)
                    .push(" AND status = 'pending_approval'");
            }),
        )
        .await;
    let (n, data) = match reject_note {
        None => (
            sqlx::query(
                r#"UPDATE "order" SET status = 'approved', "approvedAt" = $2, "approvedById" = $3, "updatedAt" = $2
                    WHERE id = $1 AND status = 'pending_approval'"#,
            )
            .bind(id)
            .bind(now)
            .bind(user_id)
            .execute(&state.db)
            .await?
            .rows_affected(),
            Row::new()
                .with("status", "approved")
                .with("approvedAt", json!(JsDate(now)))
                .with("approvedById", user_id)
                .with("updatedAt", json!(JsDate(now))),
        ),
        Some(note) => (
            sqlx::query(
                r#"UPDATE "order" SET status = 'rejected', "rejectedAt" = $2, "rejectedById" = $3, "rejectNote" = $4,
                      "updatedAt" = $2 WHERE id = $1 AND status = 'pending_approval'"#,
            )
            .bind(id)
            .bind(now)
            .bind(user_id)
            .bind(note)
            .execute(&state.db)
            .await?
            .rows_affected(),
            Row::new()
                .with("status", "rejected")
                .with("rejectedAt", json!(JsDate(now)))
                .with("rejectedById", user_id)
                .with("rejectNote", note)
                .with("updatedAt", json!(JsDate(now))),
        ),
    };
    audit
        .record_update_many(
            Model::Order,
            pre,
            n,
            &data,
            json!({ "id": id, "status": "pending_approval" }),
        )
        .await;
    if n == 0 {
        return Err(state_conflict());
    }
    service::find_one_in(state, company_id, uuid).await
}

/// Node: src/orders/order-transitions.service.ts:290 `approve`.
pub async fn approve(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    uuid: &str,
) -> ApiResult<Value> {
    decide(state, audit, user_id, uuid, None).await
}

/// Node: src/orders/order-transitions.service.ts:305 `reject`.
pub async fn reject(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    uuid: &str,
    note: &str,
) -> ApiResult<Value> {
    decide(state, audit, user_id, uuid, Some(note)).await
}

/// Node: src/orders/order-transitions.service.ts:336 `cancel` (`reasonId` wajib, tipe `order_cancel`).
pub async fn cancel(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    uuid: &str,
    reason_id: Option<&str>,
    note: Option<&str>,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let (id, status) = find_for_transition(state, company_id, uuid).await?;
    if !matches!(status.as_str(), "draft" | "pending_approval" | "approved") {
        return Err(state_conflict());
    }
    let Some(reason_uuid) = reason_id.filter(|r| !r.is_empty()) else {
        return Err(unprocessable(
            "CANCEL_REASON_REQUIRED",
            "reasonId is required to cancel an order",
        ));
    };
    let mut tx = state.db.begin().await?;
    let reason = masters::find_by_uuid(&mut *tx, MasterKind::Reasons, company_id, reason_uuid)
        .await?
        .ok_or_else(|| not_found("ORDER_MASTER_NOT_FOUND", "Order reason not found"))?;
    if !reason.is_active {
        return Err(unprocessable(
            "ORDER_MASTER_ARCHIVED",
            "Order reason is archived",
        ));
    }
    if reason.reason_type.as_deref() != Some("order_cancel") {
        return Err(unprocessable(
            "REASON_TYPE_MISMATCH",
            "reasonId must reference an order_cancel reason",
        ));
    }
    let now = now_naive();
    let pre = audit
        .pre_read_many(
            Model::Order,
            Lookup::filter(move |qb| {
                qb.push("id = ")
                    .push_bind(id)
                    .push(" AND status IN ('draft', 'pending_approval', 'approved')");
            }),
        )
        .await;
    let n = sqlx::query(
        r#"UPDATE "order" SET status = 'cancelled', "cancelledAt" = $2, "cancelledById" = $3, "cancelReasonId" = $4,
              "cancelNote" = $5, "updatedAt" = $2
            WHERE id = $1 AND status IN ('draft', 'pending_approval', 'approved')"#,
    )
    .bind(id)
    .bind(now)
    .bind(user_id)
    .bind(reason.id)
    .bind(note)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    audit
        .record_update_many(
            Model::Order,
            pre,
            n,
            &Row::new()
                .with("status", "cancelled")
                .with("cancelledAt", json!(JsDate(now)))
                .with("cancelledById", user_id)
                .with("cancelReasonId", reason.id)
                .with("cancelNote", json!(note))
                .with("updatedAt", json!(JsDate(now))),
            json!({ "id": id, "status": { "in": ["draft", "pending_approval", "approved"] } }),
        )
        .await;
    if n == 0 {
        return Err(state_conflict());
    }
    tx.commit().await?;
    service::find_one_in(state, company_id, uuid).await
}

#[derive(sqlx::FromRow)]
struct DupSource {
    id: i32,
    ot_uuid: String,
    c_uuid: String,
    s_uuid: Option<String>,
    ow_uuid: String,
    #[sqlx(rename = "requestedDeliveryDate")]
    requested_delivery_date: Option<NaiveDate>,
    note: Option<String>,
    pt_uuid: Option<String>,
    pm_uuid: Option<String>,
    #[sqlx(rename = "paymentReference")]
    payment_reference: Option<String>,
    #[sqlx(rename = "customFields")]
    custom_fields: Value,
}

#[derive(sqlx::FromRow)]
struct DupItem {
    product_uuid: String,
    uom_uuid: String,
    qty: Decimal,
    #[sqlx(rename = "discountPercent")]
    discount_percent: Decimal,
    #[sqlx(rename = "discountAmount")]
    discount_amount: Decimal,
    #[sqlx(rename = "isFreeGood")]
    is_free_good: bool,
    note: Option<String>,
    #[sqlx(rename = "customFields")]
    custom_fields: Value,
}

/// Node: src/orders/order-transitions.service.ts:381 `duplicate` — draft baru, harga dihitung ulang.
pub async fn duplicate(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    uuid: &str,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let source: Option<DupSource> = sqlx::query_as(
        r#"SELECT o.id, ot.uuid::text AS ot_uuid, c.uuid::text AS c_uuid, s.uuid::text AS s_uuid,
                  ow.uuid::text AS ow_uuid, o."requestedDeliveryDate", o.note, pt.uuid::text AS pt_uuid,
                  pm.uuid::text AS pm_uuid, o."paymentReference", o."customFields"
             FROM "order" o
             JOIN "orderType" ot ON ot.id = o."orderTypeId"
             JOIN customer c ON c.id = o."customerId"
             LEFT JOIN store s ON s.id = o."storeId"
             JOIN users ow ON ow.id = o."ownerId"
             LEFT JOIN "orderPaymentTerm" pt ON pt.id = o."orderPaymentTermId"
             LEFT JOIN "orderPaymentMethod" pm ON pm.id = o."orderPaymentMethodId"
            WHERE o.uuid = $1 AND o."companyId" = $2 LIMIT 1"#,
    )
    .bind(crate::common::row_uuid(uuid)?)
    .bind(company_id)
    .fetch_optional(&state.db)
    .await?;
    let source = source.ok_or_else(order_not_found)?;
    let items: Vec<DupItem> = sqlx::query_as(
        r#"SELECT p.uuid::text AS product_uuid, u.uuid::text AS uom_uuid, i.qty, i."discountPercent",
                  i."discountAmount", i."isFreeGood", i.note, i."customFields"
             FROM "orderItem" i JOIN product p ON p.id = i."productId" JOIN uom u ON u.id = i."uomId"
            WHERE i."orderId" = $1 ORDER BY i."lineNo" ASC"#,
    )
    .bind(source.id)
    .fetch_all(&state.db)
    .await?;
    let tz = crate::common::company_time_zone(&state.db, company_id).await?;
    let order_defs = definitions(&state.db, company_id, "order").await?;
    let item_defs = definitions(&state.db, company_id, "orderItem").await?;
    let today = date_str(today_in(&tz));
    let (customer_id, store_id) = match &source.s_uuid {
        Some(s) => (None, Some(s.clone())),
        None => (Some(source.c_uuid.clone()), None),
    };
    let dto = CreateOrder {
        client_request_id: uuid::Uuid::new_v4().to_string(),
        order_type_id: source.ot_uuid.clone(),
        customer_id,
        store_id,
        store_visit_id: None,
        owner_id: Some(source.ow_uuid.clone()),
        order_date: today,
        requested_delivery_date: source.requested_delivery_date.map(date_str),
        note: source.note.clone(),
        order_payment_term_id: source.pt_uuid.clone(),
        order_payment_method_id: source.pm_uuid.clone(),
        payment_reference: source.payment_reference.clone(),
        custom_fields: Some(keep_writable_keys(&order_defs, &source.custom_fields)),
    };
    if items.is_empty() {
        return service::create(state, audit, user_id, &dto).await;
    }
    let inputs: Vec<ItemInput> = items
        .iter()
        .map(|i| ItemInput {
            product_id: i.product_uuid.clone(),
            uom_id: i.uom_uuid.clone(),
            qty: dstr(i.qty),
            discount_percent: Some(dstr(i.discount_percent)),
            discount_amount: Some(dstr(i.discount_amount)),
            is_free_good: Some(i.is_free_good),
            note: i.note.clone(),
            custom_fields: Some(keep_writable_keys(&item_defs, &i.custom_fields)),
            unit_price: None,
        })
        .collect();
    service::create_with_items(
        state,
        audit,
        user_id,
        &dto,
        &inputs,
        "web",
        BeforeCommit::None,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use mz_products::custom_fields_validator::CfDefinition;

    fn def(key: &str, active: bool, required: bool) -> CfDefinition {
        CfDefinition {
            key: key.into(),
            field_type: "text".into(),
            is_required: required,
            is_active: active,
            options: None,
        }
    }

    // Node: src/orders/order-transitions.service.spec.ts (aturan custom field submit/duplicate)
    #[test]
    fn submit_ignores_unknown_but_requires_required() {
        let defs = vec![def("po", true, true), def("old", false, false)];
        let v = json!({"old": "x"}).as_object().unwrap().clone();
        let e = assert_required_fields_satisfied("order", &defs, &v, None).unwrap_err();
        assert!(crate::common::has_code(&e, "CUSTOM_FIELD_REQUIRED"));
        let ok = json!({"old": "x", "po": "1"}).as_object().unwrap().clone();
        assert!(assert_required_fields_satisfied("order", &defs, &ok, None).is_ok());
        let e = assert_required_fields_satisfied("orderItem", &defs, &v, Some("item line 2"))
            .unwrap_err();
        match e {
            mz_core::ApiError::Body { body, .. } => {
                assert_eq!(body["message"], "item line 2: po is required")
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn duplicate_keeps_only_active_keys() {
        let defs = vec![def("po", true, false), def("old", false, false)];
        let kept = keep_writable_keys(&defs, &json!({"old": 1, "po": "A", "x": 2}));
        assert_eq!(Value::Object(kept), json!({"po": "A"}));
    }
}
