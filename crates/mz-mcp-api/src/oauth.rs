//! Port `src/mcp-api/oauth/*`: server OAuth 2.1 untuk klien MCP USER (registrasi klien dinamis,
//! authorize + PKCE S256, token, refresh rotasi, revoke) + dokumen `.well-known`.
//!
//! Keamanan (identik dengan Node): token/refresh disimpan sebagai SHA-256 hex; kode otorisasi
//! disimpan apa adanya (sama dengan Node), kedaluwarsa 10 menit, sekali pakai; access token 24
//! jam; refresh token 365 hari, dirotasi tiap dipakai; PKCE hanya S256 (bandingkan waktu
//! konstan); `redirect_uri` wajib terdaftar pada klien; `register` memakai handler
//! `mz_admin_mcp::oauth::h_register` (kode Node-nya identik: `validateRedirectUris`, tabel
//! `mcpOAuthClient` bersama).
//!
//! Lihat `plan/deviations/mz-mcp-api.md`.

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, Uri};
use axum::response::Response;
use chrono::{Duration, NaiveDateTime};
use mz_admin_mcp::body::{Body, V, query_map, str_of};
use mz_admin_mcp::oauth::{build_error_redirect_url, build_redirect_url, form_query, verify_pkce};
use mz_core::gate::RequestCtx;
use mz_core::redirect::redirect_302;
use mz_core::time::now_naive;
use mz_core::{ApiError, ApiResult, AppState};
use rand::TryRngCore;
use serde_json::{Map, Value, json};

use crate::pat::hash_token;

const AUTH_CODE_TTL: Duration = Duration::minutes(10);
const ACCESS_TOKEN_TTL: Duration = Duration::hours(24);
const REFRESH_TOKEN_TTL: Duration = Duration::days(365);
/// `ACCESS_TOKEN_TTL_MS / 1000`, `REFRESH_TOKEN_TTL_MS / 1000`.
const ACCESS_TOKEN_TTL_SECS: i64 = 24 * 60 * 60;
const REFRESH_TOKEN_TTL_SECS: i64 = 365 * 24 * 60 * 60;

fn api_url(state: &AppState) -> String {
    state
        .config
        .var("API_URL")
        .unwrap_or_else(|| "http://localhost:3000".to_owned())
}

fn random_hex(bytes: usize) -> ApiResult<String> {
    let mut buf = vec![0_u8; bytes];
    rand::rngs::OsRng
        .try_fill_bytes(&mut buf)
        .map_err(ApiError::internal)?;
    Ok(hex::encode(buf))
}

// ───── well-known ─────

/// Node: src/mcp-api/oauth/well-known.controller.ts:11 (`GET /.well-known/oauth-authorization-server`)
pub async fn h_metadata(State(state): State<AppState>) -> Json<Value> {
    let issuer = api_url(&state);
    Json(json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{issuer}/mcp-api/oauth/authorize"),
        "token_endpoint": format!("{issuer}/mcp-api/oauth/token"),
        "registration_endpoint": format!("{issuer}/mcp-api/oauth/register"),
        "revocation_endpoint": format!("{issuer}/mcp-api/oauth/revoke"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
        "scopes_supported": ["read"],
    }))
}

/// Node: src/mcp-api/oauth/well-known.controller.ts:16 (`GET /.well-known/oauth-protected-resource`)
pub async fn h_protected_resource(State(state): State<AppState>) -> Json<Value> {
    let issuer = api_url(&state);
    Json(json!({
        "resource": issuer,
        "authorization_servers": [issuer],
        "bearer_methods_supported": ["header"],
        "scopes_supported": ["read"],
    }))
}

// ───── authorize ─────

/// `AuthorizeQueryDto` / `ApproveAuthorizeDto`. `approve` menambahkan `approved` (properti kelas
/// turunan = divalidasi lebih dulu).
fn validate_authorize(m: &Map<String, Value>, approve: bool) -> ApiResult<()> {
    let mut v = V::default();
    if approve {
        v.boolean("approved", m.get("approved"));
    }
    v.is_in("response_type", m.get("response_type"), &["code"]);
    v.uuid("client_id", m.get("client_id"));
    v.not_empty_string("redirect_uri", m.get("redirect_uri"));
    v.not_empty_string("code_challenge", m.get("code_challenge"));
    v.opt_is_in(
        "code_challenge_method",
        m.get("code_challenge_method"),
        &["S256"],
    );
    v.opt_string("state", m.get("state"));
    v.opt_string("scope", m.get("scope"));
    v.finish()
}

