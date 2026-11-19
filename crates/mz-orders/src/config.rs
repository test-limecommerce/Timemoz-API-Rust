//! Port `src/orders/order-config.service.ts` (admin, FB4) dan `MobileOrdersService.getConfig` (§10.1):
//! master aktif, definisi custom field, batas diskon, `catalogEpoch` (+ `timeZone` admin).

use mz_core::company::require_company_id;
use mz_core::time::JsDate;
use mz_core::{ApiResult, AppState};
use rust_decimal::Decimal;
use serde_json::{Value, json};

use crate::common::internal;
use crate::custom_fields::{OrderCfRow, active_rows};
use crate::masters::{MasterKind, list_active};
use crate::pricing::dstr;

/// `withoutDeliveryReasons` (specs/003 U1).
pub fn without_delivery_reasons(reasons: Vec<Value>) -> Vec<Value> {
    reasons
        .into_iter()
        .filter(|r| r.get("reasonType").and_then(Value::as_str) != Some("delivery_adjustment"))
        .collect()
}

/// `toCustomFieldEntity` versi admin: `createdAt`/`updatedAt` = `Date` (ISO), bukan epoch-ms.
fn admin_cf_entity(row: &OrderCfRow) -> Value {
    json!({
        "id": row.uuid,
        "entity": row.entity,
        "key": row.key,
        "label": row.label,
        "type": row.field_type,
        "options": row.options.clone().unwrap_or(Value::Null),
        "isRequired": row.is_required,
        "isSensitive": row.is_sensitive,
        "sortOrder": row.sort_order,
        "isActive": row.is_active,
        "createdAt": JsDate(row.created_at),
        "updatedAt": row.updated_at.map(JsDate),
    })
}

async fn company_bits(
    state: &AppState,
    company_id: i32,
) -> ApiResult<(Option<Decimal>, i32, String)> {
    let row: Option<(Option<Decimal>, i32, String)> = sqlx::query_as(
        r#"SELECT "maxOrderDiscountPercent", "catalogEpoch", "timeZone" FROM company WHERE id = $1"#,
    )
    .bind(company_id)
    .fetch_optional(&state.db)
    .await?;
    row.ok_or_else(|| internal("No company found"))
}

/// Node: src/orders/order-config.service.ts:31 `getConfig` (admin) / mobile-orders.service.ts:52.
pub async fn get_config(state: &AppState, user_id: i32, admin: bool) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let (max, epoch, tz) = company_bits(state, company_id).await?;
    let types = list_active(state, MasterKind::Types, company_id).await?;
    let reasons = list_active(state, MasterKind::Reasons, company_id).await?;
    let terms = list_active(state, MasterKind::PaymentTerms, company_id).await?;
    let methods = list_active(state, MasterKind::PaymentMethods, company_id).await?;
    let order_defs = active_rows(&state.db, company_id, "order", !admin).await?;
    let item_defs = active_rows(&state.db, company_id, "orderItem", !admin).await?;
    let cf = |rows: &[OrderCfRow]| -> Vec<Value> {
        rows.iter()
            .map(|r| {
                if admin {
                    admin_cf_entity(r)
                } else {
                    crate::custom_fields::to_entity(r)
                }
            })
            .collect()
    };
    let mut v = json!({
        "orderTypes": types,
        "orderReasons": without_delivery_reasons(reasons),
        "orderPaymentTerms": terms,
        "orderPaymentMethods": methods,
        "orderCustomFields": cf(&order_defs),
        "orderItemCustomFields": cf(&item_defs),
        "maxOrderDiscountPercent": max.map(dstr),
        "catalogEpoch": epoch,
    });
    if admin && let Some(o) = v.as_object_mut() {
        o.insert("timeZone".into(), Value::String(tz));
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivery_reasons_filtered() {
        let r = without_delivery_reasons(vec![
            json!({"reasonType": "no_order"}),
            json!({"reasonType": "delivery_adjustment"}),
            json!({"reasonType": "order_cancel"}),
        ]);
        assert_eq!(r.len(), 2);
    }
}
