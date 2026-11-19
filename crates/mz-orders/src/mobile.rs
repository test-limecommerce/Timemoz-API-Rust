//! Port `src/orders/mobile-orders.service.ts` (§10: create+submit idempoten, delta, detail milik
//! sendiri) dan `src/orders/store-visit-no-order.service.ts` (§0.17).

use std::collections::HashSet;

use chrono::NaiveDateTime;
use mz_audit::AuditHandle;
use mz_core::company::require_company_id;
use mz_core::time::now_naive;
use mz_core::{ApiResult, AppState};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use sqlx::PgConnection;

use crate::common::{
    conflict, has_code, internal, ms, not_found, order_not_found, unprocessable,
    unprocessable_details,
};
use crate::cursor::{ORDER_DELTA_SAFETY_LAG_MS, decode_order_cursor, encode_order_cursor};
use crate::dto::{CreateOrder, ItemInput};
use crate::masters::{self, MasterKind};
use crate::pricing::{dec, dstr};
use crate::service::{self, BeforeCommit};

/// Node: src/orders/mobile-orders.service.ts:166 `assertPriceTolerance` (§5.5).
pub async fn assert_price_tolerance(
    tx: &mut PgConnection,
    order_id: i32,
    company_id: i32,
    inputs: &[ItemInput],
) -> ApiResult<()> {
    let max: Option<Option<Decimal>> =
        sqlx::query_scalar(r#"SELECT "maxOrderDiscountPercent" FROM company WHERE id = $1"#)
            .bind(company_id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(max) = max.ok_or_else(|| internal("No company found"))? else {
        return Ok(());
    };
    let prices: Vec<Decimal> = sqlx::query_scalar(
        r#"SELECT "unitPrice" FROM "orderItem" WHERE "orderId" = $1 ORDER BY "lineNo" ASC"#,
    )
    .bind(order_id)
    .fetch_all(&mut *tx)
    .await?;
    for (index, server) in prices.iter().enumerate() {
        let Some(client) = inputs.get(index).and_then(|i| i.unit_price.as_deref()) else {
            continue;
        };
        let client_price = dec(client);
        if client_price >= *server || server.is_zero() {
            continue;
        }
        let diff = crate::pricing::mul(
            crate::pricing::div(crate::pricing::sub(*server, client_price), *server),
            Decimal::ONE_HUNDRED,
        );
        if diff > max {
            let n = index + 1;
            return Err(unprocessable_details(
                "PRICE_MISMATCH",
                format!(
                    "unitPrice for item {n} is below the server price beyond company.maxOrderDiscountPercent (§5.5)"
                ),
                json!({ "lineNo": n, "serverUnitPrice": dstr(*server), "clientUnitPrice": client }),
            ));
        }
    }
    Ok(())
}

/// `attachPriceAdjusted` — `priceAdjusted` ditambahkan pada item yang membawa `unitPrice`.
pub fn attach_price_adjusted(mut order: Value, inputs: &[ItemInput]) -> Value {
    if let Some(items) = order.get_mut("items").and_then(Value::as_array_mut) {
        for (index, item) in items.iter_mut().enumerate() {
            let Some(client) = inputs.get(index).and_then(|i| i.unit_price.as_deref()) else {
                continue;
            };
            let server = item
                .get("unitPrice")
                .and_then(Value::as_str)
                .map(dec)
                .unwrap_or_default();
            if let Some(o) = item.as_object_mut() {
                o.insert("priceAdjusted".into(), Value::Bool(server != dec(client)));
            }
        }
    }
    order
}

/// Node: src/orders/mobile-orders.service.ts:113 `create` → `(order, isNew)` (201 vs 200).
pub async fn create(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    header: &CreateOrder,
    items: &[ItemInput],
) -> ApiResult<(Value, bool)> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let dto = CreateOrder {
        owner_id: None,
        ..header.clone()
    };
    let before = BeforeCommit::MobileSubmit { user_id, items };
    match service::create_with_items(state, audit, user_id, &dto, items, "mobile", before).await {
        Ok(order) => Ok((attach_price_adjusted(order, items), true)),
        Err(e) if has_code(&e, "ORDER_CLIENT_REQUEST_CONFLICT") => {
            let existing: Option<(String, String)> = sqlx::query_as(
                r#"SELECT uuid::text, status::text FROM "order"
                    WHERE "companyId" = $1 AND "ownerId" = $2 AND "clientRequestId" = $3::uuid LIMIT 1"#,
            )
            .bind(company_id)
            .bind(user_id)
            .bind(&dto.client_request_id)
            .fetch_optional(&state.db)
            .await?;
            let Some((uuid, status)) = existing else {
                return Err(e);
            };
            if status != "draft" {
                let full = service::find_one_in(state, company_id, &uuid).await?;
                return Ok((attach_price_adjusted(full, items), false));
            }
            let resumed = service::replace_items(
                state,
                audit,
                user_id,
                &uuid,
                items,
                &HashSet::new(),
                BeforeCommit::MobileSubmit { user_id, items },
            )
            .await?;
            Ok((attach_price_adjusted(resumed, items), false))
        }
        Err(e) => Err(e),
    }
}

/// Node: src/orders/mobile-orders.service.ts:252 `findOne` — milik sendiri saja (lainnya 404).
pub async fn find_one(state: &AppState, user_id: i32, uuid: &str) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let owned: Option<i32> = sqlx::query_scalar(
        r#"SELECT id FROM "order" WHERE uuid = $1 AND "companyId" = $2 AND "ownerId" = $3 LIMIT 1"#,
    )
    .bind(crate::common::row_uuid(uuid)?)
    .bind(company_id)
    .bind(user_id)
    .fetch_optional(&state.db)
    .await?;
    if owned.is_none() {
        return Err(order_not_found());
    }
    service::find_one_in(state, company_id, uuid).await
}