/// Node: src/mcp-api/oauth/oauth.service.ts:51 (`validateAuthorizeParams`).
async fn validate_authorize_params(
    state: &AppState,
    client_id: &str,
    redirect_uri: &str,
    method: Option<&str>,
) -> ApiResult<()> {
    let row: Option<(Vec<String>,)> = sqlx::query_as(
        r#"SELECT "redirectUris" FROM "mcpOAuthClient" WHERE id = $1::uuid AND status = 1"#,
    )
    .bind(client_id)
    .fetch_optional(&state.db)
    .await?;
    let Some((uris,)) = row else {
        return Err(ApiError::bad_request("Unknown client_id"));
    };
    if !uris.iter().any(|u| u == redirect_uri) {
        return Err(ApiError::bad_request(
            "redirect_uri not registered for this client",
        ));
    }
    if method.is_some_and(|m| !m.is_empty() && m != "S256") {
        return Err(ApiError::bad_request(
            "Only S256 code_challenge_method is supported",
        ));
    }
    Ok(())
}

/// Node: src/mcp-api/oauth/oauth.controller.ts:41 (`GET authorize`, `@Redirect()` -> 302 ke
/// `${FRONTEND_URL}/mcp-consent?...`)
pub async fn h_authorize_get(
    State(state): State<AppState>,
    uri: Uri,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let q = query_map(uri.query());
    validate_authorize(&q, false)?;
    let client_id = str_of(&q, "client_id").unwrap_or_default();
    let redirect_uri = str_of(&q, "redirect_uri").unwrap_or_default();
    let code_challenge = str_of(&q, "code_challenge").unwrap_or_default();
    let method = str_of(&q, "code_challenge_method");
    let response_type = str_of(&q, "response_type").unwrap_or_default();
    validate_authorize_params(&state, client_id, redirect_uri, method).await?;

    let frontend = state
        .config
        .var("FRONTEND_URL")
        .unwrap_or_else(|| "http://localhost:3001".to_owned());

    let mut pairs: Vec<(&str, &str)> = vec![
        ("client_id", client_id),
        ("redirect_uri", redirect_uri),
        ("code_challenge", code_challenge),
        ("code_challenge_method", method.unwrap_or("S256")),
        ("response_type", response_type),
    ];
    if let Some(s) = str_of(&q, "state").filter(|s| !s.is_empty()) {
        pairs.push(("state", s));
    }
    if let Some(s) = str_of(&q, "scope").filter(|s| !s.is_empty()) {
        pairs.push(("scope", s));
    }
    let url = format!("{frontend}/mcp-consent?{}", form_query(&pairs));
    Ok(redirect_302(&headers, &url))
}

/// Node: src/mcp-api/oauth/oauth.controller.ts:68 (`POST authorize`, 200, `JwtAuthGuard`,
/// `@PermissionExempt('self-service')`). Respons `{ redirect_to }`.
pub async fn h_authorize_post(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Body(b): Body,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    validate_authorize(&b, true)?;
    let approved = b.get("approved").and_then(Value::as_bool).unwrap_or(false);
    let client_id = str_of(&b, "client_id").unwrap_or_default();
    let redirect_uri = str_of(&b, "redirect_uri").unwrap_or_default();
    let code_challenge = str_of(&b, "code_challenge").unwrap_or_default();
    let method = str_of(&b, "code_challenge_method");
    let oauth_state = str_of(&b, "state");

    if !approved {
        // Node tidak memvalidasi `redirect_uri` terhadap klien pada penolakan (hanya `new URL`).
        let url = build_error_redirect_url(redirect_uri, "access_denied", oauth_state)?;
        return Ok(Json(json!({ "redirect_to": url })));
    }

    // `issueAuthCode`
    validate_authorize_params(&state, client_id, redirect_uri, method).await?;
    let code = random_hex(32)?;
    sqlx::query(
        r#"INSERT INTO "mcpOAuthCode"
             (code, "clientId", "userId", "redirectUri", "codeChallenge", "codeChallengeMethod",
              scopes, state, "expiresAt")
           VALUES ($1, $2::uuid, $3, $4, $5, $6, $7, $8, $9)"#,
    )
    .bind(&code)
    .bind(client_id)
    .bind(user_id)
    .bind(redirect_uri)
    .bind(code_challenge)
    .bind(method.unwrap_or("S256"))
    .bind(vec!["read".to_owned()])
    .bind(oauth_state)
    .bind(now_naive() + AUTH_CODE_TTL)
    .execute(&state.db)
    .await?;

    let url = build_redirect_url(redirect_uri, &code, oauth_state)?;
    Ok(Json(json!({ "redirect_to": url })))
}

