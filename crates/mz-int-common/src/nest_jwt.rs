//! `JwtService` dari `JwtModule.register({ secret: process.env.SECRET_TOKEN })` di modul
//! integrasi (Discord, Jira, Google Calendar). Tanpa `signOptions`: `sign(p)` tanpa `exp`,
//! `sign(p, { expiresIn: '10m' })` menambah `exp`. `verify(token)` = `jsonwebtoken.verify`:
//! algoritma HS256/384/512, `exp` dan `nbf` DIPERIKSA tanpa toleransi.

use std::collections::HashSet;

use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use mz_core::AppState;
use serde_json::{Map, Value};

/// `jwtService.sign(payload, { expiresIn })` (`expires_in_secs = None` = tanpa opsi).
/// `iat` = detik sekarang (dibulatkan ke bawah, seperti `jsonwebtoken`).
pub fn sign(
    state: &AppState,
    mut payload: Map<String, Value>,
    expires_in_secs: Option<i64>,
) -> Result<String, jsonwebtoken::errors::Error> {
    let now = chrono::Utc::now().timestamp();
    payload.insert("iat".into(), Value::from(now));
    if let Some(secs) = expires_in_secs {
        payload.insert("exp".into(), Value::from(now + secs));
    }
    jsonwebtoken::encode(
        &Header::new(Algorithm::HS256),
        &payload,
        &EncodingKey::from_secret(state.config.jwt_secret.as_bytes()),
    )
}

/// `jwtService.verify(token)`: `None` untuk tanda tangan salah, kedaluwarsa, atau rusak
/// (Node melempar; pemanggil memetakan ke error masing-masing).
pub fn verify(state: &AppState, token: &str) -> Option<Map<String, Value>> {
    let mut v = Validation::new(Algorithm::HS256);
    v.algorithms = vec![Algorithm::HS256, Algorithm::HS384, Algorithm::HS512];
    v.validate_exp = true;
    v.validate_nbf = true;
    v.leeway = 0;
    v.required_spec_claims = HashSet::new();
    v.validate_aud = false;
    jsonwebtoken::decode::<Map<String, Value>>(
        token,
        &DecodingKey::from_secret(state.config.jwt_secret.as_bytes()),
        &v,
    )
    .ok()
    .map(|d| d.claims)
}
