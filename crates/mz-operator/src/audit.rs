//! `operatorAuditLog`: `recordOperatorAction` + `OperatorAuditInterceptor`.

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::request::Parts;
use mz_core::rate_limit::client_ip;
use mz_core::{ApiError, AppState};
use serde_json::{Map, Value};

use crate::Operator;

const MAX_IP: usize = 45;
const MAX_USER_AGENT: usize = 255;
const MAX_REASON: usize = 500;
const MAX_DEPTH: usize = 8;

fn is_secret_key(key: &str) -> bool {
    let lower = key.to_lowercase();
    matches!(lower.as_str(), "password" | "token" | "tokensvalidafter")
        || (lower.starts_with("paypal") && lower.ends_with("secret"))
}

/// `stripSecrets`: salinan tanpa kunci rahasia (`password`, `token`, `tokensValidAfter`,
/// `paypal*Secret`), rekursif lewat objek & array, maks. kedalaman 8 (`"[truncated]"`).
/// Panggil dengan `depth = 0`.
///
/// Meniru juga hasil JS (paritas adm-misc): angka dari `JSON.parse` tidak punya bentuk
/// `1000.0` (bilangan bulat ditulis `1000`), dan kunci `__proto__` tidak ikut tersimpan.
pub fn strip_secrets(value: &Value, depth: usize) -> Value {
    match value {
        Value::Object(m) => {
            if depth >= MAX_DEPTH {
                return Value::String("[truncated]".into());
            }
            Value::Object(
                m.iter()
                    .filter(|(k, _)| !is_secret_key(k) && k.as_str() != "__proto__")
                    .map(|(k, v)| (k.clone(), strip_secrets(v, depth + 1)))
                    .collect(),
            )
        }
        Value::Number(n) if n.is_f64() => n.as_f64().map_or(Value::Null, mz_core::jsfmt::num),
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

/// Konteks request untuk baris audit (`store.ipAddress`, `store.userAgent`).
/// `AuditCtx::default()` = di luar request (worker/cron): keduanya NULL.
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

/// Argumen `recordOperatorAction`. `before`/`after` di-`stripSecrets` saat ditulis;
/// `None` = kolom NULL.
#[derive(Debug, Clone, Default)]
pub struct OperatorAuditEntry<'a> {
    pub operator_id: i32,
    pub action: &'a str,
    pub target_type: &'a str,
    pub target_uuid: Option<uuid::Uuid>,
    pub company_id: Option<i32>,
    pub before: Option<&'a Value>,
    pub after: Option<&'a Value>,
    pub reason: Option<&'a str>,
}

/// `recordOperatorAction(tx, entry)`: satu baris `operatorAuditLog` di dalam transaksi
/// pemanggil (perubahan & jejaknya commit/rollback bersama). Error DIKEMBALIKAN (di Node
/// melempar → transaksi rollback).
pub async fn record<'e, E>(exec: E, ctx: &AuditCtx, e: &OperatorAuditEntry<'_>) -> sqlx::Result<()>
where
    E: sqlx::PgExecutor<'e>,
{
    sqlx::query(
        r#"INSERT INTO "operatorAuditLog"
             ("operatorId", action, "targetType", "targetUuid", "companyId", before, after, reason,
              "ipAddress", "userAgent")
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)"#,
    )
    .bind(e.operator_id)
    .bind(e.action)
    .bind(e.target_type)
    .bind(e.target_uuid)
    .bind(e.company_id)
    .bind(e.before.map(|v| strip_secrets(v, 0)))
    .bind(e.after.map(|v| strip_secrets(v, 0)))
    .bind(e.reason)
    .bind(ctx.ip.as_deref().map(|s| truncate(s, MAX_IP)))
    .bind(
        ctx.user_agent
            .as_deref()
            .map(|s| truncate(s, MAX_USER_AGENT)),
    )
    .execute(exec)
    .await?;
    Ok(())
}

