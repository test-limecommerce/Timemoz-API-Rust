//! Port `src/jwt/jwt.strategy.ts`, `token-revocation-cache.ts`, dan
//! `revoke-user-tokens.ts`.
//!
//! Kebijakan pemilik (CLAUDE.md Node): JWT user TIDAK PERNAH kedaluwarsa
//! (`ignoreExpiration: true`) tapi WAJIB bisa dicabut lewat `jwtBlackList` dan
//! `users.tokensValidAfter`. Jangan menambah validasi `exp`.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use axum::http::{HeaderMap, StatusCode};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use sqlx::{PgConnection, PgPool};

use crate::cache::{EpochCache, Keep, shared_err};
use crate::time::{JsDate, now_naive};
use crate::user_status::{STATUS_ACTIVE, STATUS_DELETED, STATUS_DISABLED};
use crate::{ApiError, ApiResult, AppState};

const DEVICE_LOGGED_OUT: &str =
    "This device is not recognized or has been logged out. Please login again.";
const SERVICE_UNAVAILABLE: &str =
    "Authentication service temporarily unavailable. Please try again.";
const SESSION_INVALID: &str = "Your session is no longer valid. Please login again.";

/// `users.id` bertipe Int.
const MAX_USER_ID: i64 = 2_147_483_647;

pub const TOKEN_REVOCATION_TTL: Duration = Duration::from_secs(60);
pub const TOKEN_REVOCATION_STALE_RETENTION: Duration = Duration::from_secs(60 * 60);
pub const TOKEN_REVOCATION_MAX_ENTRIES: usize = 20_000;
/// `revoke-user-tokens.ts` `BLACKLIST_PRUNE_MARGIN_MS`.
pub const BLACKLIST_PRUNE_MARGIN_MS: i64 = 60_000;

#[derive(Debug, Clone)]
pub struct RevocationState {
    pub status: i32,
    /// `tokensValidAfter` dalam milidetik epoch.
    pub valid_after_ms: Option<i64>,
    pub revoked_hashes: Arc<HashSet<String>>,
}

/// `None` di dalam = user tidak ada (jawaban sungguhan, bukan miss).
pub type RevocationCache = EpochCache<i32, Option<RevocationState>>;

pub fn new_revocation_cache() -> RevocationCache {
    EpochCache::new(TOKEN_REVOCATION_MAX_ENTRIES)
}

/// Identitas pemanggil yang dipasang ke request (`req.user` di Node).
#[derive(Debug, Clone)]
pub struct AuthUser {
    /// `payload.userId`. `None` untuk token operator (`{ username, sub }`).
    pub id: Option<i32>,
    /// Header `x-mac-address`, lowercase.
    pub mac_address: String,
    /// Header `x-os`.
    pub os: Option<String>,
    /// `sid` sesi desktop, kalau token membawanya.
    pub sid: Option<String>,
    /// Payload mentah, untuk handler yang butuh field lain.
    pub claims: Arc<Map<String, Value>>,
    /// Token mentah (untuk logout / blacklist).
    pub raw_token: Arc<str>,
}

/// `ExtractJwt.fromAuthHeaderAsBearerToken()`.
pub fn extract_bearer(headers: &HeaderMap) -> Option<&str> {
    let raw = headers.get(http::header::AUTHORIZATION)?.to_str().ok()?;
    let mut parts = raw.split_whitespace();
    let scheme = parts.next()?;
    let token = parts.next()?;
    scheme.eq_ignore_ascii_case("bearer").then_some(token)
}

/// Harus sama dengan `encode(sha256(convert_to(jwt,'UTF8')),'hex')` di migration.
pub fn hash_jwt(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

fn decoding_validation() -> Validation {
    let mut v = Validation::new(Algorithm::HS256);
    v.algorithms = vec![Algorithm::HS256, Algorithm::HS384, Algorithm::HS512];
    v.validate_exp = false;
    v.validate_nbf = true;
    v.required_spec_claims = HashSet::new();
    v.validate_aud = false;
    v
}

pub fn decode_claims(secret: &str, token: &str) -> Option<Map<String, Value>> {
    jsonwebtoken::decode::<Map<String, Value>>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &decoding_validation(),
    )
    .ok()
    .map(|d| d.claims)
}

