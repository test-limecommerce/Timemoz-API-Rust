//! Sesi & token: `device-session.ts`, `AuthService.logout`, `createWebSession`,
//! `signReplacementToken`, dan isi `GET /auth/me`.

use axum::http::StatusCode;
use fred::prelude::*;
use fred::types::Expiration;
use mz_core::db::advisory_xact_lock;
use mz_core::jwt::{blacklist_token, sign};
use mz_core::time::now_naive;
use mz_core::{ApiError, ApiResult, AppState};
use serde_json::{Map, Value, json};

/// Payload JWT Node: `{ userId }` atau `{ userId, sid }` (`iat`/`exp` ditambah `sign`).
pub fn sign_user_token(state: &AppState, user_id: i32, sid: Option<Value>) -> ApiResult<String> {
    let mut payload = Map::new();
    payload.insert("userId".into(), Value::from(user_id));
    if let Some(sid) = sid {
        payload.insert("sid".into(), sid);
    }
    sign(state, payload)
}

/// `activateDeviceSession(prisma, userId, macAddress, os)` — satu-satunya sesi
/// desktop aktif milik user; `sid` baru di SETIAP login. Advisory lock per user
/// menserialkan dua login bersamaan dari device berbeda.
pub async fn activate_device_session(
    state: &AppState,
    user_id: i32,
    mac_address: &str,
    os: Option<&str>,
) -> ApiResult<String> {
    let sid = uuid::Uuid::new_v4().to_string();
    let now = now_naive();
    let os = os.filter(|o| !o.is_empty()).unwrap_or("-");
    let mut tx = state.db.begin().await?;
    advisory_xact_lock(&mut tx, &format!("device-session:{user_id}")).await?;
    sqlx::query(
        r#"UPDATE "userConnected" SET status = 0
           WHERE "userId" = $1 AND status = 1 AND NOT ("macAddress" = $2)"#,
    )
    .bind(user_id)
    .bind(mac_address)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        r#"INSERT INTO "userConnected" ("userId", "macAddress", sid, os, status, "connectedAt", "lastConnectedAt")
           VALUES ($1, $2, $3::uuid, $4, 1, $5, $5)
           ON CONFLICT ("userId", "macAddress")
           DO UPDATE SET status = 1, sid = EXCLUDED.sid, os = EXCLUDED.os, "lastConnectedAt" = EXCLUDED."lastConnectedAt""#,
    )
    .bind(user_id)
    .bind(mac_address)
    .bind(&sid)
    .bind(os)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(sid)
}

