use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use mz_core::AppState;
use reqwest::Method;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::error::PaypalError;
use crate::payloads::{CreatePlanPayload, CreateSubscriptionPayload};

/// Path OAuth (paypal.service.ts:63).
pub const TOKEN_PATH: &str = "/v1/oauth2/token";
const LIVE_BASE: &str = "https://api-m.paypal.com";
const SANDBOX_BASE: &str = "https://api-m.sandbox.paypal.com";
/// `timeout: 15_000` axios.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);

/// Konfigurasi klien (env Node: `PAYPAL_MODE`, `PAYPAL_CLIENT_ID`, `PAYPAL_CLIENT_SECRET`,
/// `PAYPAL_BRAND_NAME`).
#[derive(Debug, Clone)]
pub struct PaypalConfig {
    /// Tanpa `/` di akhir.
    pub base_url: String,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    /// Sudah menerapkan `|| 'Timemoz'`.
    pub brand_name: String,
    pub timeout: Duration,
}

impl PaypalConfig {
    /// Dari fungsi pembaca env (kosong/blank = `None`).
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        let base_url = match get("MZ_PAYPAL_API_BASE") {
            Some(v) => v.trim_end_matches('/').to_owned(),
            None if get("PAYPAL_MODE").as_deref() == Some("live") => LIVE_BASE.to_owned(),
            None => SANDBOX_BASE.to_owned(),
        };
        Self {
            base_url,
            client_id: get("PAYPAL_CLIENT_ID"),
            client_secret: get("PAYPAL_CLIENT_SECRET"),
            brand_name: get("PAYPAL_BRAND_NAME").unwrap_or_else(|| "Timemoz".to_owned()),
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// Dari `state.config.var(..)` (menghormati override uji `mz-testkit`).
    pub fn from_state(state: &AppState) -> Self {
        Self::from_lookup(|k| state.config.var(k))
    }
}

type TokenKey = (String, String, String);
type TokenCache = Mutex<HashMap<TokenKey, (String, Instant)>>;

/// Cache token per proses (`this.accessToken`/`tokenExpiry`). Statik supaya klien yang dibuat
/// per-request tetap berbagi token seperti service singleton Node.
fn token_cache() -> &'static TokenCache {
    static CACHE: OnceLock<TokenCache> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Klien PayPal; murah di-`clone`/dibuat ulang.
#[derive(Clone)]
pub struct PaypalClient {
    cfg: PaypalConfig,
    http: reqwest::Client,
}

impl PaypalClient {
    pub fn new(cfg: PaypalConfig, http: reqwest::Client) -> Self {
        Self { cfg, http }
    }

    /// Konfigurasi dari env + pakai `state.http`.
    pub fn from_state(state: &AppState) -> Self {
        Self::new(PaypalConfig::from_state(state), state.http.clone())
    }

    pub fn config(&self) -> &PaypalConfig {
        &self.cfg
    }

    // ---------------------------------------------------------------- transport

    fn map_send_err(&self, e: reqwest::Error) -> PaypalError {
        if e.is_timeout() {
            return PaypalError::transport(format!(
                "timeout of {}ms exceeded",
                self.cfg.timeout.as_millis()
            ));
        }
        if e.is_connect() {
            let addr = e
                .url()
                .map(|u| {
                    format!(
                        "{}:{}",
                        u.host_str().unwrap_or_default(),
                        u.port_or_known_default().unwrap_or(0)
                    )
                })
                .unwrap_or_default();
            return PaypalError::transport(format!("connect ECONNREFUSED {addr}"));
        }
        PaypalError::transport(e.without_url().to_string())
    }

    /// axios: respons non-2xx dilempar dengan `response.data`; body bukan JSON jadi string.
    async fn finish(&self, res: reqwest::Response) -> Result<Value, PaypalError> {
        let status = res.status();
        let text = res.text().await.map_err(|e| self.map_send_err(e))?;
        let data = serde_json::from_str::<Value>(&text).unwrap_or(Value::String(text));
        if status.is_success() {
            Ok(data)
        } else {
            Err(PaypalError {
                message: format!("Request failed with status code {}", status.as_u16()),
                status: Some(status.as_u16()),
                data: Some(data).filter(|d| d.as_str() != Some("")),
            })
        }
    }

