//! Port `src/rate-limit/*` (express-rate-limit + rate-limit-redis).
//!
//! Kunci Redis & skripnya SAMA dengan `rate-limit-redis` v4, jadi selama Node dan
//! Rust berjalan berdampingan keduanya berbagi satu bucket:
//! `"rl:" + keyPrefix + ":" + key` (prefix `rl:` bawaan RedisStore).

use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use fred::prelude::*;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::AppState;

const SCRIPT: &str = r#"
local totalHits = redis.call("INCR", KEYS[1])
local timeToExpire = redis.call("PTTL", KEYS[1])
if timeToExpire <= 0 or ARGV[1] == "1"
then
  redis.call("PEXPIRE", KEYS[1], tonumber(ARGV[2]))
  timeToExpire = tonumber(ARGV[2])
end
return { totalHits, timeToExpire }
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateKey {
    /// `req.ip` (trust proxy 1).
    Ip,
    /// sha256(lower(trim(body.username)))[0..32].
    Email,
    /// sha256(bearer token)[0..32] — bawaan `createRateLimiter`.
    Token,
}

#[derive(Debug, Clone, Copy)]
pub struct RateRule {
    pub key_prefix: &'static str,
    /// `None` = `RATE_LIMIT_MAX` (bawaan 20).
    pub max: Option<u64>,
    pub window_ms: u64,
    pub key: RateKey,
    pub pass_on_store_error: bool,
}

const MINUTE: u64 = 60_000;
const HOUR: u64 = 60 * MINUTE;

impl RateRule {
    pub const fn auth(key_prefix: &'static str, max: u64, window_ms: u64, key: RateKey) -> Self {
        Self {
            key_prefix,
            max: Some(max),
            window_ms,
            key,
            pass_on_store_error: true,
        }
    }
    pub const fn token(key_prefix: &'static str, max: u64, window_ms: u64) -> Self {
        Self {
            key_prefix,
            max: Some(max),
            window_ms,
            key: RateKey::Token,
            pass_on_store_error: false,
        }
    }
}

/// Tabel `rate-limit/auth-rate-limits.ts` untuk route milik crate ini.
pub mod rules {
    use super::*;
    pub const AUTH_LOGIN: RateRule = RateRule::auth("rl:auth-login", 60, MINUTE, RateKey::Ip);
    pub const AUTH_FORGOT_GEN: RateRule =
        RateRule::auth("rl:auth-forgot-gen", 5, MINUTE, RateKey::Ip);
    pub const AUTH_FORGOT_GEN_EMAIL: RateRule =
        RateRule::auth("rl:auth-forgot-gen-email", 5, HOUR, RateKey::Email);
    pub const AUTH_RESEND_CONFIRM: RateRule =
        RateRule::auth("rl:auth-resend-confirm", 5, MINUTE, RateKey::Ip);
    pub const AUTH_RESEND_CONFIRM_EMAIL: RateRule =
        RateRule::auth("rl:auth-resend-confirm-email", 5, HOUR, RateKey::Email);
    pub const AUTH_REGISTER: RateRule = RateRule::auth("rl:auth-register", 10, MINUTE, RateKey::Ip);
    pub const AUTH_RESET: RateRule = RateRule::auth("rl:auth-reset", 10, MINUTE, RateKey::Ip);
    pub const AUTH_CONFIRM: RateRule = RateRule::auth("rl:auth-confirm", 10, MINUTE, RateKey::Ip);
    pub const AUTH_REGISTER_INVITE: RateRule =
        RateRule::auth("rl:auth-register-invite", 20, MINUTE, RateKey::Ip);
    pub const AUTH_INVITE_INFO: RateRule =
        RateRule::auth("rl:auth-invite-info", 30, MINUTE, RateKey::Ip);
    pub const AUTH_CHECK_VERIFICATION: RateRule =
        RateRule::auth("rl:auth-check-verification", 30, MINUTE, RateKey::Ip);
    pub const AUTH_CHANGE_PASSWORD: RateRule =
        RateRule::auth("rl:auth-change-password", 5, MINUTE, RateKey::Token);
    pub const AUTH_LOGOUT: RateRule = RateRule::auth("rl:auth-logout", 3, MINUTE, RateKey::Token);
    pub const AUTH_WEB_SESSION: RateRule =
        RateRule::auth("rl:auth-web-session", 10, MINUTE, RateKey::Token);
    /// `main.ts` — `/users/delete-account`.
    pub const USERS_DELETE_ACCOUNT: RateRule =
        RateRule::token("rl:users-delete-account", 5, MINUTE);
    /// `auth-rate-limits.ts` (`DEMO_REQUESTS`) — `POST /demo-requests`, form publik landing page.
    pub const DEMO_REQUESTS: RateRule = RateRule::auth("rl:demo-requests", 3, MINUTE, RateKey::Ip);
}

