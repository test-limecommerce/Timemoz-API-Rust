//! Perlindungan CSRF untuk login/register Google (OAuth `state`).
//!
//! Skema IDENTIK dengan Node `src/google-oauth/oauth-state-signing.ts`, supaya
//! login yang dimulai di salah satu server bisa diselesaikan server lain:
//!
//! ```text
//! state   = "v1." + b64url(payloadJson) + "." + b64url(HMAC_SHA256(SECRET_TOKEN,
//!           "google-oauth-state:v1." + b64url(payloadJson)))
//! payload = { ...state lama, n: nonce, exp: detik epoch }
//! cookie  = mz_goauth=<nonce>; Path=/auth; HttpOnly; SameSite=Lax; Max-Age=600 (+ Secure)
//! ```
//!
//! Callback hanya menerima state yang tanda tangannya sah, belum kedaluwarsa,
//! dan nonce-nya sama dengan cookie dari browser yang memulai login.

use axum::http::HeaderValue;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac};
use rand::RngCore;
use serde_json::{Map, Value};
use sha2::Sha256;
use subtle::ConstantTimeEq;

use mz_core::AppState;

pub const OAUTH_STATE_COOKIE: &str = "mz_goauth";
pub const OAUTH_STATE_TTL_SECONDS: i64 = 600;
const VERSION: &str = "v1";
const MAC_CONTEXT: &str = "google-oauth-state:v1.";
/// Pesan yang sama dengan `GoogleOAuthStateInvalidException` di Node.
pub const INVALID_STATE_MESSAGE: &str =
    "Google sign-in expired or was not started from this browser. Please try again.";

fn mac(secret: &str, payload_b64: &str) -> Option<Vec<u8>> {
    let mut m = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).ok()?;
    m.update(MAC_CONTEXT.as_bytes());
    m.update(payload_b64.as_bytes());
    Some(m.finalize().into_bytes().to_vec())
}

pub fn new_nonce() -> String {
    let mut buf = [0u8; 32];
    rand::rng().fill_bytes(&mut buf);
    URL_SAFE_NO_PAD.encode(buf)
}

/// `signOAuthState(payload, nonce, secret, now)`. `payload_json` = state lama
/// (`build_state`); `n` dan `exp` ditambahkan di sini.
pub fn sign_state(payload_json: &str, nonce: &str, secret: &str, now_secs: i64) -> Option<String> {
    let mut obj = match serde_json::from_str::<Value>(payload_json).ok()? {
        Value::Object(o) => o,
        _ => Map::new(),
    };
    obj.insert("n".into(), Value::String(nonce.to_owned()));
    obj.insert(
        "exp".into(),
        Value::from(now_secs + OAUTH_STATE_TTL_SECONDS),
    );
    let json = serde_json::to_string(&Value::Object(obj)).ok()?;
    let payload_b64 = URL_SAFE_NO_PAD.encode(json.as_bytes());
    let sig = URL_SAFE_NO_PAD.encode(mac(secret, &payload_b64)?);
    Some(format!("{VERSION}.{payload_b64}.{sig}"))
}

/// `verifyOAuthState` — JSON payload bila sah untuk cookie ini, `None` bila tidak.
pub fn verify_state(
    raw: Option<&str>,
    cookie_nonce: Option<&str>,
    secret: &str,
    now_secs: i64,
) -> Option<String> {
    let raw = raw?;
    let cookie_nonce = cookie_nonce.filter(|c| !c.is_empty())?;
    if secret.is_empty() {
        return None;
    }
    let mut parts = raw.split('.');
    let (Some(version), Some(payload_b64), Some(sig_b64), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    if version != VERSION {
        return None;
    }
    let expected = mac(secret, payload_b64)?;
    let given = URL_SAFE_NO_PAD.decode(sig_b64).ok()?;
    if expected.len() != given.len() || !bool::from(expected.ct_eq(&given)) {
        return None;
    }
    let json = String::from_utf8(URL_SAFE_NO_PAD.decode(payload_b64).ok()?).ok()?;
    let Value::Object(obj) = serde_json::from_str::<Value>(&json).ok()? else {
        return None;
    };
    let exp = obj.get("exp").and_then(Value::as_f64)?;
    #[allow(clippy::cast_precision_loss)]
    if exp <= now_secs as f64 {
        return None;
    }
    let nonce = obj.get("n").and_then(Value::as_str)?;
    if nonce.len() != cookie_nonce.len()
        || !bool::from(nonce.as_bytes().ct_eq(cookie_nonce.as_bytes()))
    {
        return None;
    }
    Some(json)
}

/// Ambil satu cookie dari header `Cookie` mentah.
pub fn read_cookie<'a>(headers: &'a axum::http::HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(axum::http::header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|part| {
            let (k, v) = part.split_once('=')?;
            (k.trim() == name).then(|| v.trim())
        })
}

