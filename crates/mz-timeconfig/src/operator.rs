//! `JwtAdminAuthGuard` (strategy `jwt-admin`, `src/jwt/jwt-admin.strategy.ts`) dan
//! `OperatorAuditInterceptor` (`src/operator-audit`) untuk route `admin/app-releases`.
//! Route ini `@PermissionExempt('separate-auth')` → dipasang sebagai `RouteSpec::public()`
//! (tanpa JwtAuthGuard/gerbang user) dan memakai extractor [`Operator`] di handler.

use std::collections::HashSet;
use std::net::SocketAddr;

use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::StatusCode;
use axum::http::request::Parts;
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use mz_core::jwt::extract_bearer;
use mz_core::rate_limit::client_ip;
use mz_core::{ApiError, AppState};
use serde_json::{Map, Value};

const MAX_IP: usize = 45;
const MAX_USER_AGENT: usize = 255;
const MAX_REASON: usize = 500;
const MAX_DEPTH: usize = 8;

/// Operator hasil `validate()`: baris `operators` aktif (`status = 1`).
#[derive(Debug, Clone)]
pub struct Operator {
    pub id: i32,
    pub name: String,
}

/// Verifikasi token operator seperti passport-jwt: HS256 + `SECRET_TOKEN`, `exp`
/// DIPERIKSA bila ada (berbeda dari `JwtStrategy` user yang mengabaikan kedaluwarsa).
pub fn decode_operator_claims(secret: &str, token: &str) -> Option<Map<String, Value>> {
    let mut v = Validation::new(Algorithm::HS256);
    v.validate_exp = true;
    v.validate_nbf = true;
    v.required_spec_claims = HashSet::new();
    v.validate_aud = false;
    jsonwebtoken::decode::<Map<String, Value>>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &v,
    )
    .ok()
    .map(|d| d.claims)
}

fn unauthorized() -> ApiError {
    ApiError::Bare(StatusCode::UNAUTHORIZED)
}

impl FromRequestParts<AppState> for Operator {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let token = extract_bearer(&parts.headers).ok_or_else(unauthorized)?;
        let claims =
            decode_operator_claims(&state.config.jwt_secret, token).ok_or_else(unauthorized)?;
        // Prisma `findUnique({ id: payload.sub })` melempar bila `sub` bukan integer
        // (500 di Node); di sini 401.
        let sub = claims
            .get("sub")
            .and_then(Value::as_i64)
            .and_then(|n| i32::try_from(n).ok())
            .ok_or_else(unauthorized)?;
        let row: Option<(i32, String, i32)> =
            sqlx::query_as("SELECT id, name, status FROM operators WHERE id = $1")
                .bind(sub)
                .fetch_optional(&state.db)
                .await?;
        // D-1: operator nonaktif ditolak di semua endpoint admin.
        match row {
            Some((id, name, 1)) => Ok(Self { id, name }),
            _ => Err(unauthorized()),
        }
    }
}

fn is_secret_key(key: &str) -> bool {
    let lower = key.to_lowercase();
    matches!(lower.as_str(), "password" | "token" | "tokensvalidafter")
        || (lower.starts_with("paypal") && lower.ends_with("secret"))
}

/// `stripSecrets`: salinan tanpa kunci rahasia, rekursif (maks. kedalaman 8).
pub fn strip_secrets(value: &Value, depth: usize) -> Value {
    match value {
        Value::Object(m) => {
            if depth >= MAX_DEPTH {
                return Value::String("[truncated]".into());
            }
            Value::Object(
                m.iter()
                    .filter(|(k, _)| !is_secret_key(k))
                    .map(|(k, v)| (k.clone(), strip_secrets(v, depth + 1)))
                    .collect(),
            )
        }
        Value::Array(a) => {
            if depth >= MAX_DEPTH {
                return Value::String("[truncated]".into());
            }
            Value::Array(a.iter().map(|v| strip_secrets(v, depth + 1)).collect())
        }
        other => other.clone(),
    }
}

fn truncate(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// Konteks request untuk baris audit (`req.ip`, `user-agent`).
#[derive(Debug, Clone, Default)]
pub struct AuditCtx {
    pub ip: Option<String>,
    pub user_agent: Option<String>,
}

impl FromRequestParts<AppState> for AuditCtx {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        _state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let peer = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|c| c.0);
        Ok(Self {
            ip: client_ip(&parts.headers, peer),
            user_agent: parts
                .headers
                .get(axum::http::header::USER_AGENT)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned),
        })
    }
}