/// Format `expiresIn` paket `ms` yang dipakai `@nestjs/jwt`: `60`, `"60s"`,
/// `"30m"`, `"24h"`, `"7d"`, `"2 days"`. Angka polos = detik.
pub fn parse_expires_in(raw: &str) -> Option<i64> {
    let raw = raw.trim();
    if let Ok(secs) = raw.parse::<i64>() {
        return Some(secs);
    }
    let split = raw.find(|c: char| !c.is_ascii_digit() && c != '.')?;
    let (num, unit) = raw.split_at(split);
    let num: f64 = num.parse().ok()?;
    let unit = unit.trim().to_ascii_lowercase();
    let mult = match unit.as_str() {
        "ms" | "msec" | "msecs" | "millisecond" | "milliseconds" => 0.001,
        "s" | "sec" | "secs" | "second" | "seconds" => 1.0,
        "m" | "min" | "mins" | "minute" | "minutes" => 60.0,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3_600.0,
        "d" | "day" | "days" => 86_400.0,
        "w" | "week" | "weeks" => 604_800.0,
        "y" | "yr" | "yrs" | "year" | "years" => 31_557_600.0,
        _ => return None,
    };
    #[allow(clippy::cast_possible_truncation)]
    Some((num * mult).round() as i64)
}

/// `jwtService.sign(payload)` — HS256, `iat` otomatis, `exp` dari `JWTEXPIRED_IN`.
pub fn sign(state: &AppState, mut payload: Map<String, Value>) -> ApiResult<String> {
    let now = chrono::Utc::now().timestamp();
    payload.insert("iat".into(), Value::from(now));
    if let Some(exp) = state
        .config
        .jwt_expires_in
        .as_deref()
        .and_then(parse_expires_in)
    {
        payload.insert("exp".into(), Value::from(now + exp));
    }
    jsonwebtoken::encode(
        &Header::new(Algorithm::HS256),
        &payload,
        &EncodingKey::from_secret(state.config.jwt_secret.as_bytes()),
    )
    .map_err(ApiError::internal)
}

fn session_invalid() -> ApiError {
    ApiError::status_false(StatusCode::UNAUTHORIZED, SESSION_INVALID)
}

fn as_user_id(v: Option<&Value>) -> Option<i32> {
    let n = v?.as_i64()?;
    // `Number.isInteger` menolak 1.5; `as_i64` sudah menolak float non-bulat.
    if n > 0 && n <= MAX_USER_ID {
        i32::try_from(n).ok()
    } else {
        None
    }
}

/// `UsersService.findTokenRevocationState`.
async fn fetch_revocation_state(
    db: &PgPool,
    user_id: i32,
) -> sqlx::Result<Option<RevocationState>> {
    let user = sqlx::query_as::<_, (i32, Option<JsDate>)>(
        r#"SELECT status, "tokensValidAfter" FROM users WHERE id = $1"#,
    )
    .bind(user_id)
    .fetch_optional(db);
    let revoked = sqlx::query_scalar::<_, String>(
        r#"SELECT "tokenHash" FROM "jwtBlackList" WHERE "userId" = $1"#,
    )
    .bind(user_id)
    .fetch_all(db);
    let (user, revoked) = tokio::try_join!(user, revoked)?;
    Ok(user.map(|(status, valid_after)| RevocationState {
        status,
        valid_after_ms: valid_after.map(|d| d.timestamp_millis()),
        revoked_hashes: Arc::new(revoked.into_iter().collect()),
    }))
}

async fn load_revocation_state(
    state: &AppState,
    user_id: i32,
) -> ApiResult<Option<RevocationState>> {
    let cache = &state.caches.revocation;
    let db = state.db.clone();
    let loaded = cache
        .get_or_load(
            user_id,
            |_| {
                Some(Keep {
                    fresh: TOKEN_REVOCATION_TTL,
                    retain: TOKEN_REVOCATION_STALE_RETENTION,
                })
            },
            move || async move {
                fetch_revocation_state(&db, user_id)
                    .await
                    .map_err(shared_err)
            },
        )
        .await;
    match loaded {
        Ok(v) => Ok(v),
        Err(err) => {
            if let Some(stale) = cache.get_stale(&user_id) {
                tracing::warn!(user_id, error = %err, "Token revocation check served from stale cache");
                return Ok(stale);
            }
            // 503, bukan 401: klien bisa membaca 401 sebagai "sudah logout".
            tracing::error!(user_id, error = %err, "{SERVICE_UNAVAILABLE}");
            Err(ApiError::status_false(
                StatusCode::SERVICE_UNAVAILABLE,
                SERVICE_UNAVAILABLE,
            ))
        }
    }
}

