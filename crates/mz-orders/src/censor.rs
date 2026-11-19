//! Port `src/orders/order-response-censor.ts` + interceptor `@TableFields('orders', { path: 'data' })`:
//! header order disensor tabel `orders`, `items[]` tabel `order_items`, `deliveryHistory[]` dikosongkan
//! bila `grandTotal`/`finalGrandTotal` tersensor (R8).

use std::sync::Arc;

use mz_core::{ApiResult, AppState, RequestCtx};
use mz_table_fields::apply::{apply_field_policy, censor_response_body};
use mz_table_fields::policy::{CensoredColumns, policy_for_request};
use serde_json::Value;

/// Kolom tersensor role caller untuk `orders` dan `order_items`.
pub struct OrderPolicy {
    pub orders: Arc<CensoredColumns>,
    pub items: Arc<CensoredColumns>,
}

/// `policies.censoredColumns(req, 'orders' | 'order_items')`.
pub async fn policy(state: &AppState, ctx: &RequestCtx) -> ApiResult<OrderPolicy> {
    let p = policy_for_request(state, ctx, false).await?;
    Ok(OrderPolicy {
        orders: p.for_table("orders"),
        items: p.for_table("order_items"),
    })
}

const HISTORY_MONEY_KEYS: [&str; 2] = ["grandTotalBefore", "grandTotalAfter"];

/// Node: src/orders/order-response-censor.ts:26 `censorDeliveryHistory`.
pub fn censor_delivery_history(history: &mut Value, orders: &CensoredColumns) {
    let Some(entries) = history.as_array_mut() else {
        return;
    };
    if entries.is_empty()
        || (!orders.contains_key("grandTotal") && !orders.contains_key("finalGrandTotal"))
    {
        return;
    }
    for e in entries.iter_mut().filter_map(Value::as_object_mut) {
        for k in HISTORY_MONEY_KEYS {
            if let Some(slot) = e.get_mut(k) {
                *slot = Value::Null;
            }
        }
    }
}

/// Node: src/orders/order-response-censor.ts:42 `censorOrderChildren` — satu record order.
pub fn censor_order_children(order: &mut Value, p: &OrderPolicy) {
    let Some(o) = order.as_object_mut() else {
        return;
    };
    if let Some(items) = o
        .get_mut("items")
        .filter(|v| v.as_array().is_some_and(|a| !a.is_empty()))
    {
        apply_field_policy(items, "order_items", &p.items, None);
    }
    if let Some(h) = o.get_mut("deliveryHistory") {
        censor_delivery_history(h, &p.orders);
    }
}

/// `{ data }` dengan anak order disensor lalu interceptor `orders` (`fieldPolicy` ditempel).
pub fn envelope_one(mut data: Value, p: &OrderPolicy) -> Value {
    censor_order_children(&mut data, p);
    let mut body = serde_json::json!({ "data": data });
    censor_response_body(&mut body, "orders", "data", &p.orders, None);
    body
}

/// Interceptor `orders` saja untuk body list (`{ data: [...], meta }`).
pub fn envelope_list(mut body: Value, p: &OrderPolicy, children: bool) -> Value {
    if children && let Some(rows) = body.get_mut("data").and_then(Value::as_array_mut) {
        for r in rows {
            censor_order_children(r, p);
        }
    }
    censor_response_body(&mut body, "orders", "data", &p.orders, None);
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use mz_table_fields::registry::FieldVisibilityMode;
    use serde_json::json;

    fn cc(pairs: &[(&str, FieldVisibilityMode)]) -> Arc<CensoredColumns> {
        Arc::new(pairs.iter().map(|(k, m)| ((*k).to_owned(), *m)).collect())
    }

    // Node: src/orders/order-field-visibility.spec.ts
    #[test]
    fn detail_censors_header_items_and_history() {
        let p = OrderPolicy {
            orders: cc(&[("grandTotal", FieldVisibilityMode::Hidden)]),
            items: cc(&[("unitPrice", FieldVisibilityMode::Masked)]),
        };
        let data = json!({
            "id": "a", "grandTotal": "111000", "subtotal": "100000",
            "items": [{"id": "b", "unitPrice": "100000", "lineTotal": "111000"}],
            "deliveryHistory": [{"id": "c", "grandTotalBefore": "111000", "grandTotalAfter": "66600", "lines": []}],
        });
        let body = envelope_one(data, &p);
        assert_eq!(body["data"]["grandTotal"], Value::Null);
        assert_eq!(body["data"]["subtotal"], "100000");
        assert_eq!(body["data"]["items"][0]["unitPrice"], Value::Null);
        assert_eq!(body["data"]["items"][0]["lineTotal"], "111000");
        assert_eq!(
            body["data"]["deliveryHistory"][0]["grandTotalBefore"],
            Value::Null
        );
        assert_eq!(
            body["data"]["deliveryHistory"][0]["grandTotalAfter"],
            Value::Null
        );
        assert_eq!(body["fieldPolicy"]["tableKey"], "orders");
    }

    #[test]
    fn no_policy_leaves_body_untouched() {
        let p = OrderPolicy {
            orders: cc(&[]),
            items: cc(&[]),
        };
        let body = envelope_one(
            json!({"grandTotal": "1", "deliveryHistory": [{"grandTotalBefore": "1"}]}),
            &p,
        );
        assert_eq!(
            body,
            json!({"data": {"grandTotal": "1", "deliveryHistory": [{"grandTotalBefore": "1"}]}})
        );
    }
}