    /// Node: paypal.service.ts:58 `getAccessToken`.
    pub async fn get_access_token(&self) -> Result<String, PaypalError> {
        let key = (
            self.cfg.base_url.clone(),
            self.cfg.client_id.clone().unwrap_or_default(),
            self.cfg.client_secret.clone().unwrap_or_default(),
        );
        let mut cache = token_cache().lock().await;
        if let Some((token, expiry)) = cache.get(&key)
            && Instant::now() < *expiry
        {
            return Ok(token.clone());
        }
        // Node: `${clientId}:${clientSecret}` -> "undefined" bila env kosong.
        let id = self.cfg.client_id.as_deref().unwrap_or("undefined");
        let secret = self.cfg.client_secret.as_deref().unwrap_or("undefined");
        let res = self
            .http
            .post(format!("{}{TOKEN_PATH}", self.cfg.base_url))
            .timeout(self.cfg.timeout)
            .basic_auth(id, Some(secret))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body("grant_type=client_credentials")
            .send()
            .await;
        let result = match res {
            Ok(r) => self.finish(r).await,
            Err(e) => Err(self.map_send_err(e)),
        };
        let body = result.inspect_err(|e| {
            tracing::error!(
                data = ?e.data,
                error = %e,
                "Failed to get PayPal access token"
            );
        })?;
        let token = body
            .get("access_token")
            .and_then(Value::as_str)
            .unwrap_or("undefined")
            .to_owned();
        let expires = body.get("expires_in").and_then(Value::as_u64).unwrap_or(0);
        let ttl = Duration::from_secs(expires).saturating_sub(Duration::from_secs(60));
        cache.insert(key, (token.clone(), Instant::now() + ttl));
        Ok(token)
    }

    /// Satu pemanggilan HTTP dengan token yang sudah ada. `data = None` -> tanpa body.
    async fn send(
        &self,
        token: &str,
        method: &Method,
        path: &str,
        data: Option<&Value>,
    ) -> Result<Value, PaypalError> {
        let mut req = self
            .http
            .request(method.clone(), format!("{}{path}", self.cfg.base_url))
            .timeout(self.cfg.timeout)
            .bearer_auth(token);
        if let Some(d) = data {
            req = req.json(d);
        }
        match req.send().await {
            Ok(r) => self.finish(r).await,
            Err(e) => Err(self.map_send_err(e)),
        }
    }

    /// Node: paypal.service.ts:93 `request` (log `PayPal API Error: METHOD url`).
    async fn request(
        &self,
        method: Method,
        path: &str,
        data: Option<Value>,
    ) -> Result<Value, PaypalError> {
        // Galat token sudah dilog di get_access_token (tanpa log kedua, seperti Node).
        let token = self.get_access_token().await?;
        self.send(&token, &method, path, data.as_ref())
            .await
            .inspect_err(|e| {
                tracing::error!(
                    data = ?e.data,
                    error = %e,
                    "PayPal API Error: {method} {path}"
                );
            })
    }

    fn app_context(&self, user_action: &str, return_url: &str, cancel_url: &str) -> Value {
        json!({
            "brand_name": self.cfg.brand_name,
            "locale": "en-US",
            "shipping_preference": "NO_SHIPPING",
            "user_action": user_action,
            "return_url": return_url,
            "cancel_url": cancel_url,
        })
    }

    // ---------------------------------------------------------------- produk

    /// Node: paypal.service.ts:117 `createProduct`. `type` bawaan `SERVICE`.
    pub async fn create_product(
        &self,
        name: &str,
        description: &str,
        r#type: Option<&str>,
    ) -> Result<Value, PaypalError> {
        self.request(
            Method::POST,
            "/v1/catalogs/products",
            Some(json!({
                "name": name,
                "description": description,
                "type": r#type.unwrap_or("SERVICE"),
                "category": "SOFTWARE",
            })),
        )
        .await
    }

    /// Node: paypal.service.ts:133 `getProduct`.
    pub async fn get_product(&self, product_id: &str) -> Result<Value, PaypalError> {
        self.request(
            Method::GET,
            &format!("/v1/catalogs/products/{product_id}"),
            None,
        )
        .await
    }

    // ---------------------------------------------------------------- plan

