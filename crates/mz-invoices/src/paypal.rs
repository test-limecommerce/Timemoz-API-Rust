//! Pemakaian klien `mz-paypal` oleh alur invoice + aktivasi tier Free (`activateFreeTier`).

use mz_audit::AuditHandle;
use mz_core::AppState;
use mz_paypal::{CreateSubscriptionPayload, PaypalClient};
use serde_json::Value;

use crate::load::{SubRow, UserRow};
use crate::util::Fail;

/// Argumen `paypal.createSubscription({...})` yang dipakai semua alur invoice.
pub struct NewSub<'a> {
    pub plan_id: &'a str,
    pub user: &'a UserRow,
    pub company_id: i32,
    pub return_url: String,
    pub cancel_url: String,
    pub quantity: i64,
    pub start_time: Option<String>,
}

pub async fn create_subscription(client: &PaypalClient, p: NewSub<'_>) -> Result<Value, Fail> {
    Ok(client
        .create_subscription(&CreateSubscriptionPayload {
            plan_id: p.plan_id.to_owned(),
            subscriber_email: p.user.username.clone(),
            subscriber_name: p.user.name.clone(),
            custom_id: Some(format!("company-{}", p.company_id)),
            return_url: p.return_url,
            cancel_url: p.cancel_url,
            quantity: Some(p.quantity),
            start_time: p.start_time,
        })
        .await?)
}

/// `${PAYPAL_RETURN_URL}?invoice=${n}` / `${PAYPAL_CANCEL_URL}?invoice=${n}`.
pub fn invoice_urls(state: &AppState, invoice_number: &str) -> (String, String) {
    (
        format!(
            "{}?invoice={invoice_number}",
            crate::util::env_js(state, "PAYPAL_RETURN_URL")
        ),
        format!(
            "{}?invoice={invoice_number}",
            crate::util::env_js(state, "PAYPAL_CANCEL_URL")
        ),
    )
}

/// `obj.key` sebagai string (`undefined` bila tidak ada / bukan string).
pub fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// `this.activateFreeTier(subscription, newTier, companyId)` — hasil = objek Node.
pub async fn activate_free(
    state: &AppState,
    audit: &AuditHandle,
    sub: &SubRow,
    tier_id: i32,
    company_id: i32,
) -> Result<Value, Fail> {
    let tier = mz_billing::FreeTier::by_id(&state.db, tier_id)
        .await?
        .ok_or_else(|| Fail::Other(format!("pricing tier {tier_id} hilang")))?;
    let outcome = mz_billing::activate_free_tier(
        state,
        audit,
        &PaypalClient::from_state(state),
        &sub.free_tier_input(),
        &tier,
        company_id,
        None,
    )
    .await?;
    crate::load::invalidate_subscription_lock(state, company_id).await;
    Ok(outcome.to_json())
}
