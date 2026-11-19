//! Port `src/orders/order-deliveries.service.ts` (specs/003) — catat qty yang diterima
//! (`approved → delivered`) dan koreksi (dashboard saja). Penjaga TOCTOU T1–T6 sama dengan Node:
//! `FOR UPDATE` + CAS status, pagar `deliveryRevision`, replay `clientRequestId`, alasan dibaca
//! di dalam transaksi, item dibaca sesudah kunci.

use std::collections::{HashMap, HashSet};

use chrono::NaiveDateTime;
use mz_audit::{AuditHandle, Lookup, Model, Row};
use mz_core::company::require_company_id;
use mz_core::time::{JsDate, now_naive};
use mz_core::{ApiError, ApiResult, AppState};
use mz_products::dto::Field;
use mz_store_visits::scope::{StoreScope, is_in_scope, push_scope_clause, resolve_store_scope};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use sqlx::{PgConnection, Postgres, QueryBuilder};

use crate::common::{
    has_code, internal, not_found, order_not_found, state_conflict, unprocessable,
    unprocessable_details,
};
use crate::cursor::{ORDER_DELTA_SAFETY_LAG_MS, decode_order_cursor, encode_order_cursor};
use crate::dto::{CorrectDelivery, DELIVERY_LIST_STATUSES, DeliverOrder, DeliveryLine};
use crate::pricing::{
    DeliverableLine, LineResult, compute_delivered_line, compute_delivered_totals, dec, dstr,
};
use crate::service;

/// R12 — toleransi jam perangkat untuk `deliveredAt`.
pub const DELIVERED_AT_FUTURE_TOLERANCE_MS: i64 = 120_000;

fn revision_conflict(expected: i64, current: i64) -> ApiError {
    ApiError::body(
        axum::http::StatusCode::CONFLICT,
        json!({
            "code": "ORDER_DELIVERY_REVISION_CONFLICT",
            "message": "The delivery record was changed by someone else — reload before correcting it",
            "details": { "expectedRevision": expected, "currentRevision": current },
        }),
    )
}

/// `orderInScope`: order tanpa toko terlihat oleh semua; dengan toko = predikat region toko.
fn order_in_scope(scope: &StoreScope, store: Option<Option<i32>>) -> bool {
    match store {
        None => true,
        Some(group) => is_in_scope(scope, group),
    }
}

#[derive(sqlx::FromRow, Clone)]
struct ItemRow {
    id: i32,
    uuid: String,
    #[sqlx(rename = "lineNo")]
    line_no: i32,
    qty: Decimal,
    #[sqlx(rename = "unitPrice")]
    unit_price: Decimal,
    #[sqlx(rename = "conversionRate")]
    conversion_rate: Decimal,
    #[sqlx(rename = "discountPercent")]
    discount_percent: Decimal,
    #[sqlx(rename = "discountAmount")]
    discount_amount: Decimal,
    #[sqlx(rename = "taxRate")]
    tax_rate: Decimal,
    #[sqlx(rename = "taxIsInclusive")]
    tax_is_inclusive: bool,
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
    #[sqlx(rename = "deliveredQty")]
    delivered_qty: Option<Decimal>,
    #[sqlx(rename = "deliveryReasonId")]
    delivery_reason_id: Option<i32>,
    #[sqlx(rename = "deliveryNote")]
    delivery_note: Option<String>,
}

impl ItemRow {
    fn deliverable(&self) -> DeliverableLine {
        DeliverableLine {
            qty: dstr(self.qty),
            unit_price: dstr(self.unit_price),
            conversion_rate: dstr(self.conversion_rate),
            discount_percent: dstr(self.discount_percent),
            discount_amount: dstr(self.discount_amount),
            tax_rate: dstr(self.tax_rate),
            tax_is_inclusive: self.tax_is_inclusive,
            stored: LineResult {
                qty_in_base_uom: dstr(self.qty_in_base_uom),
                gross_amount: dstr(self.gross_amount),
                discount_value: dstr(self.discount_value),
                net_amount: dstr(self.net_amount),
                tax_amount: dstr(self.tax_amount),
                line_total: dstr(self.line_total),
            },
        }
    }
}

async fn items_locked(tx: &mut PgConnection, order_id: i32) -> ApiResult<Vec<ItemRow>> {
    Ok(sqlx::query_as(
        r#"SELECT id, uuid::text AS uuid, "lineNo", qty, "unitPrice", "conversionRate", "discountPercent",
                  "discountAmount", "taxRate", "taxIsInclusive", "qtyInBaseUom", "grossAmount", "discountValue",
                  "netAmount", "taxAmount", "lineTotal", "deliveredQty", "deliveryReasonId", "deliveryNote"
             FROM "orderItem" WHERE "orderId" = $1 ORDER BY "lineNo" ASC"#,
    )
    .bind(order_id)
    .fetch_all(&mut *tx)
    .await?)
}