async fn assert_not_revoked(
    state: &AppState,
    claims: &Map<String, Value>,
    raw_token: &str,
) -> ApiResult<()> {
    // State OAuth ditandatangani dengan secret yang sama dan membawa `type`.
    let Some(user_id) = as_user_id(claims.get("userId")) else {
        return Err(session_invalid());
    };
    if claims.get("type").is_some_and(|t| !t.is_null()) {
        return Err(session_invalid());
    }
    let Some(rev) = load_revocation_state(state, user_id).await? else {
        return Err(session_invalid());
    };
    if rev.status == STATUS_DELETED || rev.status == STATUS_DISABLED {
        return Err(session_invalid());
    }
    if let Some(valid_after_ms) = rev.valid_after_ms {
        // Per detik penuh, mengikuti resolusi `iat`.
        let cutoff = valid_after_ms.div_euclid(1000);
        match claims.get("iat").and_then(Value::as_f64) {
            #[allow(clippy::cast_precision_loss)]
            Some(iat) if iat >= cutoff as f64 => {}
            _ => return Err(session_invalid()),
        }
    }
    if !rev.revoked_hashes.is_empty() && rev.revoked_hashes.contains(&hash_jwt(raw_token)) {
        return Err(session_invalid());
    }
    Ok(())
}

async fn assert_active_session(
    state: &AppState,
    user_id: Option<i32>,
    sid: &Value,
    url: &str,
) -> ApiResult<()> {
    let (Some(user_id), Some(sid)) = (
        user_id,
        sid.as_str().filter(|s| crate::uuid_rules::is_uuid_v4(s)),
    ) else {
        return Err(ApiError::status_false(
            StatusCode::UNAUTHORIZED,
            DEVICE_LOGGED_OUT,
        ));
    };
    let session = sqlx::query_scalar::<_, i32>(
        r#"SELECT id FROM "userConnected" WHERE "userId" = $1 AND sid = $2::uuid AND status = 1 LIMIT 1"#,
    )
    .bind(user_id)
    .bind(sid)
    .fetch_optional(&state.db)
    .await
    .map_err(|err| {
        tracing::error!(user_id, url, error = %err, "{SERVICE_UNAVAILABLE}");
        ApiError::status_false(StatusCode::UNAUTHORIZED, SERVICE_UNAVAILABLE)
    })?;
    if session.is_none() {
        tracing::warn!(user_id, url, "{DEVICE_LOGGED_OUT}");
        return Err(ApiError::status_false(
            StatusCode::UNAUTHORIZED,
            DEVICE_LOGGED_OUT,
        ));
    }
    Ok(())
}

async fn assert_legacy_device(
    state: &AppState,
    user_id: Option<i32>,
    mac: &str,
    url: &str,
) -> ApiResult<()> {
    let rows = match user_id {
        Some(uid) => sqlx::query_as::<_, (String, i32)>(
            r#"SELECT "macAddress", status FROM "userConnected" WHERE "userId" = $1"#,
        )
        .bind(uid)
        .fetch_all(&state.db)
        .await
        .map_err(|err| {
            tracing::error!(user_id = ?user_id, url, mac, error = %err, "{SERVICE_UNAVAILABLE}");
            ApiError::status_false(StatusCode::UNAUTHORIZED, SERVICE_UNAVAILABLE)
        })?,
        // Prisma melempar untuk `userId: undefined` → cabang catch → 401 unavailable.
        None => {
            return Err(ApiError::status_false(
                StatusCode::UNAUTHORIZED,
                SERVICE_UNAVAILABLE,
            ));
        }
    };
    let ok = rows
        .iter()
        .any(|(m, status)| m.to_lowercase() == mac && *status == 1);
    if !ok {
        tracing::error!(user_id = ?user_id, url, mac, "{DEVICE_LOGGED_OUT}");
        return Err(ApiError::status_false(
            StatusCode::UNAUTHORIZED,
            DEVICE_LOGGED_OUT,
        ));
    }
    Ok(())
}

/// `JwtAuthGuard` + `JwtStrategy.validate`. `url` = path + query (Node `req.url`).
pub async fn authenticate(state: &AppState, headers: &HeaderMap, url: &str) -> ApiResult<AuthUser> {
    let token = extract_bearer(headers).ok_or(ApiError::Bare(StatusCode::UNAUTHORIZED))?;
    let claims = decode_claims(&state.config.jwt_secret, token)
        .ok_or(ApiError::Bare(StatusCode::UNAUTHORIZED))?;

    let is_operator_token = claims.get("userId").is_none()
        && claims.get("sub").is_some_and(Value::is_number)
        && claims.get("type").is_none();
    if !is_operator_token {
        assert_not_revoked(state, &claims, token).await?;
    }

    let mac = headers
        .get("x-mac-address")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_lowercase();
    let os = headers
        .get("x-os")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let is_sensitive = ["/auth/me", "/projects", "/tracking-timelines"]
        .iter()
        .any(|r| url.contains(r));
    let user_id = as_user_id(claims.get("userId"));

    let sid = match claims.get("sid") {
        Some(sid) => {
            if is_sensitive {
                assert_active_session(state, user_id, sid, url).await?;
            }
            Some(
                sid.as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| sid.to_string()),
            )
        }
        None => {
            if is_sensitive && !mac.is_empty() {
                assert_legacy_device(state, user_id, &mac, url).await?;
            }
            None
        }
    };

    Ok(AuthUser {
        id: user_id,
        mac_address: mac,
        os,
        sid,
        claims: Arc::new(claims),
        raw_token: Arc::from(token),
    })
}