    fn plan_body(p: &CreatePlanPayload, quantity_based: bool) -> Value {
        let mut body = json!({
            "product_id": p.product_id,
            "name": p.name,
            "description": p.description,
            "status": "ACTIVE",
        });
        if quantity_based && let Some(o) = body.as_object_mut() {
            o.insert("quantity_supported".into(), Value::Bool(true));
        }
        if let Some(o) = body.as_object_mut() {
            o.insert(
                "billing_cycles".into(),
                json!([{
                    "frequency": {
                        "interval_unit": p.interval_unit.as_deref().unwrap_or("MONTH"),
                        "interval_count": p.interval_count.unwrap_or(1),
                    },
                    "tenure_type": "REGULAR",
                    "sequence": 1,
                    "total_cycles": 0,
                    "pricing_scheme": {
                        "fixed_price": {
                            "value": to_fixed2(p.price_usd),
                            "currency_code": "USD",
                        },
                    },
                }]),
            );
            o.insert(
                "payment_preferences".into(),
                json!({
                    "auto_bill_outstanding": true,
                    "setup_fee_failure_action": "CONTINUE",
                    "payment_failure_threshold": 3,
                }),
            );
        }
        body
    }

    /// Node: paypal.service.ts:142 `createPlan` (harga tetap).
    pub async fn create_plan(&self, p: &CreatePlanPayload) -> Result<Value, PaypalError> {
        self.request(
            Method::POST,
            "/v1/billing/plans",
            Some(Self::plan_body(p, false)),
        )
        .await
    }

    /// Node: paypal.service.ts:186 `createQuantityBasedPlan` (`quantity_supported: true`).
    pub async fn create_quantity_based_plan(
        &self,
        p: &CreatePlanPayload,
    ) -> Result<Value, PaypalError> {
        self.request(
            Method::POST,
            "/v1/billing/plans",
            Some(Self::plan_body(p, true)),
        )
        .await
    }

    /// Node: paypal.service.ts:232 `getPlan`.
    pub async fn get_plan(&self, plan_id: &str) -> Result<Value, PaypalError> {
        self.request(Method::GET, &format!("/v1/billing/plans/{plan_id}"), None)
            .await
    }

    /// Node: paypal.service.ts:239 `listPlans` (query tidak di-encode, seperti Node).
    pub async fn list_plans(&self, product_id: Option<&str>) -> Result<Value, PaypalError> {
        let url = match product_id.filter(|s| !s.is_empty()) {
            Some(p) => {
                format!("/v1/billing/plans?product_id={p}&page_size=20&page=1&total_required=true")
            }
            None => "/v1/billing/plans?page_size=20&page=1&total_required=true".to_owned(),
        };
        self.request(Method::GET, &url, None).await
    }

    /// Node: paypal.service.ts:250 `deactivatePlan`.
    pub async fn deactivate_plan(&self, plan_id: &str) -> Result<(), PaypalError> {
        self.request(
            Method::POST,
            &format!("/v1/billing/plans/{plan_id}/deactivate"),
            Some(json!({})),
        )
        .await
        .map(drop)
    }

    /// Node: paypal.service.ts:260 `updatePlanName` (PATCH `/name`, `/description` bila tak kosong).
    pub async fn update_plan_name(
        &self,
        plan_id: &str,
        name: &str,
        description: Option<&str>,
    ) -> Result<(), PaypalError> {
        let mut ops = vec![json!({ "op": "replace", "path": "/name", "value": name })];
        if let Some(d) = description.filter(|d| !d.is_empty()) {
            ops.push(json!({ "op": "replace", "path": "/description", "value": d }));
        }
        self.request(
            Method::PATCH,
            &format!("/v1/billing/plans/{plan_id}"),
            Some(Value::Array(ops)),
        )
        .await
        .map(drop)
    }

    /// Node: paypal.service.ts:272 `updatePlanPrice` (`update-pricing-schemes`, sequence 1).
    pub async fn update_plan_price(
        &self,
        plan_id: &str,
        price_per_user_usd: f64,
    ) -> Result<(), PaypalError> {
        self.request(
            Method::POST,
            &format!("/v1/billing/plans/{plan_id}/update-pricing-schemes"),
            Some(json!({
                "pricing_schemes": [{
                    "billing_cycle_sequence": 1,
                    "pricing_scheme": {
                        "fixed_price": {
                            "value": to_fixed2(price_per_user_usd),
                            "currency_code": "USD",
                        },
                    },
                }],
            })),
        )
        .await
        .map(drop)
    }