/// Hasil satu baris deliver/correct.
struct Resolved {
    item: ItemRow,
    qty_before: Decimal,
    qty_after: Decimal,
    reason_id: Option<i32>,
    note: Option<String>,
    fin: LineResult,
}

/// `resolveReasons` (T5): hanya alasan pada baris yang berakhir KURANG yang divalidasi.
async fn resolve_reasons(
    tx: &mut PgConnection,
    company_id: i32,
    items: &[ItemRow],
    sent: &[(String, &DeliveryLine)],
) -> ApiResult<HashMap<String, i32>> {
    let qty_by_uuid: HashMap<&str, Decimal> =
        items.iter().map(|i| (i.uuid.as_str(), i.qty)).collect();
    let mut wanted: Vec<String> = Vec::new();
    for (key, line) in sent {
        if let (Some(r), Some(q)) = (&line.reason_id, qty_by_uuid.get(key.as_str()))
            && dec(&line.delivered_qty) < *q
            && !wanted.contains(r)
        {
            wanted.push(r.clone());
        }
    }
    if wanted.is_empty() {
        return Ok(HashMap::new());
    }
    let parsed: Vec<uuid::Uuid> = wanted
        .iter()
        .filter_map(|u| uuid::Uuid::parse_str(u).ok())
        .collect();
    let rows: Vec<(i32, String, bool, String)> = sqlx::query_as(
        r#"SELECT id, uuid::text, "isActive", "reasonType"::text FROM "orderReason"
            WHERE uuid = ANY($1) AND "companyId" = $2"#,
    )
    .bind(&parsed)
    .bind(company_id)
    .fetch_all(&mut *tx)
    .await?;
    let by_uuid: HashMap<&str, &(i32, String, bool, String)> =
        rows.iter().map(|r| (r.1.as_str(), r)).collect();
    for w in &wanted {
        let Some(row) = by_uuid.get(w.as_str()) else {
            return Err(not_found(
                "ORDER_MASTER_NOT_FOUND",
                "Order reason not found",
            ));
        };
        if !row.2 {
            return Err(unprocessable(
                "ORDER_MASTER_ARCHIVED",
                "Order reason is archived",
            ));
        }
        if row.3 != "delivery_adjustment" {
            return Err(unprocessable(
                "REASON_TYPE_MISMATCH",
                "reasonId must reference a delivery_adjustment reason",
            ));
        }
    }
    Ok(rows.into_iter().map(|r| (r.1, r.0)).collect())
}

