//! `cancelSubscription`, `resumeSubscription`, `reactivateSubscription`, `subscribe`
//! (`POST /invoices/{cancel,resume,reactivate}-subscription`, `POST /invoices/subscribe`).

use mz_audit::AuditHandle;
use mz_core::jsfmt::{num, num_string};
use mz_core::time::now_naive;
use mz_core::{ApiResult, AppState};
use mz_paypal::{PaypalClient, get_approval_url};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::db::{
    V, cancel_unpaid_invoices, invoice_status, paypal_source, sub_status, update_by_id,
};
use crate::load;
use crate::paypal::{NewSub, activate_free, create_subscription, str_field};
use crate::util::{Fail, add_days, assert_not_manual, date_json, dmy, fail_msg, finish, iso};

/// Node: invoices.service.ts:1161 (`cancelSubscription`). Trial dengan langganan PayPal: dibatalkan
/// keras dan kembali ke TRIAL. Selain itu: tagih invoice SUBSCRIPTION yang belum dibayar, matikan
/// `autoRenew` (SEBELUM suspend, supaya webhook SUSPENDED tahu ini pembatalan akhir periode), lalu
/// suspend di PayPal.
pub async fn cancel(state: &AppState, user_id: i32) -> ApiResult<Value> {
    finish(
        cancel_run(state, user_id).await,
        "Failed to cancel subscription",
        "cancelSubscription",
    )
}

async fn cancel_run(state: &AppState, user_id: i32) -> Result<Value, Fail> {
    let Some(ctx) = load::context(state, user_id).await? else {
        return Ok(fail_msg("User not found"));
    };
    let Some(company) = ctx.company else {
        return Ok(fail_msg("Company not found"));
    };
    assert_not_manual(state, company.id).await?;
    let Some(sw) = ctx.subscription else {
        return Ok(fail_msg("No active subscription found"));
    };
    let sub = sw.sub;
    if !sub.auto_renew {
        return Ok(fail_msg("Subscription is already set to not renew"));
    }
    let client = PaypalClient::from_state(state);

    // === TRIAL CANCEL FLOW === belum ada pembayaran: batalkan keras, kembali ke TRIAL.
    if company.is_trial
        && let Some(pp_id) = sub.paypal_id()
    {
        client
            .cancel_subscription(
                pp_id,
                Some("Trial subscription cancelled by user before billing started"),
            )
            .await?;
        let mut tx = state.db.begin().await?;
        cancel_unpaid_invoices(&mut tx, sub.id).await?;
        update_by_id(
            &mut tx,
            "subscriptions",
            sub.id,
            &[
                ("status", sub_status("TRIAL")),
                ("paypalSubscriptionId", V::OptStr(None)),
                ("paypalPlanId", V::OptStr(None)),
                ("paypalStatus", V::OptStr(None)),
                ("autoRenew", V::Bool(false)),
                ("nextInvoiceDate", V::OptTs(None)),
            ],
        )
        .await?;
        tx.commit().await?;
        load::invalidate_subscription_lock(state, company.id).await;
        return Ok(json!({
            "success": true,
            "message": "PayPal subscription cancelled. You can continue using the service until the end of your trial period.",
            "data": { "endDate": date_json(sub.end_date), "autoRenew": false },
        }));
    }

    // === NORMAL CANCEL FLOW ===
    // Tagih invoice SUBSCRIPTION yang belum dibayar sebelum suspend (PRORATION memakai Order sekali
    // bayar, tidak ikut).
    let unpaid: Vec<(i32, String, f64)> = sqlx::query_as(
        r#"SELECT id, "invoiceNumber", "totalPrice" FROM invoices
            WHERE "subscriptionId" = $1 AND status = 'UNPAID'::"InvoiceStatus"
              AND "invoiceType" = 'SUBSCRIPTION'::"InvoiceType"
            ORDER BY id"#,
    )
    .bind(sub.id)
    .fetch_all(&state.db)
    .await?;
    // `captureSubscriptionPayment(subscription.paypalSubscriptionId, …)`: id null → `/null/capture`.
    let capture_id = sub.paypal_subscription_id.as_deref().unwrap_or("null");
    for (inv_id, number, total) in unpaid {
        let captured = async {
            client
                .capture_subscription_payment(
                    capture_id,
                    total,
                    &format!("Outstanding payment: {number}"),
                )
                .await
                .map_err(|e| e.message)?;
            let mut conn = state.db.acquire().await.map_err(|e| e.to_string())?;
            update_by_id(
                &mut conn,
                "invoices",
                inv_id,
                &[("status", invoice_status("PAID"))],
            )
            .await
            .map_err(|e| format!("{e:?}"))?;
            Ok::<(), String>(())
        };
        match captured.await {
            Ok(()) => tracing::info!(
                "Captured outstanding invoice {number} on cancel — ${}",
                mz_core::jsfmt::to_fixed(total, 2)
            ),
            Err(e) => tracing::error!("Failed to capture invoice {number} on cancel: {e}"),
        }
    }

    // autoRenew=false SEBELUM suspend: handler webhook BILLING.SUBSCRIPTION.SUSPENDED melihatnya dan
    // mempertahankan status ACTIVE (pembatalan akhir periode).
    {
        let mut conn = state.db.acquire().await?;
        update_by_id(
            &mut conn,
            "subscriptions",
            sub.id,
            &[
                ("autoRenew", V::Bool(false)),
                ("nextInvoiceDate", V::OptTs(None)),
            ],
        )
        .await?;
    }
    load::invalidate_subscription_lock(state, company.id).await;
    if let Some(pp_id) = sub.paypal_id() {
        client
            .suspend_subscription(pp_id, Some("Auto-renewal disabled by user"))
            .await?;
    }

    let tier_name = sw
        .tier
        .as_ref()
        .map(|t| t.name.as_str())
        .filter(|n| !n.is_empty())
        .unwrap_or("-");
    mz_core::mail::spawn_send(
        state,
        crate::mail::subscription_cancellation(
            state,
            &ctx.user.username,
            &ctx.user.name,
            tier_name,
            &dmy(sub.end_date),
        ),
    );

    Ok(json!({
        "success": true,
        "message": "Auto-renewal disabled. You can still use the service until the end of your current billing period.",
        "data": { "endDate": date_json(sub.end_date), "autoRenew": false },
    }))
}

