//! Port `src/mcp-api/auth/*`: `PatAuthGuard`, `PatService`, `ReadOnlyInterceptor`,
//! `AuditLogInterceptor`, `@PatUser()`.
//!
//! Satu middleware per route ([`pat`]) menjalankan rantai Nest dengan urutan sama:
//! guard (token Bearer `mcp_pat_*` atau `mcp_oauth_*`) -> `ReadOnlyInterceptor` -> handler ->
//! `AuditLogInterceptor` (tulis log pemakaian di latar, galat diabaikan).
//!
//! Token disimpan sebagai SHA-256 hex (`tokenHash`); token tak pernah disimpan plain. Token mati
//! bila `status != 1`, sudah dicabut, kedaluwarsa (`expiresAt < now`; `NULL` = tak pernah), atau
//! pemiliknya bukan user aktif (audit C5).

use std::time::Instant;

use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, header};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::routing::MethodRouter;
use chrono::NaiveDateTime;
use mz_core::time::now_naive;
use mz_core::{ApiError, AppState};
use sha2::{Digest, Sha256};

pub const PAT_PREFIX: &str = "mcp_pat_";
pub const OAUTH_PREFIX: &str = "mcp_oauth_";

/// `createHash('sha256').update(plain).digest('hex')`.
pub fn hash_token(plain: &str) -> String {
    hex::encode(Sha256::digest(plain.as_bytes()))
}

/// `req.patUser` (hanya bagian yang dipakai handler).
#[derive(Debug, Clone)]
pub struct PatUser {
    pub user_id: i32,
}

/// `req.patTokenRecord` (untuk log audit).
#[derive(Debug, Clone)]
enum TokenRecord {
    Pat {
        id: i32,
        user_id: i32,
    },
    OAuth {
        id: i32,
        user_id: i32,
        client_id: String,
        client_name: String,
    },
}

impl TokenRecord {
    fn user_id(&self) -> i32 {
        match self {
            Self::Pat { user_id, .. } | Self::OAuth { user_id, .. } => *user_id,
        }
    }
}

/// Aturan `findByToken` / `findByOAuthToken`: `status === 1`, belum dicabut, belum kedaluwarsa,
/// pemilik user aktif (`users.status === 1`).
pub fn token_usable(
    status: i32,
    revoked_at: Option<NaiveDateTime>,
    expires_at: Option<NaiveDateTime>,
    user_status: i32,
    now: NaiveDateTime,
) -> bool {
    if status != 1 || revoked_at.is_some() {
        return false;
    }
    if expires_at.is_some_and(|e| e < now) {
        return false;
    }
    user_status == mz_core::user_status::ACTIVE
}

type PatRow = (
    i32,
    i32,
    i32,
    Option<NaiveDateTime>,
    Option<NaiveDateTime>,
    i32,
);

/// Node: src/mcp-api/auth/pat.service.ts:21 (`findByToken`).
async fn find_pat(state: &AppState, plain: &str) -> Result<Option<TokenRecord>, ApiError> {
    if !plain.starts_with(PAT_PREFIX) {
        return Ok(None);
    }
    let row: Option<PatRow> = sqlx::query_as(
        r#"SELECT t.id, t."userId", t.status, t."revokedAt", t."expiresAt", u.status
             FROM "mcpAccessToken" t
             JOIN users u ON u.id = t."userId"
            WHERE t."tokenHash" = $1"#,
    )
    .bind(hash_token(plain))
    .fetch_optional(&state.db)
    .await?;
    let now = now_naive();
    Ok(
        row.and_then(|(id, user_id, status, revoked, expires, user_status)| {
            token_usable(status, revoked, expires, user_status, now)
                .then_some(TokenRecord::Pat { id, user_id })
        }),
    )
}

type OAuthRow = (
    i32,
    i32,
    String,
    String,
    i32,
    Option<NaiveDateTime>,
    Option<NaiveDateTime>,
    i32,
);

/// Node: src/mcp-api/oauth/oauth.service.ts:213 (`findByOAuthToken`).
async fn find_oauth(state: &AppState, plain: &str) -> Result<Option<TokenRecord>, ApiError> {
    if !plain.starts_with(OAUTH_PREFIX) {
        return Ok(None);
    }
    let row: Option<OAuthRow> = sqlx::query_as(
        r#"SELECT t.id, t."userId", t."clientId"::text, c."clientName", t.status, t."revokedAt",
                  t."expiresAt", u.status
             FROM "mcpOAuthAccessToken" t
             JOIN users u ON u.id = t."userId"
             JOIN "mcpOAuthClient" c ON c.id = t."clientId"
            WHERE t."tokenHash" = $1"#,
    )
    .bind(hash_token(plain))
    .fetch_optional(&state.db)
    .await?;
    let now = now_naive();
    Ok(row.and_then(
        |(id, user_id, client_id, client_name, status, revoked, expires, user_status)| {
            token_usable(status, revoked, expires, user_status, now).then_some(TokenRecord::OAuth {
                id,
                user_id,
                client_id,
                client_name,
            })
        },
    ))
}

fn api_url(state: &AppState) -> String {
    state
        .config
        .var("API_URL")
        .unwrap_or_else(|| "http://localhost:3000".to_owned())
}

