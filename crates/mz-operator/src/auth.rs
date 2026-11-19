//! `JwtAdminAuthGuard` + `OperatorRolesGuard`.

use std::collections::HashSet;

use axum::extract::FromRequestParts;
use axum::http::StatusCode;
use axum::http::request::Parts;
use chrono::NaiveDateTime;
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use mz_audit::AuditHandle;
use mz_core::jwt::extract_bearer;
use mz_core::{ApiError, AppState};
use serde_json::{Map, Value, json};

/// Enum Prisma `OperatorRole` (`src/operator-auth/operator-role.ts`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperatorRole {
    Owner,
    Finance,
    Support,
}

impl OperatorRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "OWNER",
            Self::Finance => "FINANCE",
            Self::Support => "SUPPORT",
        }
    }
}

/// `req.user` sesudah `JwtAdminStrategy.validate()`: baris `operators` aktif (`status = 1`).
/// `password` sengaja tidak dimuat; handler yang butuh (login, ganti password) membaca sendiri.
#[derive(Debug, Clone)]
pub struct Operator {
    pub id: i32,
    pub uuid: uuid::Uuid,
    pub username: String,
    pub name: String,
    pub status: i32,
    /// Teks enum apa adanya (`OWNER` | `FINANCE` | `SUPPORT`).
    pub role: String,
    pub created_at: NaiveDateTime,
    pub updated_at: Option<NaiveDateTime>,
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

fn forbidden(code: &str, message: &str) -> ApiError {
    ApiError::Body {
        status: StatusCode::FORBIDDEN,
        body: json!({ "code": code, "message": message }),
    }
}

type OperatorRow = (
    i32,
    uuid::Uuid,
    String,
    String,
    i32,
    String,
    NaiveDateTime,
    Option<NaiveDateTime>,
);

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
        let row: Option<OperatorRow> = sqlx::query_as(
            r#"SELECT id, uuid, username, name, status, role::text, "createdAt", "updatedAt"
                 FROM operators WHERE id = $1"#,
        )
        .bind(sub)
        .fetch_optional(&state.db)
        .await?;
        // D-1: operator nonaktif ditolak di semua endpoint admin.
        let Some((id, uuid, username, name, status, role, created_at, updated_at)) = row else {
            return Err(unauthorized());
        };
        if status != 1 {
            return Err(unauthorized());
        }
        // `OperatorAuditInterceptor` (class-level di semua controller operator): store.operator.
        AuditHandle::from_parts(parts, state.db.clone()).set_operator(id, &name);
        Ok(Self {
            id,
            uuid,
            username,
            name,
            status,
            role,
            created_at,
            updated_at,
        })
    }
}

impl Operator {
    /// `OperatorRolesGuard` dengan `@OperatorRoles(...allowed)`. Dipanggil di awal handler
    /// (guard Nest jalan sebelum pipe validasi body/query, jadi panggil SEBELUM validasi).
    /// `allowed` kosong = endpoint tanpa metadata → selalu ditolak (fail closed).
    pub fn require(&self, allowed: &[OperatorRole]) -> Result<(), ApiError> {
        if allowed.is_empty() {
            return Err(forbidden(
                "OPERATOR_ROLE_FORBIDDEN",
                "Endpoint has no operator role configured",
            ));
        }
        if self.status != 1 {
            return Err(forbidden(
                "OPERATOR_INACTIVE",
                "Operator account is inactive",
            ));
        }
        if !allowed.iter().any(|r| r.as_str() == self.role) {
            return Err(forbidden(
                "OPERATOR_ROLE_FORBIDDEN",
                "Operator role is not allowed for this endpoint",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{EncodingKey, Header};

    fn op(role: &str, status: i32) -> Operator {
        Operator {
            id: 1,
            uuid: uuid::Uuid::nil(),
            username: "u".into(),
            name: "n".into(),
            status,
            role: role.into(),
            created_at: NaiveDateTime::default(),
            updated_at: None,
        }
    }

    fn code(r: Result<(), ApiError>) -> String {
        match r {
            Ok(()) => "ALLOWED".into(),
            Err(ApiError::Body { body, .. }) => body
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            Err(_) => "OTHER".into(),
        }
    }

    #[test]
    fn roles_matrix() {
        use OperatorRole::*;
        let of = [Owner, Finance];
        assert_eq!(code(op("OWNER", 1).require(&of)), "ALLOWED");
        assert_eq!(code(op("FINANCE", 1).require(&of)), "ALLOWED");
        assert_eq!(
            code(op("SUPPORT", 1).require(&of)),
            "OPERATOR_ROLE_FORBIDDEN"
        );
        assert_eq!(code(op("OWNER", 0).require(&of)), "OPERATOR_INACTIVE");
        assert_eq!(code(op("OWNER", 1).require(&[])), "OPERATOR_ROLE_FORBIDDEN");
    }

    fn token(claims: &Value, secret: &str) -> String {
        jsonwebtoken::encode(
            &Header::new(Algorithm::HS256),
            claims,
            &EncodingKey::from_secret(secret.as_bytes()),
        )
        .unwrap_or_default()
    }

    #[test]
    fn accepts_token_without_exp_rejects_expired() {
        let t = token(&json!({ "sub": 5, "username": "op" }), "s");
        assert_eq!(
            decode_operator_claims("s", &t).and_then(|c| c.get("sub").and_then(Value::as_i64)),
            Some(5)
        );
        let expired = token(&json!({ "sub": 5, "exp": 1_000_000 }), "s");
        assert!(decode_operator_claims("s", &expired).is_none());
        assert!(decode_operator_claims("other", &t).is_none());
    }
}