/// Langganan → ACTIVE + `autoRenew` + `nextInvoiceDate = endDate - 3 hari`.
async fn reenable(
    state: &AppState,
    sub_id: i32,
    end: chrono::NaiveDateTime,
) -> Result<chrono::NaiveDateTime, Fail> {
    let next = add_days(end, -3);
    let mut conn = state.db.acquire().await?;
    update_by_id(
        &mut conn,
        "subscriptions",
        sub_id,
        &[
            ("status", sub_status("ACTIVE")),
            ("paypalStatus", V::Str("ACTIVE".into())),
            ("autoRenew", V::Bool(true)),
            ("nextInvoiceDate", V::Ts(next)),
        ],
    )
    .await?;
    Ok(next)
}

/// Node: invoices.service.ts:1346 (`resumeSubscription`).
pub async fn resume(state: &AppState, user_id: i32) -> ApiResult<Value> {
    finish(
        resume_run(state, user_id).await,
        "Failed to resume subscription",
        "resumeSubscription",
    )
}

async fn resume_run(state: &AppState, user_id: i32) -> Result<Value, Fail> {
    let Some(ctx) = load::context(state, user_id).await? else {
        return Ok(fail_msg("User not found"));
    };
    let Some(company) = ctx.company else {
        return Ok(fail_msg("Company not found"));
    };
    assert_not_manual(state, company.id).await?;
    let Some(sw) = ctx.subscription else {
        return Ok(fail_msg("No active subscription found"));
    };
    let sub = sw.sub;
    if sub.auto_renew {
        return Ok(fail_msg("Subscription is already set to auto-renew"));
    }
    if now_naive() > sub.end_date {
        return Ok(fail_msg(
            "Subscription period has already ended. Please subscribe again.",
        ));
    }
    // Aktifkan lagi di PayPal (di-suspend saat cancel).
    if let Some(pp_id) = sub.paypal_id() {
        PaypalClient::from_state(state)
            .activate_subscription(pp_id, Some("Auto-renewal re-enabled by user"))
            .await?;
    }
    let next = reenable(state, sub.id, sub.end_date).await?;
    load::invalidate_subscription_lock(state, company.id).await;

    mz_core::mail::spawn_send(
        state,
        crate::mail::resume_subscription(
            state,
            &ctx.user.username,
            &ctx.user.name,
            &dmy(sub.end_date),
        ),
    );
    Ok(json!({
        "success": true,
        "message": "Subscription resumed. Auto-renewal has been re-enabled.",
        "data": {
            "endDate": date_json(sub.end_date),
            "autoRenew": true,
            "nextInvoiceDate": date_json(next),
        },
    }))
}

