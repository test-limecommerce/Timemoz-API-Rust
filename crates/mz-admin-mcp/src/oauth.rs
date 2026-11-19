//! Port `src/admin-mcp/oauth/*`: server OAuth 2.1 admin MCP (registrasi klien dinamis,
//! authorize + PKCE S256, token, refresh rotasi, revoke) + dokumen `.well-known`.
//!
//! Keamanan: token/refresh disimpan sebagai SHA-256 hex; kode otorisasi disimpan apa adanya
//! (sama dengan Node), kedaluwarsa 10 menit, sekali pakai; access token 24 jam; refresh
//! token 365 hari, dirotasi tiap dipakai. Lihat `plan/deviations/mz-admin-mcp.md`.

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::Response;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{Duration, NaiveDateTime};
use mz_core::redirect::redirect_302;
use mz_core::time::now_naive;
use mz_core::{ApiError, ApiResult, AppState};
use mz_operator::Operator;
use rand::TryRngCore;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use url::Url;

use crate::body::{Body, V, query_map, str_of};
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

fn internal(msg: &str) -> ApiError {
    ApiError::internal(anyhow::anyhow!("{msg}"))
}

// ───── well-known ─────

/// Node: src/admin-mcp/oauth/well-known.controller.ts:15 (`oauth-authorization-server`)
pub async fn h_metadata(State(state): State<AppState>) -> Json<Value> {
    let issuer = api_url(&state);
    Json(json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{issuer}/admin-mcp/oauth/authorize"),
        "token_endpoint": format!("{issuer}/admin-mcp/oauth/token"),
        "registration_endpoint": format!("{issuer}/admin-mcp/oauth/register"),
        "revocation_endpoint": format!("{issuer}/admin-mcp/oauth/revoke"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
        "scopes_supported": ["read"],
    }))
}

/// Node: src/admin-mcp/oauth/well-known.controller.ts:22 (`oauth-protected-resource`)
pub async fn h_protected_resource(State(state): State<AppState>) -> Json<Value> {
    let issuer = api_url(&state);
    Json(json!({
        "resource": format!("{issuer}/admin-mcp"),
        "authorization_servers": [issuer],
        "bearer_methods_supported": ["header"],
        "scopes_supported": ["read"],
    }))
}

// ───── register ─────

/// `validateRedirectUris`.
fn validate_redirect_uris(uris: &[String]) -> ApiResult<()> {
    for uri in uris {
        let Ok(url) = Url::parse(uri) else {
            return Err(ApiError::bad_request(format!(
                "Invalid redirect_uri: {uri}"
            )));
        };
        let is_localhost = matches!(url.host_str(), Some("localhost" | "127.0.0.1"));
        let is_https = url.scheme() == "https";
        let is_custom_scheme = !matches!(url.scheme(), "http" | "https");
        if !is_localhost && !is_https && !is_custom_scheme {
            return Err(ApiError::bad_request(format!(
                "redirect_uri must use HTTPS or a custom scheme (except localhost): {uri}"
            )));
        }
    }
    Ok(())
}

fn string_list(v: Option<&Value>) -> Option<Vec<String>> {
    match v {
        Some(Value::Array(a)) => Some(
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_owned))
                .collect(),
        ),
        // `@IsString({ each })` meloloskan string tunggal, tetapi `@IsArray` menolaknya lebih dulu.
        _ => None,
    }
}