/// Node: src/orders/mobile-orders.service.ts:268 `findAll` — keyset `(updatedAt, id)`, lag 5 dtk.
pub async fn find_all(
    state: &AppState,
    user_id: i32,
    cursor: Option<&str>,
    limit: i64,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let upper = now_naive() - chrono::Duration::milliseconds(ORDER_DELTA_SAFETY_LAG_MS);
    let decoded = cursor.map(decode_order_cursor).transpose()?;
    let mut qb: sqlx::QueryBuilder<sqlx::Postgres> = sqlx::QueryBuilder::new(
        r#"SELECT o.id, o."updatedAt" FROM "order" o WHERE o."companyId" = "#,
    );
    qb.push_bind(company_id)
        .push(r#" AND o."ownerId" = "#)
        .push_bind(user_id)
        .push(r#" AND o."updatedAt" <= "#)
        .push_bind(upper);
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
    let rows: Vec<(i32, Option<NaiveDateTime>)> = qb.build_query_as().fetch_all(&state.db).await?;
    let has_more = i64::try_from(rows.len()).unwrap_or(i64::MAX) > limit;
    let page: Vec<_> = rows
        .into_iter()
        .take(usize::try_from(limit).unwrap_or(0))
        .collect();
    let ids: Vec<i32> = page.iter().map(|r| r.0).collect();
    let data = crate::entity::find_many_detailed(&state.db, company_id, &ids).await?;
    let next = match page.last() {
        Some((id, Some(at))) if has_more => Value::String(encode_order_cursor(*at, *id)),
        _ => Value::Null,
    };
    Ok(json!({ "data": data, "meta": { "nextCursor": next, "hasMore": has_more } }))
}

// ───────────────────────── store-visit-no-order.service.ts ─────────────────────────

fn no_order_entity(
    id: &str,
    visit_uuid: &str,
    reason: (&str, &str, &str),
    note: Option<&str>,
    created: NaiveDateTime,
) -> Value {
    json!({
        "id": id,
        "storeVisitId": visit_uuid,
        "reason": { "id": reason.0, "code": reason.1, "name": reason.2 },
        "note": note,
        "createdAt": ms(created),
    })
}

/// Node: src/orders/store-visit-no-order.service.ts:27 `create`.
pub async fn create_no_order(
    state: &AppState,
    user_id: i32,
    visit_uuid: &str,
    reason_id: &str,
    note: Option<&str>,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let visit: Option<(i32, String, i32, i32)> = sqlx::query_as(
        r#"SELECT id, uuid::text, "userId", "storeId" FROM "storeVisit" WHERE uuid = $1 AND "companyId" = $2 LIMIT 1"#,
    )
    .bind(crate::common::row_uuid(visit_uuid)?)
    .bind(company_id)
    .fetch_optional(&state.db)
    .await?;
    let (visit_id, v_uuid, visit_user, store_id) =
        visit.ok_or_else(|| not_found("STORE_VISIT_NOT_FOUND", "Store visit not found"))?;
    if visit_user != user_id {
        return Err(unprocessable(
            "STORE_VISIT_OWNERSHIP_MISMATCH",
            "Store visit does not belong to the caller (§13.9)",
        ));
    }
    let reason = masters::find_by_uuid(&state.db, MasterKind::Reasons, company_id, reason_id)
        .await?
        .ok_or_else(|| not_found("ORDER_MASTER_NOT_FOUND", "Order reason not found"))?;
    if !reason.is_active {
        return Err(unprocessable(
            "ORDER_MASTER_ARCHIVED",
            "Order reason is archived",
        ));
    }
    if reason.reason_type.as_deref() != Some("no_order") {
        return Err(unprocessable(
            "REASON_TYPE_MISMATCH",
            "reasonId must reference a no_order reason",
        ));
    }
    let inserted: Result<(String, NaiveDateTime), sqlx::Error> = sqlx::query_as(
        r#"INSERT INTO "storeVisitNoOrder" ("companyId", "storeVisitId", "storeId", "userId", "orderReasonId", note)
           VALUES ($1, $2, $3, $4, $5, $6) RETURNING uuid::text, "createdAt""#,
    )
    .bind(company_id)
    .bind(visit_id)
    .bind(store_id)
    .bind(user_id)
    .bind(reason.id)
    .bind(note)
    .fetch_one(&state.db)
    .await;
    match inserted {
        Ok((uuid, created)) => Ok(no_order_entity(
            &uuid,
            &v_uuid,
            (&reason.uuid, &reason.code, &reason.name),
            note,
            created,
        )),
        Err(e) if mz_core::db::is_unique_violation(&e) => Err(conflict(
            "STORE_VISIT_NO_ORDER_EXISTS",
            "This visit already has a no-order record — one per visit (§3.7)",
        )),
        Err(e) => Err(e.into()),
    }
}

/// Node: src/orders/store-visit-no-order.service.ts:89 `findForVisit` (admin, scope company saja).
pub async fn find_no_order(state: &AppState, user_id: i32, visit_uuid: &str) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let visit: Option<(i32, String)> = sqlx::query_as(
        r#"SELECT id, uuid::text FROM "storeVisit" WHERE uuid = $1 AND "companyId" = $2 LIMIT 1"#,
    )
    .bind(crate::common::row_uuid(visit_uuid)?)
    .bind(company_id)
    .fetch_optional(&state.db)
    .await?;
    let (visit_id, v_uuid) =
        visit.ok_or_else(|| not_found("STORE_VISIT_NOT_FOUND", "Store visit not found"))?;
    #[allow(clippy::type_complexity)]
    let row: Option<(
        String,
        Option<String>,
        NaiveDateTime,
        String,
        String,
        String,
    )> = sqlx::query_as(
        r#"SELECT n.uuid::text, n.note, n."createdAt", r.uuid::text, r.code, r.name
             FROM "storeVisitNoOrder" n JOIN "orderReason" r ON r.id = n."orderReasonId"
            WHERE n."storeVisitId" = $1"#,
    )
    .bind(visit_id)
    .fetch_optional(&state.db)
    .await?;
    let (uuid, note, created, r_uuid, r_code, r_name) = row.ok_or_else(|| {
        not_found(
            "STORE_VISIT_NO_ORDER_NOT_FOUND",
            "This visit has no recorded \"no order\" reason",
        )
    })?;
    Ok(no_order_entity(
        &uuid,
        &v_uuid,
        (&r_uuid, &r_code, &r_name),
        note.as_deref(),
        created,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(price: Option<&str>) -> ItemInput {
        ItemInput {
            unit_price: price.map(Into::into),
            ..Default::default()
        }
    }

    // Node: src/orders/mobile-orders.service.spec.ts (priceAdjusted)
    #[test]
    fn price_adjusted_marker() {
        let order =
            json!({"items": [{"unitPrice": "15000"}, {"unitPrice": "15000"}, {"unitPrice": "1"}]});
        let out = attach_price_adjusted(
            order,
            &[input(Some("15000.00")), input(Some("14000")), input(None)],
        );
        assert_eq!(out["items"][0]["priceAdjusted"], false);
        assert_eq!(out["items"][1]["priceAdjusted"], true);
        assert!(out["items"][2].get("priceAdjusted").is_none());
        let no_items = attach_price_adjusted(json!({"id": "x"}), &[input(Some("1"))]);
        assert_eq!(no_items, json!({"id": "x"}));
    }
}