// ───── token ─────

struct Issued {
    access: String,
    refresh: String,
}

/// Sisipkan pasangan access + refresh token baru di dalam transaksi pemanggil.
async fn issue_pair(
    tx: &mut sqlx::PgConnection,
    client_id: &str,
    user_id: i32,
    scopes: &[String],
) -> ApiResult<Issued> {
    let access = format!("mcp_oauth_{}", random_hex(16)?);
    let now = now_naive();
    sqlx::query(
        r#"INSERT INTO "mcpOAuthAccessToken"
             ("tokenHash", "tokenPrefix", "clientId", "userId", scopes, "expiresAt", status)
           VALUES ($1, $2, $3::uuid, $4, $5, $6, 1)"#,
    )
    .bind(hash_token(&access))
    .bind(access.chars().take(16).collect::<String>())
    .bind(client_id)
    .bind(user_id)
    .bind(scopes)
    .bind(now + ACCESS_TOKEN_TTL)
    .execute(&mut *tx)
    .await?;

    let refresh = format!("mcp_refresh_{}", random_hex(24)?);
    sqlx::query(
        r#"INSERT INTO "mcpOAuthRefreshToken"
             ("tokenHash", "tokenPrefix", "clientId", "userId", scopes, "expiresAt", status)
           VALUES ($1, $2, $3::uuid, $4, $5, $6, 1)"#,
    )
    .bind(hash_token(&refresh))
    .bind(refresh.chars().take(16).collect::<String>())
    .bind(client_id)
    .bind(user_id)
    .bind(scopes)
    .bind(now + REFRESH_TOKEN_TTL)
    .execute(&mut *tx)
    .await?;
    Ok(Issued { access, refresh })
}

fn token_response(issued: &Issued, scopes: &[String]) -> Value {
    json!({
        "access_token": issued.access,
        "token_type": "Bearer",
        "expires_in": ACCESS_TOKEN_TTL_SECS,
        "refresh_token": issued.refresh,
        "refresh_token_expires_in": REFRESH_TOKEN_TTL_SECS,
        "scope": scopes.join(" "),
    })
}

type CodeRow = (
    i32,
    String,
    i32,
    String,
    String,
    String,
    Vec<String>,
    NaiveDateTime,
    Option<NaiveDateTime>,
);

/// Node: src/mcp-api/oauth/oauth.service.ts:117 (`exchangeCode`).
async fn exchange_code(
    state: &AppState,
    code: &str,
    client_id: &str,
    redirect_uri: &str,
    code_verifier: &str,
) -> ApiResult<Value> {
    let row: Option<CodeRow> = sqlx::query_as(
        r#"SELECT id, "clientId"::text, "userId", "redirectUri", "codeChallenge",
                  "codeChallengeMethod", scopes, "expiresAt", "usedAt"
             FROM "mcpOAuthCode" WHERE code = $1"#,
    )
    .bind(code)
    .fetch_optional(&state.db)
    .await?;
    let Some((id, rec_client, user_id, rec_redirect, challenge, method, scopes, expires, used)) =
        row
    else {
        return Err(ApiError::unauthorized("Invalid authorization code"));
    };
    if used.is_some() {
        return Err(ApiError::unauthorized("Authorization code already used"));
    }
    if expires < now_naive() {
        return Err(ApiError::unauthorized("Authorization code expired"));
    }
    if rec_client != client_id {
        return Err(ApiError::unauthorized("client_id mismatch"));
    }
    if rec_redirect != redirect_uri {
        return Err(ApiError::unauthorized("redirect_uri mismatch"));
    }
    verify_pkce(code_verifier, &challenge, &method)?;

    let mut tx = state.db.begin().await?;
    // Penandaan atomik: dua penukaran paralel tidak bisa sama-sama lolos (Node: cek lalu tulis).
    let marked = sqlx::query(
        r#"UPDATE "mcpOAuthCode" SET "usedAt" = $1 WHERE id = $2 AND "usedAt" IS NULL"#,
    )
    .bind(now_naive())
    .bind(id)
    .execute(&mut *tx)
    .await?;
    if marked.rows_affected() == 0 {
        return Err(ApiError::unauthorized("Authorization code already used"));
    }
    let issued = issue_pair(&mut tx, &rec_client, user_id, &scopes).await?;
    tx.commit().await?;
    Ok(token_response(&issued, &scopes))
}

