//! Server HTTP tiruan untuk uji integrasi API eksternal (ClickUp, Jira, Discord, Google).
//!
//! ```ignore
//! let mock = MockServer::start().await;
//! mock.on("GET", "/team", 200, json!({ "teams": [] }));
//! let base = mock.url();                       // http://127.0.0.1:<port>
//! let app = TestApp::with_vars(build, &[("MZ_CLICKUP_API_BASE", &base)]).await;
//! // ... panggil route ...
//! let reqs = mock.requests();                  // urutan request yang diterima
//! ```
//!
//! Aturan dicocokkan dari yang TERAKHIR ditambahkan (aturan baru menimpa yang lama).
//! `path` dicocokkan dengan path TANPA query; `*` di akhir pola = awalan. Tanpa aturan
//! yang cocok → 404 `{"err":"mock: no rule"}`.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use serde_json::Value;

/// Request yang diterima server tiruan.
#[derive(Clone, Debug)]
pub struct Recorded {
    pub method: String,
    /// Path tanpa query.
    pub path: String,
    /// Query mentah (tanpa `?`), kosong bila tidak ada.
    pub query: String,
    pub headers: HeaderMap,
    pub body: String,
}

impl Recorded {
    /// Body sebagai JSON (`Value::Null` bila bukan JSON).
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }

    /// Body `application/x-www-form-urlencoded` sebagai pasangan kunci-nilai.
    pub fn form(&self) -> Vec<(String, String)> {
        self.body
            .split('&')
            .filter(|p| !p.is_empty())
            .map(|p| {
                let (k, v) = p.split_once('=').unwrap_or((p, ""));
                (decode(k), decode(v))
            })
            .collect()
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
}

fn decode(s: &str) -> String {
    let s = s.replace('+', " ");
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        if b == b'%' {
            let hex = bytes
                .get(i + 1..i + 3)
                .and_then(|h| std::str::from_utf8(h).ok());
            if let Some(n) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(n);
                i += 3;
                continue;
            }
        }
        out.push(b);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[derive(Clone)]
struct Rule {
    method: String,
    pattern: String,
    status: u16,
    body: Value,
}

impl Rule {
    fn matches(&self, method: &str, path: &str) -> bool {
        if !self.method.eq_ignore_ascii_case(method) {
            return false;
        }
        match self.pattern.strip_suffix('*') {
            Some(prefix) => path.starts_with(prefix),
            None => path == self.pattern,
        }
    }
}

#[derive(Default)]
struct Inner {
    rules: Vec<Rule>,
    log: Vec<Recorded>,
}

/// Server tiruan; berhenti saat runtime tokio test selesai.
#[derive(Clone)]
pub struct MockServer {
    addr: SocketAddr,
    inner: Arc<Mutex<Inner>>,
}

impl MockServer {
    /// Mulai di `127.0.0.1:<port acak>`.
    pub async fn start() -> Self {
        let inner = Arc::new(Mutex::new(Inner::default()));
        let shared = inner.clone();
        let app = axum::Router::new().fallback(
            move |method: Method, uri: Uri, headers: HeaderMap, body: Bytes| {
                let shared = shared.clone();
                async move { handle(&shared, &method, &uri, headers, &body) }
            },
        );
        let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
            Ok(l) => l,
            Err(err) => {
                tracing::error!(error = %err, "mock: bind gagal");
                return Self {
                    addr: SocketAddr::from(([127, 0, 0, 1], 9)),
                    inner,
                };
            }
        };
        let addr = listener
            .local_addr()
            .unwrap_or_else(|_| SocketAddr::from(([127, 0, 0, 1], 9)));
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self { addr, inner }
    }

    /// `http://127.0.0.1:<port>` (tanpa `/` di akhir).
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Tambah aturan balasan. `body = Value::Null` → body kosong.
    pub fn on(&self, method: &str, pattern: &str, status: u16, body: Value) {
        self.lock().rules.push(Rule {
            method: method.to_owned(),
            pattern: pattern.to_owned(),
            status,
            body,
        });
    }

    /// Semua request yang diterima, berurutan.
    pub fn requests(&self) -> Vec<Recorded> {
        self.lock().log.clone()
    }

    /// Request dengan method + path persis.
    pub fn requests_to(&self, method: &str, path: &str) -> Vec<Recorded> {
        self.requests()
            .into_iter()
            .filter(|r| r.method.eq_ignore_ascii_case(method) && r.path == path)
            .collect()
    }

    /// Kosongkan log request (aturan tetap).
    pub fn clear_log(&self) {
        self.lock().log.clear();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn handle(
    inner: &Mutex<Inner>,
    method: &Method,
    uri: &Uri,
    headers: HeaderMap,
    body: &Bytes,
) -> Response {
    let mut g = inner.lock().unwrap_or_else(PoisonError::into_inner);
    let path = uri.path().to_owned();
    g.log.push(Recorded {
        method: method.as_str().to_owned(),
        path: path.clone(),
        query: uri.query().unwrap_or_default().to_owned(),
        headers,
        body: String::from_utf8_lossy(body).into_owned(),
    });
    let rule = g
        .rules
        .iter()
        .rev()
        .find(|r| r.matches(method.as_str(), &path))
        .cloned();
    drop(g);
    match rule {
        Some(r) => {
            let status = StatusCode::from_u16(r.status).unwrap_or(StatusCode::OK);
            if r.body.is_null() {
                (status, Body::empty()).into_response()
            } else {
                (
                    status,
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    r.body.to_string(),
                )
                    .into_response()
            }
        }
        None => (
            StatusCode::NOT_FOUND,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            r#"{"err":"mock: no rule"}"#,
        )
            .into_response(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn records_and_replies_latest_rule() {
        let mock = MockServer::start().await;
        mock.on("GET", "/team", 200, serde_json::json!({ "a": 1 }));
        mock.on("GET", "/team", 500, serde_json::json!({ "a": 2 }));
        mock.on("POST", "/task/*", 204, Value::Null);
        let http = reqwest_like(&mock).await;
        assert_eq!(
            http,
            vec![
                (500, r#"{"a":2}"#.to_owned()),
                (204, String::new()),
                (404, r#"{"err":"mock: no rule"}"#.to_owned())
            ]
        );
        let reqs = mock.requests();
        assert_eq!(reqs.len(), 3);
        assert_eq!(reqs[0].query, "x=1");
        assert_eq!(reqs[1].form(), vec![("k".to_owned(), "a b/c".to_owned())]);
    }

    /// Klien HTTP mentah via tokio (crate ini tidak bergantung pada reqwest).
    async fn reqwest_like(mock: &MockServer) -> Vec<(u16, String)> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut out = Vec::new();
        for raw in [
            "GET /team?x=1 HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n".to_owned(),
            "POST /task/9/comment HTTP/1.1\r\nHost: x\r\nConnection: close\r\nContent-Length: 11\r\n\r\nk=a+b%2Fc".to_owned()
                .replace("Content-Length: 11", "Content-Length: 9"),
            "GET /nope HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n".to_owned(),
        ] {
            let mut s = tokio::net::TcpStream::connect(mock.addr).await.unwrap();
            s.write_all(raw.as_bytes()).await.unwrap();
            let mut buf = String::new();
            s.read_to_string(&mut buf).await.unwrap();
            let status: u16 = buf[9..12].parse().unwrap();
            let body = buf.split_once("\r\n\r\n").map(|(_, b)| b.to_owned()).unwrap_or_default();
            out.push((status, body));
        }
        out
    }
}