/// `blacklistToken` — menolak SATU token (logout). Panggil di dalam transaksi,
/// lalu [`crate::state::AppState::invalidate_token_revocation`] SESUDAH commit.
pub async fn blacklist_token(
    conn: &mut PgConnection,
    raw_token: &str,
    user_id: i32,
) -> sqlx::Result<()> {
    let at = now_naive();
    sqlx::query(
        r#"INSERT INTO "jwtBlackList" ("tokenHash", "userId", "revokedAt", "createdAt", status)
           VALUES ($1, $2, $3, $3, $4)
           ON CONFLICT ("tokenHash") DO NOTHING"#,
    )
    .bind(hash_jwt(raw_token))
    .bind(user_id)
    .bind(at)
    .bind(STATUS_ACTIVE)
    .execute(conn)
    .await?;
    Ok(())
}

/// `revokeAllUserTokens` — mencabut SEMUA kredensial user (JWT lewat
/// `tokensValidAfter`, PAT & token OAuth MCP). Panggil di dalam transaksi
/// pemanggil, invalidasi cache SESUDAH commit.
pub async fn revoke_all_user_tokens(conn: &mut PgConnection, user_id: i32) -> sqlx::Result<()> {
    let at = now_naive();
    let prune_before = at - chrono::Duration::milliseconds(BLACKLIST_PRUNE_MARGIN_MS);
    sqlx::query(r#"UPDATE users SET "tokensValidAfter" = $2 WHERE id = $1"#)
        .bind(user_id)
        .bind(at)
        .execute(&mut *conn)
        .await?;
    sqlx::query(r#"DELETE FROM "jwtBlackList" WHERE "userId" = $1 AND "revokedAt" < $2"#)
        .bind(user_id)
        .bind(prune_before)
        .execute(&mut *conn)
        .await?;
    for table in [
        "mcpAccessToken",
        "mcpOAuthAccessToken",
        "mcpOAuthRefreshToken",
    ] {
        sqlx::query(&format!(
            r#"UPDATE "{table}" SET "revokedAt" = $2 WHERE "userId" = $1 AND "revokedAt" IS NULL"#
        ))
        .bind(user_id)
        .bind(at)
        .execute(&mut *conn)
        .await?;
    }
    sqlx::query(
        r#"UPDATE "mcpOAuthCode" SET "usedAt" = $2 WHERE "userId" = $1 AND "usedAt" IS NULL"#,
    )
    .bind(user_id)
    .bind(at)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_extraction_matches_passport() {
        let mut h = HeaderMap::new();
        h.insert(
            http::header::AUTHORIZATION,
            http::HeaderValue::from_static("bearer abc.def"),
        );
        assert_eq!(extract_bearer(&h), Some("abc.def"));
        h.insert(
            http::header::AUTHORIZATION,
            http::HeaderValue::from_static("Basic abc"),
        );
        assert_eq!(extract_bearer(&h), None);
    }

    #[test]
    fn expires_in_formats() {
        assert_eq!(parse_expires_in("7d"), Some(604_800));
        assert_eq!(parse_expires_in("24h"), Some(86_400));
        assert_eq!(parse_expires_in("3600"), Some(3600));
        assert_eq!(parse_expires_in("2 days"), Some(172_800));
    }

    #[test]
    fn expired_tokens_are_still_accepted() {
        let secret = "s";
        let claims = serde_json::json!({ "userId": 1, "iat": 1, "exp": 2 });
        let token = jsonwebtoken::encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(secret.as_bytes()),
        )
        .unwrap_or_default();
        assert!(decode_claims(secret, &token).is_some());
        assert!(decode_claims("wrong", &token).is_none());
    }

    #[test]
    fn hash_matches_node_sha256_hex() {
        assert_eq!(
            hash_jwt("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