/// `@OperatorAuditAction(action, { targetType, targetParam?, companyFrom? })`.
#[derive(Debug, Clone, Copy)]
pub struct AuditMeta {
    pub action: &'static str,
    pub target_type: &'static str,
    /// `companyFrom`.
    pub company_from: Option<CompanyFrom>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompanyFrom {
    /// UUID di `targetParam` adalah UUID company (id dicari lewat `company.uuid`).
    Param,
    /// `body.companyId` (integer > 0).
    Body,
}

/// Body Express: request tanpa body → `req.body = {}` (bukan NULL); non-objek → NULL.
fn interceptor_body(body: Option<&Value>) -> Option<Value> {
    match body {
        None => Some(Value::Object(Map::new())),
        Some(b) if b.is_object() || b.is_array() => Some(strip_secrets(b, 0)),
        Some(_) => None,
    }
}

/// `Number.isInteger(id) && id > 0`.
fn js_positive_int(v: Option<&Value>) -> Option<i32> {
    let n = v?.as_f64()?;
    if n.fract() != 0.0 || n <= 0.0 || n > f64::from(i32::MAX) {
        return None;
    }
    // aman: sudah dicek bulat & dalam rentang i32
    #[allow(clippy::cast_possible_truncation)]
    Some(n as i32)
}

/// `OperatorAuditInterceptor.write`: panggil SESUDAH handler sukses (Node: `tap.next`,
/// tidak ada baris bila handler melempar, dan dilewati bila respons objek ber-`success: false`
/// — cek itu di pemanggil). `raw_target` = `req.params[targetParam]` (nilai mentah; dipakai
/// hanya bila berbentuk UUID). `body` = body request mentah (`None` = tanpa body).
/// Kegagalan hanya dicatat di log; respons tidak digagalkan.
pub async fn audit_after_success(
    state: &AppState,
    operator: &Operator,
    ctx: &AuditCtx,
    meta: AuditMeta,
    raw_target: Option<&str>,
    body: Option<&Value>,
) {
    let after = interceptor_body(body);
    // `UUID_RE` Node: hanya bentuk 8-4-4-4-12 ber-hyphen (uuid crate juga menerima bentuk lain).
    let target_uuid = raw_target
        .filter(|t| t.len() == 36)
        .and_then(|t| uuid::Uuid::try_parse(t).ok());
    let reason = after
        .as_ref()
        .and_then(|a| a.get("reason"))
        .and_then(Value::as_str)
        .map(|r| truncate(r, MAX_REASON));
    let company_id = match meta.company_from {
        Some(CompanyFrom::Param) => match target_uuid {
            Some(u) => {
                match sqlx::query_scalar::<_, i32>("SELECT id FROM company WHERE uuid = $1")
                    .bind(u)
                    .fetch_optional(&state.db)
                    .await
                {
                    Ok(id) => id,
                    Err(err) => {
                        tracing::error!(action = meta.action, error = %err, "Gagal menulis operatorAuditLog");
                        return;
                    }
                }
            }
            None => None,
        },
        Some(CompanyFrom::Body) => js_positive_int(body.and_then(|b| b.get("companyId"))),
        None => None,
    };
    let entry = OperatorAuditEntry {
        operator_id: operator.id,
        action: meta.action,
        target_type: meta.target_type,
        target_uuid,
        company_id,
        before: None,
        // sudah di-strip; strip ulang idempoten
        after: after.as_ref(),
        reason: reason.as_deref(),
    };
    if let Err(err) = record(&state.db, ctx, &entry).await {
        tracing::error!(action = meta.action, error = %err, "Gagal menulis operatorAuditLog");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn strips_secrets_recursively() {
        let v = json!({ "a": 1, "Password": "x", "n": { "token": "t", "paypalLiveSecret": "s", "ok": [ { "password": 1, "k": 2 } ] } });
        assert_eq!(
            strip_secrets(&v, 0),
            json!({ "a": 1, "n": { "ok": [ { "k": 2 } ] } })
        );
    }

    #[test]
    fn normalizes_numbers_and_drops_proto_like_js() {
        let v: Value = serde_json::from_str(
            r#"{ "basePrice": 1000.0, "e": 1e2, "f": 1.5, "__proto__": { "x": 1 }, "n": [2.0] }"#,
        )
        .unwrap_or_default();
        assert_eq!(
            strip_secrets(&v, 0).to_string(),
            r#"{"basePrice":1000,"e":100,"f":1.5,"n":[2]}"#
        );
    }

    #[test]
    fn missing_body_is_empty_object_like_express() {
        assert_eq!(interceptor_body(None), Some(json!({})));
        assert_eq!(interceptor_body(Some(&json!("teks"))), None);
        assert_eq!(
            interceptor_body(Some(&json!([{ "password": "x", "a": 1 }]))),
            Some(json!([{ "a": 1 }]))
        );
    }

    #[test]
    fn company_id_from_body_is_js_positive_integer() {
        assert_eq!(js_positive_int(Some(&json!(5))), Some(5));
        assert_eq!(js_positive_int(Some(&json!(5.0))), Some(5));
        assert_eq!(js_positive_int(Some(&json!(5.5))), None);
        assert_eq!(js_positive_int(Some(&json!(0))), None);
        assert_eq!(js_positive_int(Some(&json!("5"))), None);
        assert_eq!(js_positive_int(None), None);
    }
}