/// `${paypalSub.status}` (JS `String(x)`).
fn js_string(v: Option<&Value>) -> String {
    match v {
        None => "undefined".into(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) => "null".into(),
        Some(Value::Number(n)) => n.as_f64().map_or_else(|| n.to_string(), num_string),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| match v {
                Value::Null => String::new(),
                other => js_string(Some(other)),
            })
            .collect::<Vec<_>>()
            .join(","),
        Some(Value::Object(_)) => "[object Object]".into(),
        Some(other) => other.to_string(),
    }
}

/// Node: invoices.service.ts:1462 (`reactivateSubscription`).
pub async fn reactivate(state: &AppState, user_id: i32) -> ApiResult<Value> {
    finish(
        reactivate_run(state, user_id).await,
        "Failed to reactivate subscription",
        "reactivateSubscription",
    )
}

async fn reactivate_run(state: &AppState, user_id: i32) -> Result<Value, Fail> {
    let Some(ctx) = load::context(state, user_id).await? else {
        return Ok(fail_msg("User not found"));
    };
    let Some(company) = ctx.company else {
        return Ok(fail_msg("Company not found"));
    };
    assert_not_manual(state, company.id).await?;
    let Some(sw) = ctx.subscription else {
        return Ok(fail_msg("No subscription found"));
    };
    let sub = sw.sub;
    if sub.status != "SUSPENDED" {
        return Ok(fail_msg("Subscription is not suspended"));
    }
    let Some(pp_id) = sub.paypal_id() else {
        return Ok(fail_msg("No PayPal subscription linked"));
    };
    let client = PaypalClient::from_state(state);
    // Status PayPal sebenarnya dicek dulu.
    let paypal_sub = client.get_subscription(pp_id).await?;
    let status = paypal_sub.get("status");
    match status.and_then(Value::as_str) {
        Some("CANCELLED") => {
            return Ok(fail_msg(
                "PayPal subscription has been fully cancelled and cannot be reactivated. Please create a new subscription.",
            ));
        }
        Some("SUSPENDED") => {}
        _ => {
            return Ok(fail_msg(&format!(
                "Cannot reactivate subscription in status: {}",
                js_string(status)
            )));
        }
    }
    client
        .activate_subscription(pp_id, Some("Subscription reactivated by user"))
        .await?;
    let next = reenable(state, sub.id, sub.end_date).await?;
    load::invalidate_subscription_lock(state, company.id).await;

    mz_core::mail::spawn_send(
        state,
        crate::mail::reactivate_subscription(
            state,
            &ctx.user.username,
            &ctx.user.name,
            &dmy(sub.end_date),
        ),
    );
    Ok(json!({
        "success": true,
        "message": "Subscription reactivated successfully.",
        "data": {
            "endDate": date_json(sub.end_date),
            "autoRenew": true,
            "nextInvoiceDate": date_json(next),
        },
    }))
}