/// Node: src/admin-mcp/oauth/admin-oauth.controller.ts:31 (`POST register`, 201)
pub async fn h_register(
    State(state): State<AppState>,
    Body(b): Body,
) -> ApiResult<(StatusCode, Json<Value>)> {
    // `RegisterClientDto` (pipe global).
    let mut v = V::default();
    if !crate::body::is_nullish(b.get("client_name")) {
        v.max_length("client_name", b.get("client_name"), 255);
        v.opt_string("client_name", b.get("client_name"));
    }
    v.string_array("redirect_uris", b.get("redirect_uris"));
    v.opt_string_array("grant_types", b.get("grant_types"));
    v.opt_string_array("response_types", b.get("response_types"));
    v.opt_string(
        "token_endpoint_auth_method",
        b.get("token_endpoint_auth_method"),
    );
    v.opt_string("scope", b.get("scope"));
    v.finish()?;

    let redirect_uris = string_list(b.get("redirect_uris")).unwrap_or_default();
    validate_redirect_uris(&redirect_uris)?;

    let client_name = str_of(&b, "client_name").unwrap_or("Unknown Client");
    let grant_types =
        string_list(b.get("grant_types")).unwrap_or_else(|| vec!["authorization_code".to_owned()]);
    let response_types =
        string_list(b.get("response_types")).unwrap_or_else(|| vec!["code".to_owned()]);
    let auth_method = str_of(&b, "token_endpoint_auth_method").unwrap_or("none");
    let scope = str_of(&b, "scope").unwrap_or("read");

    let (id, name, uris, grants, responses, method, scope): (
        String,
        String,
        Vec<String>,
        Vec<String>,
        Vec<String>,
        String,
        String,
    ) = sqlx::query_as(
        r#"INSERT INTO "mcpOAuthClient"
             ("clientName", "redirectUris", "grantTypes", "responseTypes", "tokenEndpointAuthMethod", scope)
           VALUES ($1, $2, $3, $4, $5, $6)
           RETURNING id::text, "clientName", "redirectUris", "grantTypes", "responseTypes",
                     "tokenEndpointAuthMethod", scope"#,
    )
    .bind(client_name)
    .bind(&redirect_uris)
    .bind(&grant_types)
    .bind(&response_types)
    .bind(auth_method)
    .bind(scope)
    .fetch_one(&state.db)
    .await?;

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "client_id": id,
            "client_name": name,
            "redirect_uris": uris,
            "grant_types": grants,
            "response_types": responses,
            "token_endpoint_auth_method": method,
            "scope": scope,
        })),
    ))
}

// ───── authorize ─────

/// `AdminAuthorizeQueryDto` / `AdminApproveAuthorizeDto`. `approve` menambahkan
/// `approved` (properti kelas turunan = divalidasi lebih dulu).
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

/// `validateAuthorizeParams`.
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

/// Percent-encode gaya `URLSearchParams` (application/x-www-form-urlencoded).
pub fn form_query(pairs: &[(&str, &str)]) -> String {
    let mut s = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in pairs {
        s.append_pair(k, v);
    }
    s.finish()
}

/// `url.searchParams.set(key, value)`: ganti kemunculan pertama, buang yang lain, tambahkan
/// di akhir bila belum ada; query diserialisasi ulang.
pub fn set_search_param(url: &mut Url, key: &str, value: &str) {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut done = false;
    for (k, v) in url.query_pairs() {
        if k == key {
            if !done {
                out.push((k.into_owned(), value.to_owned()));
                done = true;
            }
        } else {
            out.push((k.into_owned(), v.into_owned()));
        }
    }
    if !done {
        out.push((key.to_owned(), value.to_owned()));
    }
    url.query_pairs_mut().clear().extend_pairs(out);
}

/// `new URL(redirectUri)` gagal → `TypeError` → 500 di Node.
pub fn parse_redirect(redirect_uri: &str) -> ApiResult<Url> {
    Url::parse(redirect_uri).map_err(|_| internal("Invalid URL"))
}

/// `buildRedirectUrl`.
pub fn build_redirect_url(
    redirect_uri: &str,
    code: &str,
    state: Option<&str>,
) -> ApiResult<String> {
    let mut url = parse_redirect(redirect_uri)?;
    set_search_param(&mut url, "code", code);
    if let Some(s) = state.filter(|s| !s.is_empty()) {
        set_search_param(&mut url, "state", s);
    }
    Ok(url.to_string())
}