pub fn hash32(input: &str) -> String {
    let mut h = hex::encode(Sha256::digest(input.as_bytes()));
    h.truncate(32);
    h
}

/// `req.ip` dengan `trust proxy = 1`: entri paling kanan `X-Forwarded-For`,
/// atau alamat socket. Seperti Node (`http` menggabungkan header XFF berulang dengan `, `,
/// `proxy-addr` melewati entri kosong): semua baris header dibaca, entri kosong dilewati.
pub fn client_ip(headers: &HeaderMap, peer: Option<std::net::SocketAddr>) -> Option<String> {
    let last = headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .rfind(|s| !s.is_empty());
    if let Some(last) = last {
        return Some(last.to_owned());
    }
    peer.map(|p| match p.ip() {
        // Node dual-stack menampilkan IPv4 sebagai `::ffff:a.b.c.d`.
        std::net::IpAddr::V4(v4) => format!("::ffff:{v4}"),
        std::net::IpAddr::V6(v6) => v6.to_string(),
    })
}

pub enum Outcome {
    Pass(Option<Headers>),
    Limited(Response),
}

#[derive(Debug, Clone)]
pub struct Headers {
    limit: u64,
    remaining: u64,
    reset_secs: u64,
    window_secs: u64,
}

impl Headers {
    pub fn apply(&self, h: &mut HeaderMap) {
        let set = |h: &mut HeaderMap, k: &'static str, v: String| {
            if let Ok(v) = HeaderValue::from_str(&v) {
                h.insert(k, v);
            }
        };
        set(
            h,
            "ratelimit-policy",
            format!("{};w={}", self.limit, self.window_secs),
        );
        set(h, "ratelimit-limit", self.limit.to_string());
        set(h, "ratelimit-remaining", self.remaining.to_string());
        set(h, "ratelimit-reset", self.reset_secs.to_string());
    }
}

/// Jalankan satu aturan. `key = None` → dilewati (`skip` di Node).
pub async fn check(state: &AppState, rule: &RateRule, key: Option<String>) -> Outcome {
    let Some(key) = key else {
        return Outcome::Pass(None);
    };
    let max = rule.max.unwrap_or(state.config.rate_limit_max);
    let redis_key = format!("rl:{}:{}", rule.key_prefix, key);
    let res: Result<Vec<i64>, _> = state
        .redis
        .next()
        .eval(
            SCRIPT,
            vec![redis_key],
            vec!["0".to_owned(), rule.window_ms.to_string()],
        )
        .await;
    let (hits, ttl_ms) = match res.as_deref() {
        Ok([hits, ttl, ..]) => (
            u64::try_from(*hits).unwrap_or(0),
            u64::try_from(*ttl).unwrap_or(0),
        ),
        other => {
            tracing::error!(prefix = rule.key_prefix, result = ?other.map_err(|e| e.to_string()), "rate limit store error");
            if rule.pass_on_store_error {
                return Outcome::Pass(None);
            }
            return Outcome::Limited(
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    axum::Json(json!({ "statusCode": 500, "message": "Internal server error" })),
                )
                    .into_response(),
            );
        }
    };
    let headers = Headers {
        limit: max,
        remaining: max.saturating_sub(hits),
        reset_secs: ttl_ms.div_ceil(1000),
        window_secs: rule.window_ms / 1000,
    };
    if hits > max {
        let mut res = (
            StatusCode::TOO_MANY_REQUESTS,
            axum::Json(json!({ "success": false, "message": "Too many requests, please try again later." })),
        )
            .into_response();
        headers.apply(res.headers_mut());
        if let Ok(v) = HeaderValue::from_str(&headers.reset_secs.to_string()) {
            res.headers_mut().insert(http::header::RETRY_AFTER, v);
        }
        return Outcome::Limited(res);
    }
    Outcome::Pass(Some(headers))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xff_rightmost() {
        let mut h = HeaderMap::new();
        h.insert(
            "x-forwarded-for",
            HeaderValue::from_static("1.1.1.1, 2.2.2.2"),
        );
        assert_eq!(client_ip(&h, None).as_deref(), Some("2.2.2.2"));
    }

    #[test]
    fn xff_multiple_lines_and_empty_entries() {
        let mut h = HeaderMap::new();
        h.append("x-forwarded-for", HeaderValue::from_static("6.6.6.6"));
        h.append("x-forwarded-for", HeaderValue::from_static("3.3.3.3, "));
        assert_eq!(client_ip(&h, None).as_deref(), Some("3.3.3.3"));
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", HeaderValue::from_static(" , "));
        let peer = "10.0.0.1:5000".parse().ok();
        assert_eq!(client_ip(&h, peer).as_deref(), Some("::ffff:10.0.0.1"));
    }

    #[test]
    fn hash_is_32_hex() {
        assert_eq!(hash32("abc").len(), 32);
    }
}