/// Node: invoices.service.ts:1589 (`subscribe`). Dari trial: langganan PayPal dengan penagihan
/// tertunda (`startTime` = akhir trial); invoice dibuat webhook `PAYMENT.SALE.COMPLETED`.
pub async fn subscribe(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    new_pricing_uuid: &Uuid,
) -> ApiResult<Value> {
    finish(
        subscribe_run(state, audit, user_id, new_pricing_uuid).await,
        "Failed to create subscription",
        "subscribe",
    )
}

async fn subscribe_run(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    new_pricing_uuid: &Uuid,
) -> Result<Value, Fail> {
    let Some(ctx) = load::context(state, user_id).await? else {
        return Ok(fail_msg("User not found"));
    };
    let Some(company) = ctx.company else {
        return Ok(fail_msg("Company not found"));
    };
    assert_not_manual(state, company.id).await?;
    if !company.is_trial {
        return Ok(fail_msg(
            "Only trial companies can subscribe through this API",
        ));
    }
    let Some(sw) = ctx.subscription else {
        return Ok(fail_msg("No subscription found"));
    };
    let sub = sw.sub;
    // `newPricingUuid` selalu ada (ChangePlanDto): `pricingTiers.findFirst({ where: { uuid } })`
    // tanpa filter status.
    let Some(tier) = load::tier_by_uuid(state, new_pricing_uuid, false).await? else {
        return Ok(fail_msg("New pricing tier not found"));
    };
    if tier.price_per_user_usd == 0.0 {
        return activate_free(state, audit, &sub, tier.id, company.id).await;
    }
    let Some(plan_id) = tier.plan_id().map(str::to_owned) else {
        return Ok(fail_msg("PayPal plan not configured for this pricing tier"));
    };

    let quantity = if sub.quantity == 0 { 1 } else { sub.quantity };
    let interval = tier.interval().to_owned();
    let mult = if interval == "YEAR" { 12.0 } else { 1.0 };
    let tax_price = 0.0;
    let unit_price_idr = tier.base_price * mult;
    let sub_price = unit_price_idr * f64::from(quantity);

    // Penagihan tertunda: PayPal baru menagih setelah trial berakhir.
    let paypal_sub = create_subscription(
        &PaypalClient::from_state(state),
        NewSub {
            plan_id: &plan_id,
            user: &ctx.user,
            company_id: company.id,
            return_url: state.config.var("PAYPAL_RETURN_URL").unwrap_or_default(),
            cancel_url: state.config.var("PAYPAL_CANCEL_URL").unwrap_or_default(),
            quantity: i64::from(quantity),
            start_time: Some(iso(sub.end_date)),
        },
    )
    .await?;
    let approval = get_approval_url(&paypal_sub);
    let new_id = str_field(&paypal_sub, "id");

    let mut sets = vec![
        ("status", sub_status("PENDING")),
        ("billingSource", paypal_source()),
    ];
    if let Some(id) = &new_id {
        sets.push(("paypalSubscriptionId", V::Str(id.clone())));
    }
    sets.push(("paypalPlanId", V::Str(plan_id)));
    if let Some(st) = str_field(&paypal_sub, "status") {
        sets.push(("paypalStatus", V::Str(st)));
    }
    sets.push(("pricingTierId", V::I32(tier.id)));
    {
        let mut conn = state.db.acquire().await?;
        update_by_id(&mut conn, "subscriptions", sub.id, &sets).await?;
    }
    load::invalidate_subscription_lock(state, company.id).await;

    let mut data = Map::new();
    data.insert("paymentUrl".into(), approval.into());
    if let Some(id) = new_id {
        data.insert("paypalSubscriptionId".into(), id.into());
    }
    data.insert("totalPrice".into(), num(sub_price + tax_price));
    Ok(json!({
        "success": true,
        "message": "Subscription created, please approve payment",
        "data": data,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_paypal_sebagai_string_js() {
        assert_eq!(js_string(None), "undefined");
        assert_eq!(js_string(Some(&json!("ACTIVE"))), "ACTIVE");
        assert_eq!(js_string(Some(&json!(null))), "null");
        assert_eq!(js_string(Some(&json!(3))), "3");
    }
}