/// Node: src/mcp-api/auth/pat-auth.guard.ts:16 (`PatAuthGuard.canActivate`). `Err` = respons 401.
async fn authenticate(state: &AppState, headers: &HeaderMap) -> Result<TokenRecord, Box<Response>> {
    let auth = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    // `auth.split(' ')`: scheme = [0], token = [1] (spasi ganda -> token kosong).
    let mut it = auth.split(' ');
    let scheme = it.next().unwrap_or_default();
    let token = it.next().unwrap_or_default();

    if scheme != "Bearer" || token.is_empty() {
        let mut res = ApiError::unauthorized("Missing Bearer token").into_response();
        if let Ok(v) = HeaderValue::from_str(&format!(
            "Bearer resource_metadata=\"{}/.well-known/oauth-protected-resource\"",
            api_url(state)
        )) {
            res.headers_mut().insert(header::WWW_AUTHENTICATE, v);
        }
        return Err(Box::new(res));
    }
    let invalid = || Box::new(ApiError::unauthorized("Invalid or expired token").into_response());

    if token.starts_with(PAT_PREFIX) {
        let record = find_pat(state, token)
            .await
            .map_err(|e| Box::new(e.into_response()))?
            .ok_or_else(invalid)?;
        if let TokenRecord::Pat { id, .. } = record {
            let db = state.db.clone();
            mz_core::bg::spawn(async move {
                let _ =
                    sqlx::query(r#"UPDATE "mcpAccessToken" SET "lastUsedAt" = $1 WHERE id = $2"#)
                        .bind(now_naive())
                        .bind(id)
                        .execute(&db)
                        .await;
            });
        }
        return Ok(record);
    }

    if token.starts_with(OAUTH_PREFIX) {
        let record = find_oauth(state, token)
            .await
            .map_err(|e| Box::new(e.into_response()))?
            .ok_or_else(invalid)?;
        if let TokenRecord::OAuth { id, .. } = record {
            let db = state.db.clone();
            mz_core::bg::spawn(async move {
                let _ = sqlx::query(
                    r#"UPDATE "mcpOAuthAccessToken" SET "lastUsedAt" = $1 WHERE id = $2"#,
                )
                .bind(now_naive())
                .bind(id)
                .execute(&db)
                .await;
            });
        }
        return Ok(record);
    }

    Err(Box::new(
        ApiError::unauthorized("Invalid token type").into_response(),
    ))
}

/// `AuditLogInterceptor.writeLog` (galat ditelan, di latar).
fn write_log(
    state: &AppState,
    record: TokenRecord,
    method: String,
    path: String,
    status: i32,
    duration_ms: i32,
) {
    let db = state.db.clone();
    mz_core::bg::spawn(async move {
        let _ = match record {
            TokenRecord::Pat { id, user_id } => {
                sqlx::query(
                    r#"INSERT INTO "mcpAccessTokenLog"
                         ("tokenId", "userId", method, path, "statusCode", "durationMs")
                       VALUES ($1, $2, $3, $4, $5, $6)"#,
                )
                .bind(id)
                .bind(user_id)
                .bind(method)
                .bind(path)
                .bind(status)
                .bind(duration_ms)
                .execute(&db)
                .await
            }
            TokenRecord::OAuth {
                id,
                user_id,
                client_id,
                client_name,
            } => {
                sqlx::query(
                    r#"INSERT INTO "mcpOAuthAccessTokenLog"
                         ("oauthTokenId", "userId", "clientId", "clientName", method, path,
                          "statusCode", "durationMs")
                       VALUES ($1, $2, $3::uuid, $4, $5, $6, $7, $8)"#,
                )
                .bind(id)
                .bind(user_id)
                .bind(client_id)
                .bind(client_name)
                .bind(method)
                .bind(path)
                .bind(status)
                .bind(duration_ms)
                .execute(&db)
                .await
            }
        };
    });
}

async fn pat_gate(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    let record = match authenticate(&state, req.headers()).await {
        Ok(r) => r,
        Err(res) => return *res,
    };
    // `ReadOnlyInterceptor` (sebelum `AuditLogInterceptor`: penolakan tidak dicatat).
    if !matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS) {
        return ApiError::forbidden("mcp-api endpoint is read-only").into_response();
    }
    let started = Instant::now();
    let method = req.method().as_str().to_owned();
    let path = req.uri().path().to_owned();
    req.extensions_mut().insert(PatUser {
        user_id: record.user_id(),
    });
    let res = next.run(req).await;
    let status = i32::from(res.status().as_u16());
    let duration_ms = i32::try_from(started.elapsed().as_millis()).unwrap_or(i32::MAX);
    write_log(&state, record, method, path, status, duration_ms);
    res
}

/// Pasang `PatAuthGuard` + `ReadOnlyInterceptor` + `AuditLogInterceptor` ke satu route.
pub fn pat(state: &AppState, route: MethodRouter<AppState>) -> MethodRouter<AppState> {
    route.layer(from_fn_with_state(state.clone(), pat_gate))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[test]
    fn hash_is_sha256_hex() {
        assert_eq!(
            hash_token("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn usable_rules() {
        let n = now_naive();
        assert!(token_usable(1, None, None, 1, n));
        // user bukan aktif (dinonaktifkan/dihapus) -> token mati (audit C5)
        assert!(!token_usable(1, None, None, 3, n));
        assert!(!token_usable(1, None, None, 0, n));
        // dicabut / status != 1
        assert!(!token_usable(1, Some(n), None, 1, n));
        assert!(!token_usable(0, None, None, 1, n));
        // kedaluwarsa
        assert!(!token_usable(1, None, Some(n - Duration::seconds(1)), 1, n));
        assert!(token_usable(1, None, Some(n + Duration::seconds(60)), 1, n));
    }
}