    // ---------------------------------------------------------------- subscription

    /// Node: paypal.service.ts:303 `createSubscription`.
    pub async fn create_subscription(
        &self,
        p: &CreateSubscriptionPayload,
    ) -> Result<Value, PaypalError> {
        let mut parts = p.subscriber_name.split(' ');
        let first = parts.next().unwrap_or_default();
        let given = if first.is_empty() {
            p.subscriber_name.as_str()
        } else {
            first
        };
        let rest = parts.collect::<Vec<_>>().join(" ");
        let surname = if rest.is_empty() {
            given
        } else {
            rest.as_str()
        };
        let mut body = json!({
            "plan_id": p.plan_id,
            "quantity": p.quantity.unwrap_or(1).to_string(),
            "subscriber": {
                "name": { "given_name": given, "surname": surname },
                "email_address": p.subscriber_email,
            },
            "application_context": self.app_context("SUBSCRIBE_NOW", &p.return_url, &p.cancel_url),
        });
        if let Some(o) = body.as_object_mut() {
            if let Some(t) = p.start_time.as_deref().filter(|s| !s.is_empty()) {
                o.insert("start_time".into(), json!(t));
            }
            if let Some(c) = p.custom_id.as_deref().filter(|s| !s.is_empty()) {
                o.insert("custom_id".into(), json!(c));
            }
        }
        self.request(Method::POST, "/v1/billing/subscriptions", Some(body))
            .await
    }

    /// Node: paypal.service.ts:354 `getSubscription`.
    pub async fn get_subscription(&self, subscription_id: &str) -> Result<Value, PaypalError> {
        self.request(
            Method::GET,
            &format!("/v1/billing/subscriptions/{subscription_id}"),
            None,
        )
        .await
    }

    /// Node: paypal.service.ts:366 `updateSubscriptionQuantity` (PATCH `/quantity`).
    pub async fn update_subscription_quantity(
        &self,
        subscription_id: &str,
        new_quantity: i64,
    ) -> Result<Value, PaypalError> {
        self.request(
            Method::PATCH,
            &format!("/v1/billing/subscriptions/{subscription_id}"),
            Some(json!([{
                "op": "replace",
                "path": "/quantity",
                "value": new_quantity.to_string(),
            }])),
        )
        .await
    }

    /// Node: paypal.service.ts:390 `reviseSubscriptionQuantity`.
    pub async fn revise_subscription_quantity(
        &self,
        subscription_id: &str,
        new_quantity: i64,
        return_url: &str,
        cancel_url: &str,
    ) -> Result<Value, PaypalError> {
        self.request(
            Method::POST,
            &format!("/v1/billing/subscriptions/{subscription_id}/revise"),
            Some(json!({
                "quantity": new_quantity.to_string(),
                "application_context": self.app_context("SUBSCRIBE_NOW", return_url, cancel_url),
            })),
        )
        .await
    }

    /// Node: paypal.service.ts:416 `activateSubscription`. `reason` bawaan `Reactivating subscription`.
    pub async fn activate_subscription(
        &self,
        subscription_id: &str,
        reason: Option<&str>,
    ) -> Result<(), PaypalError> {
        self.request(
            Method::POST,
            &format!("/v1/billing/subscriptions/{subscription_id}/activate"),
            Some(json!({ "reason": reason.unwrap_or("Reactivating subscription") })),
        )
        .await
        .map(drop)
    }

    /// Node: paypal.service.ts:432 `suspendSubscription`. `reason` bawaan `Subscription suspended`.
    pub async fn suspend_subscription(
        &self,
        subscription_id: &str,
        reason: Option<&str>,
    ) -> Result<(), PaypalError> {
        self.request(
            Method::POST,
            &format!("/v1/billing/subscriptions/{subscription_id}/suspend"),
            Some(json!({ "reason": reason.unwrap_or("Subscription suspended") })),
        )
        .await
        .map(drop)
    }