/// Node: src/orders/order-deliveries.service.ts:393 `resolveLines`.
async fn resolve_lines(
    tx: &mut PgConnection,
    company_id: i32,
    items: &[ItemRow],
    input: &[DeliveryLine],
    correction: bool,
) -> ApiResult<Vec<Resolved>> {
    let known: HashSet<String> = items.iter().map(|i| i.uuid.clone()).collect();
    let mut sent: Vec<(String, &DeliveryLine)> = Vec::new();
    let mut unknown: Vec<String> = Vec::new();
    let mut duplicate: Vec<String> = Vec::new();
    for line in input {
        if !known.contains(&line.item_id) {
            unknown.push(line.item_id.clone());
        } else if sent.iter().any(|(k, _)| *k == line.item_id) {
            duplicate.push(line.item_id.clone());
        } else {
            sent.push((line.item_id.clone(), line));
        }
    }
    if !unknown.is_empty() {
        return Err(unprocessable_details(
            "DELIVERY_LINE_UNKNOWN",
            "One or more lines do not belong to this order",
            json!({ "itemIds": unknown }),
        ));
    }
    if !duplicate.is_empty() {
        let mut uniq: Vec<String> = Vec::new();
        for d in duplicate {
            if !uniq.contains(&d) {
                uniq.push(d);
            }
        }
        return Err(unprocessable_details(
            "DELIVERY_LINE_DUPLICATE",
            "The same line was sent more than once",
            json!({ "itemIds": uniq }),
        ));
    }
    let reasons = resolve_reasons(tx, company_id, items, &sent).await?;
    let mut exceeds = Vec::new();
    let mut missing = Vec::new();
    let mut resolved = Vec::new();
    for item in items {
        let qty = item.qty;
        let line = sent.iter().find(|(k, _)| *k == item.uuid).map(|(_, l)| *l);
        let stored = item.delivered_qty.unwrap_or(qty);
        let qty_before = if correction { stored } else { qty };
        let qty_after = line.map_or(qty_before, |l| dec(&l.delivered_qty));
        if qty_after > qty {
            exceeds.push(json!({
                "itemId": item.uuid, "lineNo": item.line_no, "qty": dstr(qty), "deliveredQty": dstr(qty_after),
            }));
            continue;
        }
        let (mut reason_id, mut note) = (None, None);
        if qty_after < qty {
            let sent_reason = line
                .and_then(|l| l.reason_id.as_ref())
                .and_then(|r| reasons.get(r))
                .copied();
            reason_id = sent_reason.or(if correction {
                item.delivery_reason_id
            } else {
                None
            });
            note = match line.map(|l| &l.note) {
                Some(Field::Val(n)) => Some(n.clone()).filter(|n| !n.is_empty()),
                Some(Field::Null) => None,
                _ => {
                    if correction {
                        item.delivery_note.clone()
                    } else {
                        None
                    }
                }
            };
            if reason_id.is_none() {
                missing.push(json!({ "itemId": item.uuid, "lineNo": item.line_no }));
            }
        }
        let fin = compute_delivered_line(&item.deliverable(), &dstr(qty_after));
        resolved.push(Resolved {
            item: item.clone(),
            qty_before,
            qty_after,
            reason_id,
            note,
            fin,
        });
    }
    if !exceeds.is_empty() {
        return Err(unprocessable_details(
            "DELIVERY_QTY_EXCEEDS_ORDERED",
            "deliveredQty cannot exceed the ordered quantity",
            json!({ "lines": exceeds }),
        ));
    }
    if resolved.iter().all(|l| l.qty_after.is_zero()) {
        return Err(unprocessable(
            "DELIVERY_ALL_ZERO",
            "Nothing was delivered on any line — cancel the order instead of recording a delivery",
        ));
    }
    if !missing.is_empty() {
        return Err(unprocessable_details(
            "DELIVERY_REASON_REQUIRED",
            "A reason is required for every line delivered short",
            json!({ "lines": missing }),
        ));
    }
    Ok(resolved)
}

/// R12 `resolveDeliveredAt` — tidak sebelum approval, tidak di masa depan (toleransi 2 menit,
/// dijepit ke `now`).
fn resolve_delivered_at(
    epoch_ms: i64,
    approved_at: Option<NaiveDateTime>,
    now: NaiveDateTime,
) -> ApiResult<NaiveDateTime> {
    let now_ms = now.and_utc().timestamp_millis();
    if epoch_ms > now_ms + DELIVERED_AT_FUTURE_TOLERANCE_MS {
        return Err(unprocessable(
            "DELIVERED_AT_IN_FUTURE",
            "deliveredAt cannot be in the future",
        ));
    }
    if let Some(a) = approved_at
        && epoch_ms < a.and_utc().timestamp_millis()
    {
        return Err(unprocessable(
            "DELIVERED_AT_BEFORE_APPROVAL",
            "deliveredAt cannot be before the order was approved",
        ));
    }
    chrono::DateTime::from_timestamp_millis(epoch_ms.min(now_ms))
        .map(|d| d.naive_utc())
        .ok_or_else(|| internal("Invalid Date"))
}