type RefreshRow = (
    i32,
    String,
    i32,
    Vec<String>,
    NaiveDateTime,
    Option<NaiveDateTime>,
    Option<NaiveDateTime>,
    i32,
);

/// Node: src/mcp-api/oauth/oauth.service.ts:168 (`refreshAccessToken`).
async fn refresh_access_token(
    state: &AppState,
    refresh_plain: &str,
    client_id: &str,
) -> ApiResult<Value> {
    if !refresh_plain.starts_with("mcp_refresh_") {
        return Err(ApiError::unauthorized("Invalid refresh token format"));
    }
    let row: Option<RefreshRow> = sqlx::query_as(
        r#"SELECT id, "clientId"::text, "userId", scopes, "expiresAt", "usedAt", "revokedAt", status
             FROM "mcpOAuthRefreshToken" WHERE "tokenHash" = $1"#,
    )
    .bind(hash_token(refresh_plain))
    .fetch_optional(&state.db)
    .await?;
    let Some((id, rec_client, user_id, scopes, expires, used, revoked, status)) = row else {
        return Err(ApiError::unauthorized("Invalid refresh token"));
    };
    if status != 1 || revoked.is_some() {
        return Err(ApiError::unauthorized("Refresh token revoked"));
    }
    if used.is_some() {
        return Err(ApiError::unauthorized("Refresh token already used"));
    }
    if expires < now_naive() {
        return Err(ApiError::unauthorized("Refresh token expired"));
    }
    if rec_client != client_id {
        return Err(ApiError::unauthorized("client_id mismatch"));
    }

    let mut tx = state.db.begin().await?;
    let marked = sqlx::query(
        r#"UPDATE "mcpOAuthRefreshToken" SET "usedAt" = $1 WHERE id = $2 AND "usedAt" IS NULL"#,
    )
    .bind(now_naive())
    .bind(id)
    .execute(&mut *tx)
    .await?;
    if marked.rows_affected() == 0 {
        return Err(ApiError::unauthorized("Refresh token already used"));
    }
    let issued = issue_pair(&mut tx, &rec_client, user_id, &scopes).await?;
    tx.commit().await?;
    Ok(token_response(&issued, &scopes))
}

/// Node: src/mcp-api/oauth/oauth.controller.ts:90 (`POST token`, 200, publik)
pub async fn h_token(State(state): State<AppState>, Body(b): Body) -> ApiResult<Json<Value>> {
    // `TokenRequestDto`
    let mut v = V::default();
    v.is_in(
        "grant_type",
        b.get("grant_type"),
        &["authorization_code", "refresh_token"],
    );
    v.opt_not_empty_string("code", b.get("code"));
    v.opt_not_empty_string("redirect_uri", b.get("redirect_uri"));
    v.opt_not_empty_string("code_verifier", b.get("code_verifier"));
    v.opt_not_empty_string("refresh_token", b.get("refresh_token"));
    v.uuid("client_id", b.get("client_id"));
    v.finish()?;

    let client_id = str_of(&b, "client_id").unwrap_or_default();
    let present = |k: &str| str_of(&b, k).filter(|s| !s.is_empty());

    if str_of(&b, "grant_type") == Some("refresh_token") {
        let Some(rt) = present("refresh_token") else {
            return Err(ApiError::bad_request("refresh_token is required"));
        };
        return Ok(Json(refresh_access_token(&state, rt, client_id).await?));
    }

    let Some(code) = present("code") else {
        return Err(ApiError::bad_request("code is required"));
    };
    let Some(redirect_uri) = present("redirect_uri") else {
        return Err(ApiError::bad_request("redirect_uri is required"));
    };
    let Some(verifier) = present("code_verifier") else {
        return Err(ApiError::bad_request("code_verifier is required"));
    };
    Ok(Json(
        exchange_code(&state, code, client_id, redirect_uri, verifier).await?,
    ))
}

