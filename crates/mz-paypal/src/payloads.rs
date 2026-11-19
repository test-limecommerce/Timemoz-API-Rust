use serde_json::Value;

/// Node: paypal.service.ts:11 `CreatePlanPayload`.
#[derive(Debug, Clone, Default)]
pub struct CreatePlanPayload {
    pub product_id: String,
    pub name: String,
    pub description: String,
    pub price_usd: f64,
    /// `DAY|WEEK|MONTH|YEAR`; `None` = `MONTH`.
    pub interval_unit: Option<String>,
    /// `None` = 1.
    pub interval_count: Option<i64>,
}

/// Node: paypal.service.ts:21 `CreateSubscriptionPayload`.
#[derive(Debug, Clone, Default)]
pub struct CreateSubscriptionPayload {
    pub plan_id: String,
    pub subscriber_email: String,
    pub subscriber_name: String,
    pub custom_id: Option<String>,
    pub return_url: String,
    pub cancel_url: String,
    /// `None` = 1.
    pub quantity: Option<i64>,
    pub start_time: Option<String>,
}

/// Node: paypal.service.ts:~494 `getApprovalUrl` (link `rel=approve`; href kosong -> `None`).
pub fn get_approval_url(response: &Value) -> Option<String> {
    response
        .get("links")?
        .as_array()?
        .iter()
        .find(|l| l.get("rel").and_then(Value::as_str) == Some("approve"))?
        .get("href")?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}