/// `writeLines` — satu `UPDATE … FROM (VALUES …)`; jumlah baris tertulis harus sama.
async fn write_lines(tx: &mut PgConnection, order_id: i32, lines: &[Resolved]) -> ApiResult<()> {
    if lines.is_empty() {
        return Ok(());
    }
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
        r#"UPDATE "orderItem" AS oi SET "deliveredQty" = v.dq, "deliveredQtyInBaseUom" = v.dqb,
             "finalGrossAmount" = v.gross, "finalDiscountValue" = v.disc, "finalNetAmount" = v.net,
             "finalTaxAmount" = v.tax, "finalLineTotal" = v.total, "deliveryReasonId" = v.rid, "deliveryNote" = v.note
           FROM ("#,
    );
    qb.push_values(lines, |mut b, l| {
        b.push_bind(l.item.id)
            .push_unseparated("::int")
            .push_bind(l.qty_after)
            .push_unseparated("::numeric")
            .push_bind(dec(&l.fin.qty_in_base_uom))
            .push_unseparated("::numeric")
            .push_bind(dec(&l.fin.gross_amount))
            .push_unseparated("::numeric")
            .push_bind(dec(&l.fin.discount_value))
            .push_unseparated("::numeric")
            .push_bind(dec(&l.fin.net_amount))
            .push_unseparated("::numeric")
            .push_bind(dec(&l.fin.tax_amount))
            .push_unseparated("::numeric")
            .push_bind(dec(&l.fin.line_total))
            .push_unseparated("::numeric")
            .push_bind(l.reason_id)
            .push_unseparated("::int")
            .push_bind(l.note.clone())
            .push_unseparated("::varchar");
    });
    qb.push(r#") AS v(id, dq, dqb, gross, disc, net, tax, total, rid, note) WHERE oi."id" = v.id AND oi."orderId" = "#)
        .push_bind(order_id);
    let written = qb.build().execute(&mut *tx).await?.rows_affected();
    if usize::try_from(written).unwrap_or(0) != lines.len() {
        return Err(state_conflict());
    }
    Ok(())
}

/// Satu entri history (append-only, R4).
struct History<'a> {
    company_id: i32,
    order_id: i32,
    kind: &'a str,
    revision: i32,
    source: &'a str,
    client_request_id: Option<&'a str>,
    delivered_at_before: Option<NaiveDateTime>,
    delivered_at_after: NaiveDateTime,
    grand_total_before: Decimal,
    grand_total_after: Decimal,
    note: Option<&'a str>,
    recorded_by: i32,
    recorded_at: NaiveDateTime,
}

async fn append_history(
    tx: &mut PgConnection,
    h: History<'_>,
    lines: &[&Resolved],
) -> ApiResult<()> {
    let delivery_id: i32 = sqlx::query_scalar(
        r#"INSERT INTO "orderDelivery" ("companyId", "orderId", kind, revision, source, "clientRequestId",
              "deliveredAtBefore", "deliveredAtAfter", "grandTotalBefore", "grandTotalAfter", note, "recordedById", "recordedAt")
           VALUES ($1, $2, $3::"OrderDeliveryKind", $4, $5::"OrderSource", $6::uuid, $7, $8, $9, $10, $11, $12, $13)
           RETURNING id"#,
    )
    .bind(h.company_id)
    .bind(h.order_id)
    .bind(h.kind)
    .bind(h.revision)
    .bind(h.source)
    .bind(h.client_request_id)
    .bind(h.delivered_at_before)
    .bind(h.delivered_at_after)
    .bind(h.grand_total_before)
    .bind(h.grand_total_after)
    .bind(h.note)
    .bind(h.recorded_by)
    .bind(h.recorded_at)
    .fetch_one(&mut *tx)
    .await?;
    if lines.is_empty() {
        return Ok(());
    }
    let mut reason_ids: Vec<i32> = lines.iter().filter_map(|l| l.reason_id).collect();
    reason_ids.dedup();
    let reasons: Vec<(i32, String, String)> = if reason_ids.is_empty() {
        Vec::new()
    } else {
        sqlx::query_as(r#"SELECT id, code, name FROM "orderReason" WHERE id = ANY($1)"#)
            .bind(&reason_ids)
            .fetch_all(&mut *tx)
            .await?
    };
    let by_id: HashMap<i32, &(i32, String, String)> = reasons.iter().map(|r| (r.0, r)).collect();
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
        r#"INSERT INTO "orderDeliveryLine" ("orderDeliveryId", "orderItemId", "lineNo", "orderedQty", "qtyBefore",
             "qtyAfter", "reasonId", "reasonCode", "reasonName", note) "#,
    );
    qb.push_values(lines, |mut b, l| {
        let r = l.reason_id.and_then(|id| by_id.get(&id));
        b.push_bind(delivery_id)
            .push_bind(l.item.id)
            .push_bind(l.item.line_no)
            .push_bind(l.item.qty)
            .push_bind(l.qty_before)
            .push_bind(l.qty_after)
            .push_bind(l.reason_id)
            .push_bind(r.map(|r| r.1.clone()))
            .push_bind(r.map(|r| r.2.clone()))
            .push_bind(l.note.clone());
    });
    qb.build().execute(&mut *tx).await?;
    Ok(())
}

/// `true` bila `clientRequestId` = kunci delivery awal yang sudah tercatat (T4).
async fn is_replay(state: &AppState, order_id: i32, client_request_id: &str) -> ApiResult<bool> {
    let row: Option<Option<String>> = sqlx::query_scalar(
        r#"SELECT "clientRequestId"::text FROM "orderDelivery" WHERE "orderId" = $1 AND kind = 'initial' LIMIT 1"#,
    )
    .bind(order_id)
    .fetch_optional(&state.db)
    .await?;
    Ok(row
        .flatten()
        .is_some_and(|c| c.eq_ignore_ascii_case(client_request_id)))
}