    /// Node: paypal.service.ts:452 `cancelSubscription`. `SUBSCRIPTION_STATUS_INVALID` = no-op
    /// (`Ok`). `reason` bawaan `Subscription cancelled by user`. Galat token ikut ditangkap
    /// blok try yang sama seperti di Node (dilog sebagai galat API cancel).
    pub async fn cancel_subscription(
        &self,
        subscription_id: &str,
        reason: Option<&str>,
    ) -> Result<(), PaypalError> {
        let path = format!("/v1/billing/subscriptions/{subscription_id}/cancel");
        let body = json!({ "reason": reason.unwrap_or("Subscription cancelled by user") });
        let result = match self.get_access_token().await {
            Ok(token) => self.send(&token, &Method::POST, &path, Some(&body)).await,
            Err(e) => Err(e),
        };
        match result {
            Ok(_) => Ok(()),
            Err(e) => {
                if e.first_issue() == Some("SUBSCRIPTION_STATUS_INVALID") {
                    tracing::warn!(
                        "Subscription {subscription_id} not in cancellable state — treating as already cancelled"
                    );
                    return Ok(());
                }
                tracing::error!(
                    data = ?e.data,
                    error = %e,
                    "PayPal API Error: POST {path}"
                );
                Err(e)
            }
        }
    }

    /// Node: paypal.service.ts:489 `reviseSubscription` (ganti plan).
    pub async fn revise_subscription(
        &self,
        subscription_id: &str,
        new_plan_id: &str,
        return_url: &str,
        cancel_url: &str,
    ) -> Result<Value, PaypalError> {
        self.request(
            Method::POST,
            &format!("/v1/billing/subscriptions/{subscription_id}/revise"),
            Some(json!({
                "plan_id": new_plan_id,
                "application_context": self.app_context("SUBSCRIBE_NOW", return_url, cancel_url),
            })),
        )
        .await
    }

    /// Node: paypal.service.ts:512 `listSubscriptionTransactions` (waktu tidak di-encode).
    pub async fn list_subscription_transactions(
        &self,
        subscription_id: &str,
        start_time: &str,
        end_time: &str,
    ) -> Result<Value, PaypalError> {
        self.request(
            Method::GET,
            &format!(
                "/v1/billing/subscriptions/{subscription_id}/transactions?start_time={start_time}&end_time={end_time}"
            ),
            None,
        )
        .await
    }

    /// Node: paypal.service.ts:525 `verifyWebhookSignature`. `Ok(false)` = tanda tangan palsu;
    /// `Err` = PayPal tak terjangkau/galat API (pemanggil harus membalas 5xx).
    #[allow(clippy::too_many_arguments)]
    pub async fn verify_webhook_signature(
        &self,
        webhook_id: &str,
        transmission_id: &str,
        transmission_time: &str,
        cert_url: &str,
        auth_algo: &str,
        transmission_sig: &str,
        webhook_event: &Value,
    ) -> Result<bool, PaypalError> {
        let res = self
            .request(
                Method::POST,
                "/v1/notifications/verify-webhook-signature",
                Some(json!({
                    "auth_algo": auth_algo,
                    "cert_url": cert_url,
                    "transmission_id": transmission_id,
                    "transmission_sig": transmission_sig,
                    "transmission_time": transmission_time,
                    "webhook_id": webhook_id,
                    "webhook_event": webhook_event,
                })),
            )
            .await?;
        Ok(res.get("verification_status").and_then(Value::as_str) == Some("SUCCESS"))
    }

    /// Node: paypal.service.ts:~565 `captureSubscriptionPayment` (`OUTSTANDING_BALANCE`).
    pub async fn capture_subscription_payment(
        &self,
        subscription_id: &str,
        amount: f64,
        note: &str,
    ) -> Result<Value, PaypalError> {
        self.request(
            Method::POST,
            &format!("/v1/billing/subscriptions/{subscription_id}/capture"),
            Some(json!({
                "note": note,
                "capture_type": "OUTSTANDING_BALANCE",
                "amount": { "currency_code": "USD", "value": to_fixed2(amount) },
            })),
        )
        .await
    }

    // ---------------------------------------------------------------- order

    /// Node: paypal.service.ts:~585 `createOrder` (intent CAPTURE, `user_action: PAY_NOW`).
    pub async fn create_order(
        &self,
        amount: f64,
        description: &str,
        return_url: &str,
        cancel_url: &str,
    ) -> Result<Value, PaypalError> {
        self.request(
            Method::POST,
            "/v2/checkout/orders",
            Some(json!({
                "intent": "CAPTURE",
                "purchase_units": [{
                    "amount": { "currency_code": "USD", "value": to_fixed2(amount) },
                    "description": description,
                }],
                "application_context": self.app_context("PAY_NOW", return_url, cancel_url),
            })),
        )
        .await
    }

