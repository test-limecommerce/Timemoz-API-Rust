//! Panggilan `axios` ke Google yang dipakai `google-calendar*.service.ts`, lewat
//! `mz_int_common::api_base::{GOOGLE_OAUTH, GOOGLE_APIS}`.
//!
//! Node TIDAK memakai paket `googleapis`/`OAuth2Client` (hanya `axios` + URL REST): token
//! di-refresh manual oleh service (kedaluwarsa ≤ 5 menit lagi → `grant_type=refresh_token`),
//! tidak ada auto-refresh 401 dan tidak ada event `tokens`. Port ini meniru itu persis.
//!
//! Galat meniru bentuk `AxiosError`: status ≠ 2xx → `message = "Request failed with status
//! code N"`, `response.data` = body (JSON bila bisa di-parse, selain itu teks).

use std::time::Duration;

use mz_core::AppState;
use mz_int_common::api_base::{GOOGLE_APIS, GOOGLE_OAUTH};
use reqwest::Method;
use serde_json::Value;

use crate::util::{axios_query, encode_uri_component};

/// `GOOGLE_HTTP_TIMEOUT_MS` (service company). Service user tidak memasang timeout axios.
pub const COMPANY_TIMEOUT: Duration = Duration::from_millis(15_000);

/// Galat bergaya `AxiosError` / galat lain dengan `error.message`.
#[derive(Debug, Clone)]
pub enum Fail {
    Http {
        /// `err.response?.status` (`None` = tanpa respons: timeout/jaringan).
        status: Option<u16>,
        /// `err.response?.data`.
        data: Value,
        message: String,
    },
    /// Galat non-axios (DB, tanggal tidak valid, ...): `error.message`.
    Msg(String),
}

impl Fail {
    pub fn message(&self) -> String {
        match self {
            Fail::Http { message, .. } | Fail::Msg(message) => message.clone(),
        }
    }

    /// `axios.isAxiosError(err) && err.response?.status`.
    pub fn http_status(&self) -> Option<u16> {
        match self {
            Fail::Http { status, .. } => *status,
            Fail::Msg(_) => None,
        }
    }

    /// `err.response?.data?.error`.
    pub fn data_error(&self) -> Option<&str> {
        match self {
            Fail::Http { data, .. } => data.get("error").and_then(Value::as_str),
            Fail::Msg(_) => None,
        }
    }
}

impl From<sqlx::Error> for Fail {
    fn from(e: sqlx::Error) -> Self {
        tracing::error!(error = %e, "google-calendar: galat DB");
        Fail::Msg(e.to_string())
    }
}

pub type GResult<T> = Result<T, Fail>;

/// Satu request bergaya axios. `query` = `params` (nilai `None` dilewati).
async fn call(
    state: &AppState,
    method: Method,
    url: String,
    query: &[(&str, Option<String>)],
    bearer: Option<&str>,
    body: Option<&Value>,
    timeout: Option<Duration>,
) -> GResult<Value> {
    let qs = axios_query(query);
    let full = if qs.is_empty() {
        url
    } else if url.contains('?') {
        format!("{url}&{qs}")
    } else {
        format!("{url}?{qs}")
    };
    let method_has_body = matches!(method, Method::POST | Method::PUT | Method::PATCH);
    let mut req = state.http.request(method, &full);
    if let Some(t) = timeout {
        req = req.timeout(t);
    }
    if let Some(b) = bearer {
        req = req.bearer_auth(b);
    }
    if let Some(b) = body {
        req = req.json(b);
    } else if method_has_body {
        // axios mengirim `Content-Length: 0` untuk `post(url, null)`. reqwest tanpa body tidak
        // menulis header panjang sama sekali → Google menolak `/token` & `/revoke` dengan 411.
        req = req
            .header(reqwest::header::CONTENT_LENGTH, "0")
            .body(Vec::<u8>::new());
    }
    let res = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            let message = if e.is_timeout() {
                // axios: `timeout of ${timeout}ms exceeded`.
                format!(
                    "timeout of {}ms exceeded",
                    timeout.map_or(0, |t| t.as_millis())
                )
            } else {
                "Network Error".to_owned()
            };
            // `without_url`: URL bisa memuat client_secret / token.
            tracing::warn!(error = %e.without_url(), "google-calendar: request gagal");
            return Err(Fail::Http {
                status: None,
                data: Value::Null,
                message,
            });
        }
    };
    let status = res.status();
    let text = res.text().await.unwrap_or_default();
    let data = if text.is_empty() {
        Value::String(String::new())
    } else {
        serde_json::from_str(&text).unwrap_or(Value::String(text))
    };
    if status.is_success() {
        Ok(data)
    } else {
        Err(Fail::Http {
            status: Some(status.as_u16()),
            data,
            message: format!("Request failed with status code {}", status.as_u16()),
        })
    }
}