/// Kolom `after` = `req.body && typeof req.body === 'object' ? stripSecrets(req.body) : null`
/// (`operator-audit.interceptor.ts:92`). Body-parser Express mengisi `req.body = {}` untuk
/// request TANPA body (mis. `DELETE`), jadi `None` di sini = `{}`, bukan NULL. Array juga
/// `typeof 'object'` di JS → `stripSecrets` memetakan elemennya.
fn audit_after(body: Option<&Value>) -> Option<Value> {
    match body {
        None => Some(Value::Object(serde_json::Map::new())),
        Some(b) if b.is_object() || b.is_array() => Some(strip_secrets(b, 0)),
        Some(_) => None,
    }
}

/// Satu baris `operatorAuditLog` untuk `admin/app-releases` (`targetType: 'appRelease'`).
pub async fn record_action(
    state: &AppState,
    operator: &Operator,
    audit: &AuditCtx,
    action: &str,
    target_uuid: Option<&str>,
    body: Option<&Value>,
) {
    record_action_for(
        state,
        operator,
        audit,
        "appRelease",
        action,
        target_uuid,
        body,
    )
    .await;
}

/// Satu baris `operatorAuditLog` (`recordOperatorAction` via `OperatorAuditInterceptor`),
/// SETELAH handler sukses, dengan `targetType` dari `@OperatorAuditAction`. `companyId`
/// selalu NULL (belum ada route Rust ber-`companyFrom`). Kegagalan hanya dicatat; respons
/// tidak digagalkan.
pub async fn record_action_for(
    state: &AppState,
    operator: &Operator,
    audit: &AuditCtx,
    target_type: &str,
    action: &str,
    target_uuid: Option<&str>,
    body: Option<&Value>,
) {
    let after = audit_after(body);
    let reason = after
        .as_ref()
        .and_then(|a| a.get("reason"))
        .and_then(Value::as_str)
        .map(|r| truncate(r, MAX_REASON));
    // Kolom `@db.Uuid`: nilai yang ditolak Prisma ditulis NULL (pencatatan tidak digagalkan).
    let target = target_uuid.and_then(|u| mz_core::prisma::parse_uuid(u).ok());
    let res = sqlx::query(
        r#"INSERT INTO "operatorAuditLog"
             ("operatorId", action, "targetType", "targetUuid", "companyId", before, after, reason,
              "ipAddress", "userAgent")
           VALUES ($1, $2, $8, $3::uuid, NULL, NULL, $4, $5, $6, $7)"#,
    )
    .bind(operator.id)
    .bind(action)
    .bind(target)
    .bind(after)
    .bind(reason)
    .bind(audit.ip.as_deref().map(|s| truncate(s, MAX_IP)))
    .bind(
        audit
            .user_agent
            .as_deref()
            .map(|s| truncate(s, MAX_USER_AGENT)),
    )
    .bind(target_type)
    .execute(&state.db)
    .await;
    if let Err(err) = res {
        tracing::error!(action, error = %err, "Gagal menulis operatorAuditLog");
    }
}

#[cfg(test)]
mod audit_after_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn missing_body_is_empty_object_like_express() {
        assert_eq!(audit_after(None), Some(json!({})));
        assert_eq!(audit_after(Some(&json!("teks"))), None);
        assert_eq!(
            audit_after(Some(&json!([{ "password": "x", "a": 1 }]))),
            Some(json!([{ "a": 1 }]))
        );
        assert_eq!(
            audit_after(Some(&json!({ "isActive": true }))),
            Some(json!({ "isActive": true }))
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{EncodingKey, Header};
    use serde_json::json;

    fn token(claims: &Value, secret: &str) -> String {
        jsonwebtoken::encode(
            &Header::new(Algorithm::HS256),
            claims,
            &EncodingKey::from_secret(secret.as_bytes()),
        )
        .unwrap_or_default()
    }

    #[test]
    fn accepts_token_without_exp() {
        let t = token(&json!({ "sub": 5, "username": "op" }), "s");
        let c = decode_operator_claims("s", &t);
        assert_eq!(
            c.and_then(|c| c.get("sub").and_then(Value::as_i64)),
            Some(5)
        );
    }

    #[test]
    fn rejects_expired_and_wrong_secret() {
        let expired = token(&json!({ "sub": 5, "exp": 1_000_000 }), "s");
        assert!(decode_operator_claims("s", &expired).is_none());
        let t = token(&json!({ "sub": 5 }), "s");
        assert!(decode_operator_claims("other", &t).is_none());
        assert!(decode_operator_claims("s", "garbage").is_none());
    }

    #[test]
    fn strips_secrets_recursively() {
        let v = json!({ "a": 1, "Password": "x", "n": { "token": "t", "paypalLiveSecret": "s", "ok": [ { "password": 1, "k": 2 } ] } });
        assert_eq!(
            strip_secrets(&v, 0),
            json!({ "a": 1, "n": { "ok": [ { "k": 2 } ] } })
        );
    }
}