    /// Node: paypal.service.ts:~615 `captureOrder`.
    pub async fn capture_order(&self, order_id: &str) -> Result<Value, PaypalError> {
        self.request(
            Method::POST,
            &format!("/v2/checkout/orders/{order_id}/capture"),
            Some(json!({})),
        )
        .await
    }

    /// Node: paypal.service.ts:~622 `getOrder`.
    pub async fn get_order(&self, order_id: &str) -> Result<Value, PaypalError> {
        self.request(
            Method::GET,
            &format!("/v2/checkout/orders/{order_id}"),
            None,
        )
        .await
    }

    /// Node: paypal.service.ts:~628 `calculateTotalAmount`.
    pub fn calculate_total_amount(quantity: f64, price_per_user: f64) -> f64 {
        quantity * price_per_user
    }
}

/// JS `Number.prototype.toFixed(2)`: nilai persis, imbang dibulatkan ke atas (menjauhi nol
/// pada magnitudo, seperti spesifikasi). `NaN`/`Infinity` mengikuti JS.
pub fn to_fixed2(x: f64) -> String {
    if x.is_nan() {
        return "NaN".into();
    }
    if x.is_infinite() {
        return if x < 0.0 { "-Infinity" } else { "Infinity" }.into();
    }
    if x.abs() >= 1e21 {
        return format!("{x}");
    }
    let neg = x < 0.0;
    // Ekspansi desimal f64 bersifat terbatas; 60 digit cukup untuk semua nilai wajar.
    let full = format!("{:.60}", x.abs());
    let (int, frac) = full.split_once('.').unwrap_or((full.as_str(), ""));
    let mut digits: Vec<u8> = int.bytes().chain(frac.bytes().take(2)).collect();
    let round_up = frac.as_bytes().get(2).is_some_and(|d| *d >= b'5');
    if round_up {
        let mut i = digits.len();
        loop {
            if i == 0 {
                digits.insert(0, b'1');
                break;
            }
            i -= 1;
            match digits.get_mut(i) {
                Some(d) if *d == b'9' => *d = b'0',
                Some(d) => {
                    *d += 1;
                    break;
                }
                None => break,
            }
        }
    }
    let split = digits.len().saturating_sub(2);
    let (a, b) = digits.split_at(split);
    let s = format!(
        "{}.{}",
        String::from_utf8_lossy(a),
        String::from_utf8_lossy(b)
    );
    if neg { format!("-{s}") } else { s }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_fixed2_matches_js() {
        assert_eq!(to_fixed2(10.0), "10.00");
        assert_eq!(to_fixed2(0.125), "0.13"); // JS: (0.125).toFixed(2) === "0.13"
        assert_eq!(to_fixed2(1.005), "1.00"); // 1.005 sebenarnya 1.00499999...
        assert_eq!(to_fixed2(2.675), "2.67");
        assert_eq!(to_fixed2(9.995), "9.99");
        assert_eq!(to_fixed2(99.999), "100.00");
        assert_eq!(to_fixed2(0.0), "0.00");
        assert_eq!(to_fixed2(-1.5), "-1.50");
        assert_eq!(to_fixed2(-0.001), "-0.00"); // JS juga "-0.00"
    }

    #[test]
    fn config_base_url() {
        let cfg = |pairs: &'static [(&str, &str)]| {
            PaypalConfig::from_lookup(|k| {
                pairs
                    .iter()
                    .find(|(a, _)| *a == k)
                    .map(|(_, v)| (*v).to_owned())
            })
        };
        assert_eq!(cfg(&[]).base_url, SANDBOX_BASE);
        assert_eq!(cfg(&[("PAYPAL_MODE", "sandbox")]).base_url, SANDBOX_BASE);
        assert_eq!(cfg(&[("PAYPAL_MODE", "live")]).base_url, LIVE_BASE);
        assert_eq!(
            cfg(&[("MZ_PAYPAL_API_BASE", "http://x:1/")]).base_url,
            "http://x:1"
        );
        assert_eq!(cfg(&[]).brand_name, "Timemoz");
        assert_eq!(cfg(&[("PAYPAL_BRAND_NAME", "Acme")]).brand_name, "Acme");
    }
}
