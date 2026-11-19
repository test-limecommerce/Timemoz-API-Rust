//! `jiraHttp = axios.create({ timeout: 15_000 })` + bentuk galat yang diteruskan Node
//! (`error.message`).

use std::time::Duration;

use mz_core::AppState;
use mz_int_common::api_base::{ATLASSIAN_API, ATLASSIAN_AUTH};
use reqwest::{Method, RequestBuilder};
use serde_json::Value;

/// `timeout: 15_000`.
pub(crate) const HTTP_TIMEOUT: Duration = Duration::from_secs(15);

/// Galat satu panggilan HTTP (axios).
#[derive(Debug)]
pub(crate) enum HttpErr {
    /// Status di luar 2xx: `error.response.{status,data}`.
    Status { status: u16, data: Value },
    /// `ECONNABORTED` — `timeout of 15000ms exceeded`.
    Timeout,
    /// Galat jaringan lain.
    Network(String),
}

impl HttpErr {
    pub(crate) fn message(&self) -> String {
        match self {
            Self::Status { status, .. } => format!("Request failed with status code {status}"),
            Self::Timeout => "timeout of 15000ms exceeded".to_owned(),
            Self::Network(m) => m.clone(),
        }
    }

    pub(crate) fn status(&self) -> Option<u16> {
        match self {
            Self::Status { status, .. } => Some(*status),
            _ => None,
        }
    }
}

/// Galat method service. Node menangkap semuanya lalu membalas
/// `{ success: false, message: error.message, data: null }`.
#[derive(Debug)]
pub(crate) enum Error {
    Http(HttpErr),
    Db(sqlx::Error),
    Msg(String),
}

impl Error {
    pub(crate) fn message(&self) -> String {
        match self {
            Self::Http(e) => e.message(),
            Self::Db(e) => e.to_string(),
            Self::Msg(m) => m.clone(),
        }
    }
}

impl From<HttpErr> for Error {
    fn from(e: HttpErr) -> Self {
        Self::Http(e)
    }
}

impl From<sqlx::Error> for Error {
    fn from(e: sqlx::Error) -> Self {
        Self::Db(e)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

pub(crate) type Res<T> = Result<T, Error>;

fn classify(err: &reqwest::Error) -> HttpErr {
    if err.is_timeout() {
        return HttpErr::Timeout;
    }
    // Tanpa URL (bisa memuat cloudId); token tidak pernah ada di URL.
    let mut msg = String::new();
    let mut cur: Option<&dyn std::error::Error> = Some(err);
    while let Some(e) = cur {
        if !msg.is_empty() {
            msg.push_str(": ");
        }
        msg.push_str(&e.to_string());
        cur = e.source();
    }
    HttpErr::Network(msg)
}

/// Kirim, lalu `res.data` (JSON; teks bukan JSON → string, seperti axios).
pub(crate) async fn send(rb: RequestBuilder) -> Result<Value, HttpErr> {
    let res = rb.timeout(HTTP_TIMEOUT).send().await.map_err(|e| {
        let e = e.without_url();
        classify(&e)
    })?;
    let status = res.status().as_u16();
    let text = res.text().await.map_err(|e| classify(&e.without_url()))?;
    let data = if text.is_empty() {
        Value::String(String::new())
    } else {
        serde_json::from_str(&text).unwrap_or(Value::String(text))
    };
    if !(200..300).contains(&status) {
        return Err(HttpErr::Status { status, data });
    }
    Ok(data)
}

/// `https://api.atlassian.com/ex/jira/{cloudId}/rest/api/3{path}`.
pub(crate) fn jira_url(state: &AppState, cloud_id: &str, path: &str) -> String {
    format!(
        "{}/ex/jira/{cloud_id}/rest/api/3{path}",
        ATLASSIAN_API.get(state)
    )
}

/// `https://auth.atlassian.com/oauth/token`.
pub(crate) fn token_url(state: &AppState) -> String {
    format!("{}/oauth/token", ATLASSIAN_AUTH.get(state))
}

/// `https://api.atlassian.com/oauth/token/accessible-resources`.
pub(crate) fn resources_url(state: &AppState) -> String {
    format!(
        "{}/oauth/token/accessible-resources",
        ATLASSIAN_API.get(state)
    )
}

/// Request ber-`Authorization: Bearer`.
pub(crate) fn bearer(state: &AppState, method: Method, url: &str, token: &str) -> RequestBuilder {
    state
        .http
        .request(method, url)
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn axios_messages() {
        let e = HttpErr::Status {
            status: 404,
            data: Value::Null,
        };
        assert_eq!(e.message(), "Request failed with status code 404");
        assert_eq!(e.status(), Some(404));
        assert_eq!(HttpErr::Timeout.message(), "timeout of 15000ms exceeded");
        assert_eq!(HttpErr::Timeout.status(), None);
        assert_eq!(
            Error::Msg("x".into()).message(),
            Error::from(HttpErr::Network("x".into())).message()
        );
    }
}
