//! `clickupHttp` / `clickupCronHttp` (`clickup.service.ts:32`): axios dengan timeout
//! 15 dtk; varian cron mengulang HTTP 429 SEKALI setelah menunggu `X-RateLimit-Reset`
//! (maks 60 dtk). Header `Authorization` = token API mentah.
//!
//! Hasil sukses = `res.data` axios: JSON bila body JSON, selain itu string mentah.
//! Galat meniru `error.message` axios (`Request failed with status code N`,
//! `timeout of 15000ms exceeded`) dan `error.response?.status`.

use std::time::Duration;

use mz_core::AppState;
use mz_int_common::api_base;
use reqwest::Method;
use serde_json::Value;

/// `CLICKUP_HTTP_TIMEOUT_MS`.
pub(crate) const TIMEOUT: Duration = Duration::from_millis(15_000);
/// `CLICKUP_RATE_LIMIT_MAX_WAIT_MS`.
const RATE_LIMIT_MAX_WAIT_MS: i64 = 60_000;

/// `AxiosError` yang relevan bagi service.
#[derive(Debug, Clone)]
pub(crate) struct HttpErr {
    /// `error.response?.status`.
    pub status: Option<u16>,
    /// `error.message`.
    pub message: String,
}

/// `rateLimitWaitMs(resetHeader)`: header = detik unix; tak valid → 60 dtk; hasil dijepit
/// ke [1 dtk, 60 dtk].
pub(crate) fn rate_limit_wait_ms(reset: Option<&str>, now_ms: i64) -> i64 {
    // `Number(undefined)` = NaN → batas maksimum.
    let reset_sec = reset.map_or(f64::NAN, crate::util::str_to_number);
    if !reset_sec.is_finite() || reset_sec <= 0.0 {
        return RATE_LIMIT_MAX_WAIT_MS;
    }
    #[allow(clippy::cast_possible_truncation)]
    let wait = (reset_sec * 1000.0) as i64 - now_ms;
    wait.clamp(1_000, RATE_LIMIT_MAX_WAIT_MS)
}

fn axios_message(err: &reqwest::Error) -> String {
    if err.is_timeout() {
        format!("timeout of {}ms exceeded", TIMEOUT.as_millis())
    } else {
        err.to_string()
    }
}

/// `transformResponse` bawaan axios: JSON bila bisa di-parse, selain itu string.
fn parse_body(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(bytes).into_owned()))
}

/// Klien ClickUp untuk satu token.
pub(crate) struct Client<'a> {
    state: &'a AppState,
    base: String,
    token: &'a str,
    retry_429: bool,
}

impl<'a> Client<'a> {
    /// `clickupHttp` (gagal cepat).
    pub(crate) fn new(state: &'a AppState, token: &'a str) -> Self {
        Self {
            state,
            base: api_base::CLICKUP.get(state),
            token,
            retry_429: false,
        }
    }

    /// `clickupCronHttp` (429 diulang sekali).
    pub(crate) fn cron(state: &'a AppState, token: &'a str) -> Self {
        Self {
            retry_429: true,
            ..Self::new(state, token)
        }
    }

    pub(crate) async fn get(&self, path: &str) -> Result<Value, HttpErr> {
        self.send(Method::GET, path, &[], None).await
    }

    pub(crate) async fn get_query(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<Value, HttpErr> {
        self.send(Method::GET, path, query, None).await
    }

    pub(crate) async fn post(&self, path: &str, body: &Value) -> Result<Value, HttpErr> {
        self.send(Method::POST, path, &[], Some(body)).await
    }

    pub(crate) async fn put(&self, path: &str, body: &Value) -> Result<Value, HttpErr> {
        self.send(Method::PUT, path, &[], Some(body)).await
    }

    pub(crate) async fn delete(&self, path: &str) -> Result<Value, HttpErr> {
        self.send(Method::DELETE, path, &[], None).await
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
    ) -> Result<Value, HttpErr> {
        let mut retried = false;
        loop {
            let mut rb = self
                .state
                .http
                .request(method.clone(), format!("{}{path}", self.base))
                .header(reqwest::header::AUTHORIZATION, self.token)
                .timeout(TIMEOUT);
            if !query.is_empty() {
                rb = rb.query(query);
            }
            if let Some(b) = body {
                rb = rb.json(b);
            }
            let res = rb.send().await.map_err(|e| HttpErr {
                status: None,
                message: axios_message(&e),
            })?;
            let status = res.status();
            let reset = res
                .headers()
                .get("x-ratelimit-reset")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            let bytes = res.bytes().await.map_err(|e| HttpErr {
                status: None,
                message: axios_message(&e),
            })?;
            if status.is_success() {
                return Ok(parse_body(&bytes));
            }
            if self.retry_429 && status.as_u16() == 429 && !retried {
                retried = true;
                let now = chrono::Utc::now().timestamp_millis();
                let wait = rate_limit_wait_ms(reset.as_deref(), now);
                tokio::time::sleep(Duration::from_millis(wait.unsigned_abs())).await;
                continue;
            }
            return Err(HttpErr {
                status: Some(status.as_u16()),
                message: format!("Request failed with status code {}", status.as_u16()),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limit_wait() {
        let now = 1_000_000_000_000;
        assert_eq!(rate_limit_wait_ms(None, now), 60_000);
        assert_eq!(rate_limit_wait_ms(Some("abc"), now), 60_000);
        assert_eq!(rate_limit_wait_ms(Some("0"), now), 60_000);
        // reset sudah lewat → minimal 1 dtk.
        assert_eq!(rate_limit_wait_ms(Some("999999999"), now), 1_000);
        assert_eq!(rate_limit_wait_ms(Some("1000000005"), now), 5_000);
        assert_eq!(rate_limit_wait_ms(Some("1000000500"), now), 60_000);
    }

    #[test]
    fn body_parse() {
        assert_eq!(parse_body(br#"{"a":1}"#), serde_json::json!({"a": 1}));
        assert_eq!(parse_body(b"oops"), Value::String("oops".into()));
        assert_eq!(parse_body(b""), Value::String(String::new()));
    }
}