fn secure(state: &AppState) -> bool {
    state
        .config
        .var("HOST_BE")
        .is_some_and(|h| h.starts_with("https://"))
}

/// `Set-Cookie` untuk nonce baru (setara `res.cookie(...)` Express).
pub fn set_cookie(state: &AppState, nonce: &str) -> Option<HeaderValue> {
    let expires = (chrono::Utc::now() + chrono::Duration::seconds(OAUTH_STATE_TTL_SECONDS))
        .format("%a, %d %b %Y %H:%M:%S GMT");
    let mut v = format!(
        "{OAUTH_STATE_COOKIE}={nonce}; Max-Age={OAUTH_STATE_TTL_SECONDS}; Path=/auth; Expires={expires}; HttpOnly"
    );
    if secure(state) {
        v.push_str("; Secure");
    }
    v.push_str("; SameSite=Lax");
    HeaderValue::from_str(&v).ok()
}

/// `res.clearCookie(...)` — cookie sekali pakai.
pub fn clear_cookie(state: &AppState) -> HeaderValue {
    let mut v = format!(
        "{OAUTH_STATE_COOKIE}=; Path=/auth; Expires=Thu, 01 Jan 1970 00:00:00 GMT; HttpOnly"
    );
    if secure(state) {
        v.push_str("; Secure");
    }
    v.push_str("; SameSite=Lax");
    HeaderValue::from_str(&v)
        .unwrap_or_else(|_| HeaderValue::from_static("mz_goauth=; Path=/auth; Max-Age=0"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "test-secret";
    const NOW: i64 = 1_800_000_000;

    #[test]
    fn round_trip() {
        let s = sign_state(
            r#"{"action":"register","macAddress":"aa"}"#,
            "n1",
            SECRET,
            NOW,
        )
        .unwrap_or_default();
        let json = verify_state(Some(&s), Some("n1"), SECRET, NOW + 1).unwrap_or_default();
        let v: Value = serde_json::from_str(&json).unwrap_or(Value::Null);
        assert_eq!(v.get("action").and_then(Value::as_str), Some("register"));
    }

    #[test]
    fn rejects_other_browser_expired_tampered_and_legacy() {
        let s = sign_state(r#"{"action":"login"}"#, "n1", SECRET, NOW).unwrap_or_default();
        assert!(verify_state(Some(&s), Some("n2"), SECRET, NOW).is_none());
        assert!(verify_state(Some(&s), None, SECRET, NOW).is_none());
        assert!(
            verify_state(Some(&s), Some("n1"), SECRET, NOW + OAUTH_STATE_TTL_SECONDS).is_none()
        );
        assert!(verify_state(Some(&s), Some("n1"), "other", NOW).is_none());
        let mut parts: Vec<&str> = s.split('.').collect();
        let forged = URL_SAFE_NO_PAD.encode(br#"{"action":"register","n":"n1","exp":1800000600}"#);
        if let Some(p) = parts.get_mut(1) {
            *p = &forged;
        }
        assert!(verify_state(Some(&parts.join(".")), Some("n1"), SECRET, NOW).is_none());
        assert!(verify_state(Some(r#"{"action":"login"}"#), Some("n1"), SECRET, NOW).is_none());
    }

    /// Vektor yang sama dengan `oauth-state-signing.spec.ts` di Node.
    #[test]
    fn shared_vector_matches_node() {
        let state = "v1.eyJhY3Rpb24iOiJsb2dpbiIsIm4iOiJhYmMiLCJleHAiOjE4MDAwMDA2MDB9.\
                     eYjlP8Ce0YFKvM6ZdV0CgxQHLeHJe6kF-HPEkG28L2Q";
        assert_eq!(
            verify_state(Some(state), Some("abc"), SECRET, NOW).as_deref(),
            Some(r#"{"action":"login","n":"abc","exp":1800000600}"#)
        );
    }

    #[test]
    fn cookie_parsing() {
        let mut h = axum::http::HeaderMap::new();
        h.insert(
            axum::http::header::COOKIE,
            HeaderValue::from_static("a=1; mz_goauth=xyz; b=2"),
        );
        assert_eq!(read_cookie(&h, OAUTH_STATE_COOKIE), Some("xyz"));
    }
}