/// `axios.post(GOOGLE_TOKEN_URL, null, { params })`.
pub async fn token(
    state: &AppState,
    params: &[(&str, Option<String>)],
    timeout: Option<Duration>,
) -> GResult<Value> {
    let url = format!("{}/token", GOOGLE_OAUTH.get(state));
    call(state, Method::POST, url, params, None, None, timeout).await
}

/// `axios.post(`${GOOGLE_REVOKE_URL}?token=${token}`)` (token tidak di-encode, sama dengan Node).
pub async fn revoke(state: &AppState, token: &str, timeout: Option<Duration>) -> GResult<Value> {
    let url = format!("{}/revoke?token={token}", GOOGLE_OAUTH.get(state));
    call(state, Method::POST, url, &[], None, None, timeout).await
}

/// `GET /oauth2/v2/userinfo`.
pub async fn userinfo(state: &AppState, access: &str, timeout: Option<Duration>) -> GResult<Value> {
    let url = format!("{}/oauth2/v2/userinfo", GOOGLE_APIS.get(state));
    call(state, Method::GET, url, &[], Some(access), None, timeout).await
}

fn calendar_api(state: &AppState) -> String {
    format!("{}/calendar/v3", GOOGLE_APIS.get(state))
}

/// `GET /calendars/{encodeURIComponent(calendarId)}/events`.
pub async fn list_events(
    state: &AppState,
    access: &str,
    calendar_id: &str,
    params: &[(&str, Option<String>)],
    timeout: Option<Duration>,
) -> GResult<Value> {
    let url = format!(
        "{}/calendars/{}/events",
        calendar_api(state),
        encode_uri_component(calendar_id)
    );
    call(state, Method::GET, url, params, Some(access), None, timeout).await
}

/// `POST /calendars/primary/events?sendUpdates=all`.
pub async fn insert_event(
    state: &AppState,
    access: &str,
    payload: &Value,
    timeout: Option<Duration>,
) -> GResult<Value> {
    let url = format!(
        "{}/calendars/primary/events?sendUpdates=all",
        calendar_api(state)
    );
    call(
        state,
        Method::POST,
        url,
        &[],
        Some(access),
        Some(payload),
        timeout,
    )
    .await
}

/// `DELETE /calendars/{enc(calendarId)}/events/{enc(googleEventId)}?sendUpdates=all`.
pub async fn delete_event(
    state: &AppState,
    access: &str,
    calendar_id: &str,
    event_id: &str,
    timeout: Option<Duration>,
) -> GResult<Value> {
    let url = format!(
        "{}/calendars/{}/events/{}?sendUpdates=all",
        calendar_api(state),
        encode_uri_component(calendar_id),
        encode_uri_component(event_id)
    );
    call(state, Method::DELETE, url, &[], Some(access), None, timeout).await
}

/// `POST /calendars/primary/events/watch`.
pub async fn watch(state: &AppState, access: &str, body: &Value) -> GResult<Value> {
    let url = format!("{}/calendars/primary/events/watch", calendar_api(state));
    call(
        state,
        Method::POST,
        url,
        &[],
        Some(access),
        Some(body),
        Some(COMPANY_TIMEOUT),
    )
    .await
}

/// `POST /channels/stop`.
pub async fn stop_channel(state: &AppState, access: &str, body: &Value) -> GResult<Value> {
    let url = format!("{}/channels/stop", calendar_api(state));
    call(
        state,
        Method::POST,
        url,
        &[],
        Some(access),
        Some(body),
        Some(COMPANY_TIMEOUT),
    )
    .await
}

/// `GET /users/me/calendarList?maxResults=100`.
pub async fn calendar_list(state: &AppState, access: &str) -> GResult<Value> {
    let url = format!("{}/users/me/calendarList", calendar_api(state));
    call(
        state,
        Method::GET,
        url,
        &[("maxResults", Some("100".into()))],
        Some(access),
        None,
        Some(COMPANY_TIMEOUT),
    )
    .await
}