/// `buildErrorRedirectUrl`.
pub fn build_error_redirect_url(
    redirect_uri: &str,
    error: &str,
    state: Option<&str>,
) -> ApiResult<String> {
    let mut url = parse_redirect(redirect_uri)?;
    set_search_param(&mut url, "error", error);
    if let Some(s) = state.filter(|s| !s.is_empty()) {
        set_search_param(&mut url, "state", s);
    }
    Ok(url.to_string())
}

/// Node: src/admin-mcp/oauth/admin-oauth.controller.ts:41 (`GET authorize`, `@Redirect()` → 302)
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

    let admin_frontend = state
        .config
        .var("ADMIN_FRONTEND_URL")
        .or_else(|| state.config.var("FRONTEND_URL"))
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
    let url = format!("{admin_frontend}/admin/mcp-consent?{}", form_query(&pairs));
    Ok(redirect_302(&headers, &url))
}

/// Node: src/admin-mcp/oauth/admin-oauth.controller.ts:67 (`POST authorize`, 200, JwtAdminAuthGuard)
pub async fn h_authorize_post(
    State(state): State<AppState>,
    op: Operator,
    Body(b): Body,
) -> ApiResult<Json<Value>> {
    validate_authorize(&b, true)?;
    let approved = b.get("approved").and_then(Value::as_bool).unwrap_or(false);
    let client_id = str_of(&b, "client_id").unwrap_or_default();
    let redirect_uri = str_of(&b, "redirect_uri").unwrap_or_default();
    let code_challenge = str_of(&b, "code_challenge").unwrap_or_default();
    let method = str_of(&b, "code_challenge_method");
    let oauth_state = str_of(&b, "state");

    if !approved {
        let url = build_error_redirect_url(redirect_uri, "access_denied", oauth_state)?;
        return Ok(Json(json!({ "redirect_uri": url })));
    }

    // `issueAuthCode`
    validate_authorize_params(&state, client_id, redirect_uri, method).await?;
    let code = random_hex(32)?;
    sqlx::query(
        r#"INSERT INTO "mcpAdminOAuthCode"
             (code, "clientId", "operatorId", "redirectUri", "codeChallenge", "codeChallengeMethod",
              scopes, state, "expiresAt")
           VALUES ($1, $2::uuid, $3, $4, $5, $6, $7, $8, $9)"#,
    )
    .bind(&code)
    .bind(client_id)
    .bind(op.id)
    .bind(redirect_uri)
    .bind(code_challenge)
    .bind(method.unwrap_or("S256"))
    .bind(vec!["read".to_owned()])
    .bind(oauth_state)
    .bind(now_naive() + AUTH_CODE_TTL)
    .execute(&state.db)
    .await?;

    let url = build_redirect_url(redirect_uri, &code, oauth_state)?;
    Ok(Json(json!({ "redirect_uri": url })))
}

// ───── token ─────

/// `verifyPKCE`: hanya S256; `base64url(sha256(verifier))` dibandingkan dalam waktu konstan.
pub fn verify_pkce(verifier: &str, challenge: &str, method: &str) -> ApiResult<()> {
    if method != "S256" {
        return Err(ApiError::unauthorized("Unsupported code_challenge_method"));
    }
    let computed = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    if !ct_eq(computed.as_bytes(), challenge.as_bytes()) {
        return Err(ApiError::unauthorized("PKCE verification failed"));
    }
    Ok(())
}

/// `a.length === b.length && timingSafeEqual(a, b)`.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0_u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

struct Issued {
    access: String,
    refresh: String,
}