async fn order_store_group(
    exec: impl sqlx::PgExecutor<'_>,
    order_id: i32,
) -> ApiResult<Option<Option<i32>>> {
    let row: Option<(Option<i32>, Option<i32>)> = sqlx::query_as(
        r#"SELECT o."storeId", s."groupId" FROM "order" o LEFT JOIN store s ON s.id = o."storeId" WHERE o.id = $1"#,
    )
    .bind(order_id)
    .fetch_optional(exec)
    .await?;
    Ok(row.and_then(|(store, group)| store.map(|_| group)))
}

/// Node: src/orders/order-deliveries.service.ts:157 `deliver`. `mobile_scope` = jalur mobile (R6).
pub async fn deliver(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    uuid: &str,
    dto: &DeliverOrder,
    source: &str,
    mobile_scope: bool,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let scope = if mobile_scope {
        resolve_store_scope(&state.db, user_id, company_id, None).await?
    } else {
        None
    };
    let existing: Option<(i32, String)> = sqlx::query_as(
        r#"SELECT id, status::text FROM "order" WHERE uuid = $1 AND "companyId" = $2 LIMIT 1"#,
    )
    .bind(crate::common::row_uuid(uuid)?)
    .bind(company_id)
    .fetch_optional(&state.db)
    .await?;
    let (order_id, status) = existing.ok_or_else(order_not_found)?;
    if mobile_scope && !order_in_scope(&scope, order_store_group(&state.db, order_id).await?) {
        return Err(order_not_found());
    }
    let attempt = async {
        if status != "approved" {
            return Err(state_conflict());
        }
        let mut tx = state.db.begin().await?;
        deliver_in_tx(
            &mut tx,
            audit,
            user_id,
            company_id,
            order_id,
            dto,
            source,
            mobile_scope.then_some(&scope),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    };
    match attempt.await {
        Ok(()) => {}
        Err(e)
            if has_code(&e, "ORDER_STATE_CONFLICT")
                || matches!(e, ApiError::UniqueViolation(_)) =>
        {
            if !is_replay(state, order_id, &dto.client_request_id).await? {
                return Err(state_conflict());
            }
        }
        Err(e) => return Err(e),
    }
    service::find_one_in(state, company_id, uuid).await
}

#[allow(clippy::too_many_arguments)]
async fn deliver_in_tx(
    tx: &mut PgConnection,
    audit: &AuditHandle,
    user_id: i32,
    company_id: i32,
    order_id: i32,
    dto: &DeliverOrder,
    source: &str,
    scope: Option<&StoreScope>,
) -> ApiResult<()> {
    sqlx::query(r#"SELECT "id" FROM "order" WHERE "id" = $1 FOR UPDATE"#)
        .bind(order_id)
        .execute(&mut *tx)
        .await?;
    let current: Option<(String, Option<NaiveDateTime>, Decimal)> = sqlx::query_as(
        r#"SELECT status::text, "approvedAt", "grandTotal" FROM "order" WHERE id = $1"#,
    )
    .bind(order_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((status, approved_at, grand_total)) = current.filter(|c| c.0 == "approved") else {
        return Err(state_conflict());
    };
    let _ = status;
    if let Some(scope) = scope
        && !order_in_scope(scope, order_store_group(&mut *tx, order_id).await?)
    {
        return Err(order_not_found());
    }
    let now = now_naive();
    let delivered_at = resolve_delivered_at(dto.delivered_at, approved_at, now)?;
    let items = items_locked(tx, order_id).await?;
    let lines = resolve_lines(tx, company_id, &items, &dto.lines, false).await?;
    let totals = compute_delivered_totals(&lines.iter().map(|l| l.fin.clone()).collect::<Vec<_>>());

    let pre = audit
        .pre_read_many(
            Model::Order,
            Lookup::filter(move |qb| {
                qb.push("id = ")
                    .push_bind(order_id)
                    .push(" AND status = 'approved'");
            }),
        )
        .await;
    let n = sqlx::query(
        r#"UPDATE "order" SET status = 'delivered', "deliveryRevision" = 1, "finalSubtotal" = $2,
              "finalDiscountTotal" = $3, "finalTaxTotal" = $4, "finalGrandTotal" = $5, "deliveredAt" = $6,
              "deliveryRecordedAt" = $7, "deliveredById" = $8, "updatedAt" = $7
            WHERE id = $1 AND status = 'approved'"#,
    )
    .bind(order_id)
    .bind(dec(&totals.subtotal))
    .bind(dec(&totals.discount_total))
    .bind(dec(&totals.tax_total))
    .bind(dec(&totals.grand_total))
    .bind(delivered_at)
    .bind(now)
    .bind(user_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    audit
        .record_update_many(
            Model::Order,
            pre,
            n,
            &Row::new()
                .with("status", "delivered")
                .with("deliveryRevision", 1)
                .with("finalSubtotal", totals.subtotal.clone())
                .with("finalDiscountTotal", totals.discount_total.clone())
                .with("finalTaxTotal", totals.tax_total.clone())
                .with("finalGrandTotal", totals.grand_total.clone())
                .with("deliveredAt", json!(JsDate(delivered_at)))
                .with("deliveryRecordedAt", json!(JsDate(now)))
                .with("deliveredById", user_id)
                .with("updatedAt", json!(JsDate(now))),
            json!({ "id": order_id, "status": "approved" }),
        )
        .await;
    if n == 0 {
        return Err(state_conflict());
    }
    write_lines(tx, order_id, &lines).await?;
    let short: Vec<&Resolved> = lines
        .iter()
        .filter(|l| l.qty_after < l.qty_before)
        .collect();
    append_history(
        tx,
        History {
            company_id,
            order_id,
            kind: "initial",
            revision: 1,
            source,
            client_request_id: Some(&dto.client_request_id),
            delivered_at_before: None,
            delivered_at_after: delivered_at,
            grand_total_before: grand_total,
            grand_total_after: dec(&totals.grand_total),
            note: dto.note.as_deref(),
            recorded_by: user_id,
            recorded_at: now,
        },
        &short,
    )
    .await
}

/// Node: src/orders/order-deliveries.service.ts:272 `correct` (dashboard saja).
pub async fn correct(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    uuid: &str,
    dto: &CorrectDelivery,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let existing: Option<(i32, String)> = sqlx::query_as(
        r#"SELECT id, status::text FROM "order" WHERE uuid = $1 AND "companyId" = $2 LIMIT 1"#,
    )
    .bind(crate::common::row_uuid(uuid)?)
    .bind(company_id)
    .fetch_optional(&state.db)
    .await?;
    let (order_id, status) = existing.ok_or_else(order_not_found)?;
    if status != "delivered" {
        return Err(state_conflict());
    }
    let mut tx = state.db.begin().await?;
    correct_in_tx(&mut tx, audit, user_id, company_id, order_id, dto).await?;
    tx.commit().await?;
    service::find_one_in(state, company_id, uuid).await
}

async fn correct_in_tx(
    tx: &mut PgConnection,
    audit: &AuditHandle,
    user_id: i32,
    company_id: i32,
    order_id: i32,
    dto: &CorrectDelivery,
) -> ApiResult<()> {
    sqlx::query(r#"SELECT "id" FROM "order" WHERE "id" = $1 FOR UPDATE"#)
        .bind(order_id)
        .execute(&mut *tx)
        .await?;
    #[allow(clippy::type_complexity)]
    let current: Option<(String, Option<NaiveDateTime>, i32, Option<NaiveDateTime>, Option<Decimal>)> = sqlx::query_as(
        r#"SELECT status::text, "approvedAt", "deliveryRevision", "deliveredAt", "finalGrandTotal" FROM "order" WHERE id = $1"#,
    )
    .bind(order_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((_, approved_at, revision, delivered_at_cur, final_grand)) =
        current.filter(|c| c.0 == "delivered")
    else {
        return Err(state_conflict());
    };
    if i64::from(revision) != dto.expected_revision {
        return Err(revision_conflict(
            dto.expected_revision,
            i64::from(revision),
        ));
    }
    let now = now_naive();
    let delivered_at = match dto.delivered_at {
        Some(ms) => resolve_delivered_at(ms, approved_at, now)?,
        None => delivered_at_cur.ok_or_else(|| internal("deliveredAt missing"))?,
    };
    let changed_at = delivered_at_cur.map(|d| d.and_utc().timestamp_millis())
        != Some(delivered_at.and_utc().timestamp_millis());
    let items = items_locked(tx, order_id).await?;
    let lines = resolve_lines(tx, company_id, &items, &dto.lines, true).await?;
    let changed: Vec<&Resolved> = lines
        .iter()
        .filter(|l| l.qty_after != l.qty_before)
        .collect();
    if changed.is_empty() && !changed_at {
        return Err(unprocessable(
            "DELIVERY_NO_CHANGES",
            "The correction changes neither a quantity nor the delivery time",
        ));
    }
    let totals = compute_delivered_totals(&lines.iter().map(|l| l.fin.clone()).collect::<Vec<_>>());
    let expected = i32::try_from(dto.expected_revision).unwrap_or(i32::MAX);
    let pre = audit
        .pre_read_many(
            Model::Order,
            Lookup::filter(move |qb| {
                qb.push("id = ")
                    .push_bind(order_id)
                    .push(r#" AND status = 'delivered' AND "deliveryRevision" = "#)
                    .push_bind(expected);
            }),
        )
        .await;
    let n = sqlx::query(
        r#"UPDATE "order" SET "finalSubtotal" = $2, "finalDiscountTotal" = $3, "finalTaxTotal" = $4,
              "finalGrandTotal" = $5, "deliveredAt" = $6, "deliveryRevision" = "deliveryRevision" + 1, "updatedAt" = $7
            WHERE id = $1 AND status = 'delivered' AND "deliveryRevision" = $8"#,
    )
    .bind(order_id)
    .bind(dec(&totals.subtotal))
    .bind(dec(&totals.discount_total))
    .bind(dec(&totals.tax_total))
    .bind(dec(&totals.grand_total))
    .bind(delivered_at)
    .bind(now)
    .bind(expected)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    audit
        .record_update_many(
            Model::Order,
            pre,
            n,
            &Row::new()
                .with("finalSubtotal", totals.subtotal.clone())
                .with("finalDiscountTotal", totals.discount_total.clone())
                .with("finalTaxTotal", totals.tax_total.clone())
                .with("finalGrandTotal", totals.grand_total.clone())
                .with("deliveredAt", json!(JsDate(delivered_at)))
                .with("deliveryRevision", json!({ "increment": 1 }))
                .with("updatedAt", json!(JsDate(now))),
            json!({ "id": order_id, "status": "delivered", "deliveryRevision": expected }),
        )
        .await;
    if n == 0 {
        let latest: Option<i32> =
            sqlx::query_scalar(r#"SELECT "deliveryRevision" FROM "order" WHERE id = $1"#)
                .bind(order_id)
                .fetch_optional(&mut *tx)
                .await?;
        return Err(revision_conflict(
            dto.expected_revision,
            latest.map_or(dto.expected_revision, i64::from),
        ));
    }
    let changed_owned: Vec<Resolved> = changed
        .iter()
        .map(|l| Resolved {
            item: l.item.clone(),
            qty_before: l.qty_before,
            qty_after: l.qty_after,
            reason_id: l.reason_id,
            note: l.note.clone(),
            fin: l.fin.clone(),
        })
        .collect();
    write_lines(tx, order_id, &changed_owned).await?;
    append_history(
        tx,
        History {
            company_id,
            order_id,
            kind: "correction",
            revision: expected.saturating_add(1),
            source: "web",
            client_request_id: None,
            delivered_at_before: delivered_at_cur,
            delivered_at_after: delivered_at,
            grand_total_before: final_grand.unwrap_or_default(),
            grand_total_after: dec(&totals.grand_total),
            note: Some(&dto.note),
            recorded_by: user_id,
            recorded_at: now,
        },
        &changed,
    )
    .await
}

// ───────────────────────── mobile ─────────────────────────

/// Node: src/orders/order-deliveries.service.ts:640 `mobileConfig`.
pub async fn mobile_config(state: &AppState, user_id: i32) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let rows: Vec<(String, String, String, i32)> = sqlx::query_as(
        r#"SELECT uuid::text, code, name, "sortOrder" FROM "orderReason"
            WHERE "companyId" = $1 AND "reasonType" = 'delivery_adjustment' AND "isActive" = true
            ORDER BY "sortOrder" ASC, name ASC"#,
    )
    .bind(company_id)
    .fetch_all(&state.db)
    .await?;
    Ok(json!({
        "reasons": rows.iter().map(|(id, code, name, s)| json!({ "id": id, "code": code, "name": name, "sortOrder": s })).collect::<Vec<_>>(),
    }))
}

/// Klausa scope list (`OR: [{ storeId: null }, { store: storeWhere }]`); `None` = tanpa klausa.
fn push_order_scope(qb: &mut QueryBuilder<'_, Postgres>, scope: &StoreScope) {
    if scope.is_none() {
        return;
    }
    qb.push(
        r#" AND (o."storeId" IS NULL OR EXISTS (SELECT 1 FROM store ss WHERE ss.id = o."storeId""#,
    );
    push_scope_clause(qb, scope, r#"ss."groupId""#);
    qb.push("))");
}

/// Node: src/orders/order-deliveries.service.ts:654 `mobileList` — delta keyset tanpa filter owner.
pub async fn mobile_list(
    state: &AppState,
    user_id: i32,
    cursor: Option<&str>,
    limit: i64,
    status: Option<&[String]>,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let scope = resolve_store_scope(&state.db, user_id, company_id, None).await?;
    let upper = now_naive() - chrono::Duration::milliseconds(ORDER_DELTA_SAFETY_LAG_MS);
    let decoded = cursor.map(decode_order_cursor).transpose()?;
    let statuses: Vec<String> = match status.filter(|s| !s.is_empty()) {
        Some(s) => s.to_vec(),
        None => DELIVERY_LIST_STATUSES
            .iter()
            .map(|s| (*s).to_owned())
            .collect(),
    };
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
        r#"SELECT o.id, o.uuid::text, o."updatedAt" FROM "order" o WHERE o."companyId" = "#,
    );
    qb.push_bind(company_id)
        .push(" AND o.status::text = ANY(")
        .push_bind(statuses)
        .push(r#") AND o."updatedAt" <= "#)
        .push_bind(upper);
    push_order_scope(&mut qb, &scope);
    if let Some((at, id)) = decoded {
        qb.push(r#" AND (o."updatedAt" > "#)
            .push_bind(at)
            .push(r#" OR (o."updatedAt" = "#)
            .push_bind(at)
            .push(" AND o.id > ")
            .push_bind(id)
            .push("))");
    }
    qb.push(r#" ORDER BY o."updatedAt" ASC, o.id ASC LIMIT "#)
        .push_bind(limit + 1);
    let rows: Vec<(i32, String, Option<NaiveDateTime>)> =
        qb.build_query_as().fetch_all(&state.db).await?;
    let has_more = i64::try_from(rows.len()).unwrap_or(i64::MAX) > limit;
    let page: Vec<_> = rows
        .into_iter()
        .take(usize::try_from(limit).unwrap_or(0))
        .collect();
    let mut data = Vec::with_capacity(page.len());
    for (_, uuid, _) in &page {
        data.push(service::find_one_in(state, company_id, uuid).await?);
    }
    let next = match page.last() {
        Some((id, _, Some(at))) if has_more => Value::String(encode_order_cursor(*at, *id)),
        _ => Value::Null,
    };
    Ok(json!({ "data": data, "meta": { "nextCursor": next, "hasMore": has_more } }))
}

/// Node: src/orders/order-deliveries.service.ts:698 `mobileFindOne` — di luar scope / status lain = 404.
pub async fn mobile_find_one(state: &AppState, user_id: i32, uuid: &str) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let scope = resolve_store_scope(&state.db, user_id, company_id, None).await?;
    let row: Option<(i32,)> = sqlx::query_as(
        r#"SELECT id FROM "order" WHERE uuid = $1 AND "companyId" = $2 AND status IN ('approved', 'delivered') LIMIT 1"#,
    )
    .bind(crate::common::row_uuid(uuid)?)
    .bind(company_id)
    .fetch_optional(&state.db)
    .await?;
    let Some((id,)) = row else {
        return Err(order_not_found());
    };
    if !order_in_scope(&scope, order_store_group(&state.db, id).await?) {
        return Err(order_not_found());
    }
    service::find_one_in(state, company_id, uuid).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivered_at_rules() {
        let now = chrono::DateTime::from_timestamp_millis(1_000_000_000_000)
            .unwrap()
            .naive_utc();
        let approved = chrono::DateTime::from_timestamp_millis(999_000_000_000)
            .unwrap()
            .naive_utc();
        assert!(has_code(
            &resolve_delivered_at(1_000_000_000_000 + 120_001, Some(approved), now).unwrap_err(),
            "DELIVERED_AT_IN_FUTURE"
        ));
        assert_eq!(
            resolve_delivered_at(1_000_000_000_000 + 60_000, Some(approved), now).unwrap(),
            now
        );
        assert!(has_code(
            &resolve_delivered_at(998_000_000_000, Some(approved), now).unwrap_err(),
            "DELIVERED_AT_BEFORE_APPROVAL"
        ));
    }

    #[test]
    fn scope_rules() {
        let strict = Some(mz_store_visits::scope::Scope {
            group_ids: vec![1],
            strict: true,
        });
        assert!(order_in_scope(&strict, None));
        assert!(order_in_scope(&strict, Some(Some(1))));
        assert!(!order_in_scope(&strict, Some(Some(2))));
        assert!(!order_in_scope(&strict, Some(None)));
        assert!(order_in_scope(&None, Some(Some(2))));
    }
}