/// Device mana yang diputus saat logout.
#[derive(Debug, PartialEq, Eq)]
pub enum DeviceScope<'a> {
    Sid(&'a str),
    Mac(&'a str),
    None,
}

/// `sid` string non-kosong menang; selain itu MAC header (non-kosong); token web
/// (tanpa keduanya) tidak memutus device apa pun.
pub fn device_scope<'a>(sid: Option<&'a Value>, mac_address: &'a str) -> DeviceScope<'a> {
    match sid.and_then(Value::as_str) {
        Some(s) if !s.is_empty() => DeviceScope::Sid(s),
        _ if !mac_address.is_empty() => DeviceScope::Mac(mac_address),
        _ => DeviceScope::None,
    }
}

/// `AuthService.logout(jwt, userId, sid, macAddress)`.
pub async fn logout(
    state: &AppState,
    jwt: &str,
    user_id: i32,
    scope: DeviceScope<'_>,
) -> ApiResult<Value> {
    // Satu transaksi: token ditolak dan device diputus bersama-sama.
    let mut tx = state.db.begin().await?;
    blacklist_token(&mut tx, jwt, user_id).await?;
    match scope {
        DeviceScope::Sid(sid) => {
            sqlx::query(
                r#"UPDATE "userConnected" SET status = 0 WHERE "userId" = $1 AND sid = $2::uuid"#,
            )
            .bind(user_id)
            .bind(sid)
            .execute(&mut *tx)
            .await?;
        }
        DeviceScope::Mac(mac) => {
            sqlx::query(
                r#"UPDATE "userConnected" SET status = 0 WHERE "userId" = $1 AND lower("macAddress") = lower($2)"#,
            )
            .bind(user_id)
            .bind(mac)
            .execute(&mut *tx)
            .await?;
        }
        DeviceScope::None => {}
    }
    tx.commit().await?;
    // Sesudah commit: invalidasi lebih awal bisa ditimpa load yang belum melihat blacklist.
    state.invalidate_token_revocation(user_id);
    Ok(json!({ "success": true, "message": "logout success" }))
}

/// `AuthService.createWebSession(userId, sid, currentToken)`.
pub async fn create_web_session(
    state: &AppState,
    user_id: i32,
    sid: Option<&Value>,
    current_token: &str,
) -> ApiResult<Value> {
    let Some(sid) = sid else {
        return Ok(json!({
            "success": true,
            "message": "web session ready",
            "data": { "accessToken": current_token },
        }));
    };
    let session = match sid.as_str() {
        Some(s) => sqlx::query_scalar::<_, i32>(
            r#"SELECT id FROM "userConnected" WHERE "userId" = $1 AND sid = $2::uuid AND status = 1 LIMIT 1"#,
        )
        .bind(user_id)
        .bind(s)
        .fetch_optional(&state.db)
        .await?,
        None => None,
    };
    if session.is_none() {
        return Err(ApiError::status_false(
            StatusCode::UNAUTHORIZED,
            "This device is not recognized or has been logged out. Please login again.",
        ));
    }
    Ok(json!({
        "success": true,
        "message": "web session ready",
        "data": { "accessToken": sign_user_token(state, user_id, None)? },
    }))
}

/// Kunci cache `GET /auth/me` (cache-manager, tanpa prefix).
pub fn me_cache_key(user_id: i32) -> String {
    format!("user-me-{user_id}")
}

const ME_CACHE_TTL_MS: i64 = 300_000;

/// `AuthController.findMe` (setelah guard): kunci langganan dihitung di luar cache.
pub async fn find_me(state: &AppState, user_id: i32) -> ApiResult<Value> {
    let lock = mz_core::subscription_lock::get_state(state, user_id).await?;
    let lock = serde_json::to_value(lock).map_err(ApiError::internal)?;
    let key = me_cache_key(user_id);
    let cached: Option<String> = state.redis.next().get(&key).await?;
    let data = match cached {
        Some(raw) => serde_json::from_str::<Value>(&raw).map_err(ApiError::internal)?,
        None => {
            let Some(data) = mz_account::find_one::find_one(state, user_id).await? else {
                return Ok(json!({ "success": false, "message": "user not found", "data": null }));
            };
            let raw = serde_json::to_string(&data).map_err(ApiError::internal)?;
            state
                .redis
                .next()
                .set::<(), _, _>(
                    &key,
                    raw,
                    Some(Expiration::PX(ME_CACHE_TTL_MS)),
                    None,
                    false,
                )
                .await?;
            data
        }
    };
    let data = match data {
        Value::Object(mut map) => {
            map.insert("subscriptionLock".into(), lock);
            Value::Object(map)
        }
        // `{ ...value }` dari nilai non-objek → hanya `subscriptionLock`.
        _ => json!({ "subscriptionLock": lock }),
    };
    Ok(json!({ "success": true, "message": "success get data", "data": data }))
}

/// `cacheManager.del('user-me-<id>')`.
pub async fn invalidate_me_cache(state: &AppState, user_id: i32) -> ApiResult<()> {
    state
        .redis
        .next()
        .del::<i64, _>(me_cache_key(user_id))
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logout_scope_prefers_sid() {
        let sid = json!("7fe52a4f-86fe-4e4d-84c7-be42d94a3868");
        assert_eq!(
            device_scope(Some(&sid), "aa:bb"),
            DeviceScope::Sid("7fe52a4f-86fe-4e4d-84c7-be42d94a3868")
        );
        assert_eq!(device_scope(None, "aa:bb"), DeviceScope::Mac("aa:bb"));
        // `sid` non-string jatuh ke MAC, seperti `typeof sid === 'string'` di Node.
        assert_eq!(device_scope(Some(&json!(5)), ""), DeviceScope::None);
        assert_eq!(device_scope(Some(&json!("")), ""), DeviceScope::None);
    }

    #[test]
    fn me_key_matches_node() {
        assert_eq!(me_cache_key(42), "user-me-42");
    }

    #[tokio::test]
    async fn token_payload_shape() {
        let Ok(state) = AppState::for_test() else {
            return;
        };
        let token = sign_user_token(&state, 7, Some(json!("s-1"))).unwrap_or_default();
        let claims = mz_core::jwt::decode_claims("test-secret", &token).unwrap_or_default();
        assert_eq!(claims.get("userId"), Some(&json!(7)));
        assert_eq!(claims.get("sid"), Some(&json!("s-1")));
        assert!(claims.get("iat").is_some());
        let web = sign_user_token(&state, 7, None).unwrap_or_default();
        let claims = mz_core::jwt::decode_claims("test-secret", &web).unwrap_or_default();
        assert!(claims.get("sid").is_none());
    }
}