/// Sisipkan pasangan access + refresh token baru di dalam transaksi pemanggil.
async fn issue_pair(
    tx: &mut sqlx::PgConnection,
    client_id: &str,
    operator_id: i32,
    scopes: &[String],
) -> ApiResult<Issued> {
    let access = format!("mcp_admin_oauth_{}", random_hex(16)?);
    let now = now_naive();
    sqlx::query(
        r#"INSERT INTO "mcpAdminOAuthAccessToken"
             ("tokenHash", "tokenPrefix", "clientId", "operatorId", scopes, "expiresAt", status)
           VALUES ($1, $2, $3::uuid, $4, $5, $6, 1)"#,
    )
    .bind(hash_token(&access))
    .bind(access.chars().take(20).collect::<String>())
    .bind(client_id)
    .bind(operator_id)
    .bind(scopes)
    .bind(now + ACCESS_TOKEN_TTL)
    .execute(&mut *tx)
    .await?;

    let refresh = format!("mcp_admin_refresh_{}", random_hex(24)?);
    sqlx::query(
        r#"INSERT INTO "mcpAdminOAuthRefreshToken"
             ("tokenHash", "tokenPrefix", "clientId", "operatorId", scopes, "expiresAt", status)
           VALUES ($1, $2, $3::uuid, $4, $5, $6, 1)"#,
    )
    .bind(hash_token(&refresh))
    .bind(refresh.chars().take(20).collect::<String>())
    .bind(client_id)
    .bind(operator_id)
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

/// `exchangeCode`.
async fn exchange_code(
    state: &AppState,
    code: &str,
    client_id: &str,
    redirect_uri: &str,
    code_verifier: &str,
) -> ApiResult<Value> {
    let row: Option<CodeRow> = sqlx::query_as(
        r#"SELECT id, "clientId"::text, "operatorId", "redirectUri", "codeChallenge",
                  "codeChallengeMethod", scopes, "expiresAt", "usedAt"
             FROM "mcpAdminOAuthCode" WHERE code = $1"#,
    )
    .bind(code)
    .fetch_optional(&state.db)
    .await?;
    let Some((id, rec_client, operator_id, rec_redirect, challenge, method, scopes, expires, used)) =
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
        r#"UPDATE "mcpAdminOAuthCode" SET "usedAt" = $1 WHERE id = $2 AND "usedAt" IS NULL"#,
    )
    .bind(now_naive())
    .bind(id)
    .execute(&mut *tx)
    .await?;
    if marked.rows_affected() == 0 {
        return Err(ApiError::unauthorized("Authorization code already used"));
    }
    let issued = issue_pair(&mut tx, &rec_client, operator_id, &scopes).await?;
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

/// `refreshAccessToken`.
async fn refresh_access_token(
    state: &AppState,
    refresh_plain: &str,
    client_id: &str,
) -> ApiResult<Value> {
    if !refresh_plain.starts_with("mcp_admin_refresh_") {
        return Err(ApiError::unauthorized("Invalid refresh token format"));
    }
    let row: Option<RefreshRow> = sqlx::query_as(
        r#"SELECT id, "clientId"::text, "operatorId", scopes, "expiresAt", "usedAt", "revokedAt", status
             FROM "mcpAdminOAuthRefreshToken" WHERE "tokenHash" = $1"#,
    )
    .bind(hash_token(refresh_plain))
    .fetch_optional(&state.db)
    .await?;
    let Some((id, rec_client, operator_id, scopes, expires, used, revoked, status)) = row else {
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
        r#"UPDATE "mcpAdminOAuthRefreshToken" SET "usedAt" = $1 WHERE id = $2 AND "usedAt" IS NULL"#,
    )
    .bind(now_naive())
    .bind(id)
    .execute(&mut *tx)
    .await?;
    if marked.rows_affected() == 0 {
        return Err(ApiError::unauthorized("Refresh token already used"));
    }
    let issued = issue_pair(&mut tx, &rec_client, operator_id, &scopes).await?;
    tx.commit().await?;
    Ok(token_response(&issued, &scopes))
}

/// Node: src/admin-mcp/oauth/admin-oauth.controller.ts:78 (`POST token`, 200, publik)
pub async fn h_token(State(state): State<AppState>, Body(b): Body) -> ApiResult<Json<Value>> {
    // `AdminTokenRequestDto`
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
    match str_of(&b, "grant_type") {
        Some("authorization_code") => {
            let (code, redirect_uri, verifier) = (
                str_of(&b, "code").filter(|s| !s.is_empty()),
                str_of(&b, "redirect_uri").filter(|s| !s.is_empty()),
                str_of(&b, "code_verifier").filter(|s| !s.is_empty()),
            );
            let (Some(code), Some(redirect_uri), Some(verifier)) = (code, redirect_uri, verifier)
            else {
                return Err(ApiError::bad_request(
                    "code, redirect_uri, and code_verifier are required",
                ));
            };
            Ok(Json(
                exchange_code(&state, code, client_id, redirect_uri, verifier).await?,
            ))
        }
        Some("refresh_token") => {
            let Some(rt) = str_of(&b, "refresh_token").filter(|s| !s.is_empty()) else {
                return Err(ApiError::bad_request("refresh_token is required"));
            };
            Ok(Json(refresh_access_token(&state, rt, client_id).await?))
        }
        _ => Err(ApiError::bad_request("Unsupported grant_type")),
    }
}

// ───── revoke ─────

/// Node: src/admin-mcp/oauth/admin-oauth.controller.ts:104 (`POST revoke`, 200, JwtAdminAuthGuard)
pub async fn h_revoke(
    State(state): State<AppState>,
    op: Operator,
    Body(b): Body,
) -> ApiResult<Json<Value>> {
    // `body.token.startsWith` pada nilai non-string → TypeError → 500 di Node.
    let Some(token) = str_of(&b, "token") else {
        return Err(internal(
            "Cannot read properties of undefined (reading 'startsWith')",
        ));
    };
    if !token.starts_with("mcp_admin_oauth_") {
        return Err(ApiError::bad_request("Invalid token format"));
    }
    let id: Option<i32> = sqlx::query_scalar(
        r#"SELECT id FROM "mcpAdminOAuthAccessToken"
            WHERE "tokenHash" = $1 AND "operatorId" = $2 AND status = 1"#,
    )
    .bind(hash_token(token))
    .bind(op.id)
    .fetch_optional(&state.db)
    .await?;
    let Some(id) = id else {
        return Err(ApiError::not_found("Token not found"));
    };
    sqlx::query(r#"UPDATE "mcpAdminOAuthAccessToken" SET "revokedAt" = $1 WHERE id = $2"#)
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
    fn pkce_s256_rfc7636_vector() {
        // RFC 7636 appendix B.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        assert!(verify_pkce(verifier, challenge, "S256").is_ok());
        assert!(verify_pkce(verifier, "x", "S256").is_err());
        assert!(verify_pkce(verifier, challenge, "plain").is_err());
        assert!(verify_pkce("other", challenge, "S256").is_err());
    }

    #[test]
    fn redirect_uri_rules() {
        let ok = |s: &str| validate_redirect_uris(&[s.to_owned()]).is_ok();
        assert!(ok("https://example.com/cb"));
        assert!(ok("http://localhost:3000/cb"));
        assert!(ok("http://127.0.0.1/cb"));
        assert!(ok("cursor://anysphere.cursor-retrieval/oauth/callback"));
        assert!(!ok("http://example.com/cb"));
        assert!(!ok("not a url"));
        assert!(!ok("/relative"));
        assert!(validate_redirect_uris(&[]).is_ok());
    }

    #[test]
    fn redirect_url_building_matches_url_search_params() {
        assert_eq!(
            build_redirect_url("https://x.test/cb", "abc", Some("s 1"))
                .ok()
                .as_deref(),
            Some("https://x.test/cb?code=abc&state=s+1")
        );
        // param yang ada diganti di tempat; lainnya dipertahankan
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
        assert_eq!(
            build_redirect_url("app://cb#frag", "c", Some(""))
                .ok()
                .as_deref(),
            Some("app://cb?code=c#frag")
        );
        assert!(build_redirect_url("nope", "c", None).is_err());
    }

    #[test]
    fn form_query_encoding() {
        assert_eq!(
            form_query(&[("a", "x y&z"), ("b", "http://h/p?q=1")]),
            "a=x+y%26z&b=http%3A%2F%2Fh%2Fp%3Fq%3D1"
        );
    }

    #[test]
    fn ct_eq_rules() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"abcd"));
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
    }
}
