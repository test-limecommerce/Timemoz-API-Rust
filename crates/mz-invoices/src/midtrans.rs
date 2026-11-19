//! Port `src/midtrans/midtrans.service.ts` (payment link; hanya dipakai `renewPayment` untuk
//! invoice lama non-PayPal). Base URL = `MIDTRANS_HOST` (sama dengan Node; uji mengarahkannya ke
//! server tiruan), header `Authorization: Basic ${MIDTRANS_KEY}`.
//!
//! Galat dibentuk seperti `error.message` yang sampai ke `catch` `renewPayment`:
//! - status non-2xx → `Request failed with status code N` (axios);
//! - tanpa respons (jaringan/URL rusak) → `TypeError` di blok `catch` service Midtrans
//!   (`error.response.data` / `error.response.status` pada `undefined`);
//! - 2xx selain 200 → `throw new Error(resp.data)` lalu `TypeError` yang sama di `catch`.

use mz_core::AppState;
use reqwest::Method;
use serde_json::Value;

/// `error.message` yang dilempar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MidtransError(pub String);

impl std::fmt::Display for MidtransError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

enum Outcome {
    /// Respons HTTP (status, body JSON / string).
    Response(u16, Value),
    /// Tidak ada `error.response`.
    NoResponse(String),
}

pub struct Midtrans<'a> {
    state: &'a AppState,
}

impl<'a> Midtrans<'a> {
    pub fn new(state: &'a AppState) -> Self {
        Self { state }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", crate::util::env_js(self.state, "MIDTRANS_HOST"))
    }

    async fn send(&self, method: Method, url: &str, body: Option<&Value>) -> Outcome {
        let auth = format!("Basic {}", crate::util::env_js(self.state, "MIDTRANS_KEY"));
        let mut req = self
            .state
            .http
            .request(method, url)
            .header("Authorization", auth);
        if let Some(b) = body {
            req = req.json(b);
        }
        let res = match req.send().await {
            Ok(r) => r,
            Err(e) => return Outcome::NoResponse(e.without_url().to_string()),
        };
        let status = res.status().as_u16();
        let text = res.text().await.unwrap_or_default();
        let data = serde_json::from_str(&text).unwrap_or(Value::String(text));
        Outcome::Response(status, data)
    }

    /// Node: midtrans.service.ts:8 (`createPaymentLink`).
    pub async fn create_payment_link(&self, payload: &Value) -> Result<Value, MidtransError> {
        let url = self.url("/v1/payment-links");
        match self.send(Method::POST, &url, Some(payload)).await {
            Outcome::Response(200, data) => Ok(data),
            Outcome::Response(status, data) if !(200..300).contains(&status) => {
                tracing::error!(status, ?data, "Midtrans createPaymentLink gagal");
                Err(MidtransError(format!(
                    "Request failed with status code {status}"
                )))
            }
            // 2xx lain: `new Error(resp.data)` tanpa `response` → `error.response.data` TypeError.
            Outcome::Response(..) => Err(MidtransError(
                "Cannot read properties of undefined (reading 'data')".into(),
            )),
            Outcome::NoResponse(e) => {
                tracing::error!(error = %e, "Midtrans createPaymentLink: tanpa respons");
                Err(MidtransError(
                    "Cannot read properties of undefined (reading 'data')".into(),
                ))
            }
        }
    }

    /// Node: midtrans.service.ts:29 (`deletePaymentLink`): GET lalu DELETE; 404 di salah satunya =
    /// `null` (tidak ada link).
    pub async fn delete_payment_link(
        &self,
        order_id: &str,
    ) -> Result<Option<Value>, MidtransError> {
        let url = self.url(&format!("/v1/payment-links/{order_id}"));
        let check = |o: Outcome| -> Result<Option<Value>, MidtransError> {
            match o {
                Outcome::Response(s, data) if (200..300).contains(&s) => Ok(Some(data)),
                Outcome::Response(404, _) => Ok(None),
                Outcome::Response(s, _) => Err(MidtransError(format!(
                    "Request failed with status code {s}"
                ))),
                Outcome::NoResponse(e) => {
                    tracing::error!(error = %e, "Midtrans deletePaymentLink: tanpa respons");
                    Err(MidtransError(
                        "Cannot read properties of undefined (reading 'status')".into(),
                    ))
                }
            }
        };
        if check(self.send(Method::GET, &url, None).await)?.is_none() {
            return Ok(None);
        }
        match self.send(Method::DELETE, &url, None).await {
            Outcome::Response(200, data) => Ok(Some(data)),
            Outcome::Response(s, _) if (200..300).contains(&s) => Err(MidtransError(
                "Cannot read properties of undefined (reading 'status')".into(),
            )),
            other => check(other),
        }
    }
}
