//! Port `src/admin-mcp/auth/admin-pat.service.ts` + `admin-pat-auth.guard.ts`.
//!
//! Token disimpan sebagai SHA-256 hex (`tokenHash`). Dua jenis token Bearer:
//! `mcp_admin_pat_*` (PAT operator) dan `mcp_admin_oauth_*` (access token OAuth).

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use chrono::NaiveDateTime;
use mz_core::time::now_naive;
use mz_core::{ApiError, AppState};
use sha2::{Digest, Sha256};

pub const PAT_PREFIX: &str = "mcp_admin_pat_";
pub const OAUTH_PREFIX: &str = "mcp_admin_oauth_";

/// `createHash('sha256').update(plain).digest('hex')`.
pub fn hash_token(plain: &str) -> String {
    hex::encode(Sha256::digest(plain.as_bytes()))
}

/// `req.adminPatUser` (hanya bagian yang dibutuhkan handler).
#[derive(Debug, Clone)]
pub struct AdminPatUser {
    pub operator_id: i32,
    pub token_uuid: String,
}

/// Aturan `findByPat` / `findByOAuthToken`: `status === 1`, belum dicabut, operator aktif
/// (D-1), belum kedaluwarsa (`expiresAt < now` ditolak; `null` = tak pernah kedaluwarsa).
pub fn token_usable(
    status: i32,
    revoked_at: Option<NaiveDateTime>,
    expires_at: Option<NaiveDateTime>,
    operator_status: i32,
    now: NaiveDateTime,
) -> bool {
    if status != 1 || revoked_at.is_some() {
        return false;
    }
    if operator_status != 1 {
        return false;
    }
    !expires_at.is_some_and(|e| e < now)
}

type Row = (
    i32,
    String,
    i32,
    i32,
    Option<NaiveDateTime>,
    Option<NaiveDateTime>,
    i32,
);

async fn find_pat(state: &AppState, plain: &str) -> Result<Option<(i32, AdminPatUser)>, ApiError> {
    if !plain.starts_with(PAT_PREFIX) {
        return Ok(None);
    }
    let row: Option<Row> = sqlx::query_as(
        r#"SELECT t.id, t.uuid::text, t."operatorId", t.status, t."revokedAt", t."expiresAt", o.status
             FROM "mcpAdminAccessToken" t
             JOIN operators o ON o.id = t."operatorId"
            WHERE t."tokenHash" = $1"#,
    )
    .bind(hash_token(plain))
    .fetch_optional(&state.db)
    .await?;
    Ok(row.and_then(
        |(id, uuid, operator_id, status, revoked, expires, op_status)| {
            token_usable(status, revoked, expires, op_status, now_naive()).then_some((
                id,
                AdminPatUser {
                    operator_id,
                    token_uuid: uuid,
                },
            ))
        },
    ))
}

async fn find_oauth(
    state: &AppState,
    plain: &str,
) -> Result<Option<(i32, AdminPatUser)>, ApiError> {
    if !plain.starts_with(OAUTH_PREFIX) {
        return Ok(None);
    }
    let row: Option<Row> = sqlx::query_as(
        r#"SELECT t.id, t.uuid::text, t."operatorId", t.status, t."revokedAt", t."expiresAt", o.status
             FROM "mcpAdminOAuthAccessToken" t
             JOIN operators o ON o.id = t."operatorId"
             JOIN "mcpOAuthClient" c ON c.id = t."clientId"
            WHERE t."tokenHash" = $1"#,
    )
    .bind(hash_token(plain))
    .fetch_optional(&state.db)
    .await?;
    Ok(row.and_then(
        |(id, uuid, operator_id, status, revoked, expires, op_status)| {
            token_usable(status, revoked, expires, op_status, now_naive()).then_some((
                id,
                AdminPatUser {
                    operator_id,
                    token_uuid: uuid,
                },
            ))
        },
    ))
}

/// Penolakan guard: 401 Nest, dengan header `WWW-Authenticate` bila token tidak ada.
pub struct PatRejection(Response);

impl IntoResponse for PatRejection {
    fn into_response(self) -> Response {
        self.0
    }
}

impl From<ApiError> for PatRejection {
    fn from(e: ApiError) -> Self {
        Self(e.into_response())
    }
}

/// `AdminPatAuthGuard`.
#[derive(Debug, Clone)]
pub struct AdminPat(pub AdminPatUser);

impl FromRequestParts<AppState> for AdminPat {
    type Rejection = PatRejection;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let auth = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        // `auth.split(' ')`: scheme = [0], token = [1] (spasi ganda → token kosong).
        let mut it = auth.split(' ');
        let scheme = it.next().unwrap_or_default();
        let token = it.next().unwrap_or_default();

        if scheme != "Bearer" || token.is_empty() {
            let api_url = state
                .config
                .var("API_URL")
                .unwrap_or_else(|| "http://localhost:3000".to_owned());
            let mut res = ApiError::unauthorized("Missing Bearer token").into_response();
            if let Ok(v) = HeaderValue::from_str(&format!(
                "Bearer resource_metadata=\"{api_url}/admin-mcp/.well-known/oauth-protected-resource\""
            )) {
                res.headers_mut().insert(header::WWW_AUTHENTICATE, v);
            }
            return Err(PatRejection(res));
        }

        let invalid = || PatRejection::from(ApiError::unauthorized("Invalid or expired token"));

        if token.starts_with(PAT_PREFIX) {
            let (id, user) = find_pat(state, token).await?.ok_or_else(invalid)?;
            let db = state.db.clone();
            mz_core::bg::spawn(async move {
                let _ = sqlx::query(
                    r#"UPDATE "mcpAdminAccessToken" SET "lastUsedAt" = $1 WHERE id = $2"#,
                )
                .bind(now_naive())
                .bind(id)
                .execute(&db)
                .await;
            });
            return Ok(Self(user));
        }

        if token.starts_with(OAUTH_PREFIX) {
            let (id, user) = find_oauth(state, token).await?.ok_or_else(invalid)?;
            let db = state.db.clone();
            mz_core::bg::spawn(async move {
                let _ = sqlx::query(
                    r#"UPDATE "mcpAdminOAuthAccessToken" SET "lastUsedAt" = $1 WHERE id = $2"#,
                )
                .bind(now_naive())
                .bind(id)
                .execute(&db)
                .await;
            });
            return Ok(Self(user));
        }

        Err(PatRejection::from(ApiError::unauthorized(
            "Invalid token type",
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn now() -> NaiveDateTime {
        now_naive()
    }

    #[test]
    fn hash_is_sha256_hex() {
        assert_eq!(
            hash_token("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn usable_rules_match_spec() {
        let n = now();
        // operator aktif diterima
        assert!(token_usable(1, None, None, 1, n));
        // operator nonaktif ditolak (D-1)
        assert!(!token_usable(1, None, None, 0, n));
        // dicabut tetap ditolak
        assert!(!token_usable(1, Some(n), None, 1, n));
        // status bukan 1
        assert!(!token_usable(0, None, None, 1, n));
        // kedaluwarsa
        assert!(!token_usable(1, None, Some(n - Duration::seconds(1)), 1, n));
        assert!(token_usable(1, None, Some(n + Duration::seconds(60)), 1, n));
    }
}