// ───── revoke ─────

/// Node: src/mcp-api/oauth/oauth.controller.ts:110 (`POST revoke`, 200, `JwtAuthGuard`,
/// `@PermissionExempt('self-service')`) + `OAuthService.revokeToken` (oauth.service.ts:243).
/// Hanya access token milik pemanggil; refresh token TIDAK ikut dicabut (sama dengan Node).
pub async fn h_revoke(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Body(b): Body,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    // `RevokeTokenDto`: @IsString @IsNotEmpty.
    let mut v = V::default();
    v.not_empty_string("token", b.get("token"));
    v.finish()?;
    let token = str_of(&b, "token").unwrap_or_default();

    if !token.starts_with("mcp_oauth_") {
        return Err(ApiError::bad_request("Invalid token format"));
    }
    let id: Option<i32> = sqlx::query_scalar(
        r#"SELECT id FROM "mcpOAuthAccessToken"
            WHERE "tokenHash" = $1 AND "userId" = $2 AND status = 1
            LIMIT 1"#,
    )
    .bind(hash_token(token))
    .bind(user_id)
    .fetch_optional(&state.db)
    .await?;
    let Some(id) = id else {
        return Err(ApiError::not_found("Token not found"));
    };
    sqlx::query(r#"UPDATE "mcpOAuthAccessToken" SET "revokedAt" = $1 WHERE id = $2"#)
        .bind(now_naive())
        .bind(id)
        .execute(&state.db)
        .await?;
    Ok(Json(json!({ "revoked": true })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirect_url_building_matches_url_search_params() {
        assert_eq!(
            build_redirect_url("https://x.test/cb", "abc", Some("s 1"))
                .ok()
                .as_deref(),
            Some("https://x.test/cb?code=abc&state=s+1")
        );
        assert_eq!(
            build_redirect_url("https://x.test/cb?a=1&code=old&b=2", "new", None)
                .ok()
                .as_deref(),
            Some("https://x.test/cb?a=1&code=new&b=2")
        );
        assert_eq!(
            build_error_redirect_url("http://localhost:3000/cb", "access_denied", Some("st"))
                .ok()
                .as_deref(),
            Some("http://localhost:3000/cb?error=access_denied&state=st")
        );
        assert!(build_redirect_url("nope", "c", None).is_err());
    }

    #[test]
    fn authorize_validation_order() {
        let m = serde_json::Map::new();
        let e = match validate_authorize(&m, true) {
            Err(ApiError::Validation(v)) => v,
            _ => Vec::new(),
        };
        assert_eq!(
            e,
            [
                "approved must be a boolean value",
                "response_type must be one of the following values: code",
                "client_id must be a UUID",
                "redirect_uri should not be empty",
                "redirect_uri must be a string",
                "code_challenge should not be empty",
                "code_challenge must be a string",
            ]
        );
        assert!(validate_authorize(&m, false).is_err());
    }

    #[test]
    fn token_prefixes_fit_columns() {
        // `tokenPrefix` access/refresh = 16 karakter pertama (VarChar(20)).
        let access = format!("mcp_oauth_{}", "a".repeat(32));
        assert_eq!(access.chars().take(16).collect::<String>().len(), 16);
        assert!(access.starts_with("mcp_oauth_"));
        assert_eq!(random_hex(16).map(|s| s.len()).ok(), Some(32));
        assert_eq!(random_hex(24).map(|s| s.len()).ok(), Some(48));
    }
}
