//! Klien Google Cloud Storage (JSON API) + penandatangan signed URL.
//!
//! Satu-satunya klien GCS di workspace (salinan minimal `mz_users::gcs` dihapus 2026-11-16;
//! log user kini memakai `upload_buffer` / `download_buffer` / `delete_file` di sini).
//!
//! Kredensial seperti `oss.factory.ts`: berkas service account `GCP_KEY_PATH`
//! (JWT bearer grant RS256); bila tidak diset, Application Default Credentials dari
//! metadata server GCE/GKE (signed URL lewat IAM `signBlob`, seperti
//! google-auth-library). Bucket: `GCP_BUCKET_NAME`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use mz_core::{ApiError, AppState};
use serde::{Deserialize, Serialize};
use tokio::sync::OnceCell;

use crate::encode::encode_uri_component;
use crate::signer::{PreparedUrl, RsaSigner, SignError, SignParams, prepare_v4};

const SCOPE: &str = "https://www.googleapis.com/auth/devstorage.read_write";
const DEFAULT_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";
const METADATA_BASE: &str =
    "http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default";
const TOKEN_MARGIN: Duration = Duration::from_secs(60);
/// `AppState.http` punya timeout total 30 dtk — tidak cukup untuk file besar,
/// jadi transfer byte memakai timeout per-request yang lebih longgar.
pub(crate) const TRANSFER_TIMEOUT: Duration = Duration::from_secs(15 * 60);

static TOKEN: Mutex<Option<(String, Instant)>> = Mutex::new(None);
static KEY: OnceCell<Option<Arc<LoadedKey>>> = OnceCell::const_new();
static ADC_EMAIL: OnceCell<String> = OnceCell::const_new();

/// Galat GCS; `NotFound` = HTTP 404 (`ApiError.code === 404` di Node).
#[derive(Debug)]
pub enum GcsError {
    NotFound,
    Other(anyhow::Error),
}

impl std::fmt::Display for GcsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => f.write_str("gcs object not found"),
            Self::Other(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for GcsError {}

impl From<GcsError> for ApiError {
    /// Di Node galat GCS mentah bukan `HttpException` → 500.
    fn from(err: GcsError) -> Self {
        match err {
            GcsError::NotFound => Self::internal(anyhow::anyhow!("gcs object not found")),
            GcsError::Other(e) => Self::internal(e),
        }
    }
}

pub(crate) fn other(msg: impl std::fmt::Display) -> GcsError {
    GcsError::Other(anyhow::anyhow!("{msg}"))
}

impl From<SignError> for GcsError {
    fn from(e: SignError) -> Self {
        other(e)
    }
}

#[derive(Deserialize)]
struct ServiceAccountKey {
    client_email: String,
    private_key: String,
    token_uri: Option<String>,
}

struct LoadedKey {
    raw: ServiceAccountKey,
    signer: RsaSigner,
}

#[derive(Serialize)]
struct Claims<'a> {
    iss: &'a str,
    scope: &'a str,
    aud: &'a str,
    iat: i64,
    exp: i64,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: Option<u64>,
}

/// Assertion JWT untuk grant `urn:ietf:params:oauth:grant-type:jwt-bearer`.
fn signed_assertion(key: &ServiceAccountKey, now_s: i64) -> Result<(String, String), GcsError> {
    let aud = key.token_uri.as_deref().unwrap_or(DEFAULT_TOKEN_URI);
    let claims = Claims {
        iss: &key.client_email,
        scope: SCOPE,
        aud,
        iat: now_s,
        exp: now_s + 3600,
    };
    let enc = EncodingKey::from_rsa_pem(key.private_key.as_bytes()).map_err(other)?;
    let jwt = jsonwebtoken::encode(&Header::new(Algorithm::RS256), &claims, &enc).map_err(other)?;
    Ok((jwt, aud.to_owned()))
}

/// Berkas `GCP_KEY_PATH` (dibaca sekali) atau `None` bila memakai ADC.
async fn load_key(state: &AppState) -> Result<Option<Arc<LoadedKey>>, GcsError> {
    KEY.get_or_try_init(|| async {
        let Some(path) = state.config.var("GCP_KEY_PATH") else {
            return Ok(None);
        };
        let raw = tokio::fs::read_to_string(&path).await.map_err(other)?;
        let raw: ServiceAccountKey = serde_json::from_str(&raw).map_err(other)?;
        let signer = RsaSigner::from_pem(&raw.private_key)?;
        Ok(Some(Arc::new(LoadedKey { raw, signer })))
    })
    .await
    .cloned()
}

async fn fetch_token(state: &AppState) -> Result<(String, Duration), GcsError> {
    let resp = match load_key(state).await? {
        Some(key) => {
            let (jwt, aud) = signed_assertion(&key.raw, chrono::Utc::now().timestamp())?;
            state
                .http
                .post(aud)
                .form(&[
                    ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
                    ("assertion", jwt.as_str()),
                ])
                .send()
                .await
                .map_err(other)?
        }
        None => state
            .http
            .get(format!("{METADATA_BASE}/token"))
            .header("Metadata-Flavor", "Google")
            .send()
            .await
            .map_err(other)?,
    };
    if !resp.status().is_success() {
        return Err(other(format!(
            "gcs token request failed: {}",
            resp.status()
        )));
    }
    let body: TokenResponse = resp.json().await.map_err(other)?;
    Ok((
        body.access_token,
        Duration::from_secs(body.expires_in.unwrap_or(300)),
    ))
}

pub(crate) async fn access_token(state: &AppState) -> Result<String, GcsError> {
    if let Ok(guard) = TOKEN.lock()
        && let Some((token, until)) = guard.as_ref()
        && Instant::now() < *until
    {
        return Ok(token.clone());
    }
    let (token, ttl) = fetch_token(state).await?;
    if let Ok(mut guard) = TOKEN.lock() {
        *guard = Some((
            token.clone(),
            Instant::now() + ttl.saturating_sub(TOKEN_MARGIN),
        ));
    }
    Ok(token)
}

/// `process.env.GCP_BUCKET_NAME`.
pub fn default_bucket(state: &AppState) -> Result<String, GcsError> {
    state
        .config
        .var("GCP_BUCKET_NAME")
        .ok_or_else(|| other("GCP_BUCKET_NAME is not configured"))
}

pub(crate) fn resolve_bucket(state: &AppState, bucket: Option<&str>) -> Result<String, GcsError> {
    match bucket {
        Some(b) => Ok(b.to_owned()),
        None => default_bucket(state),
    }
}

const STORAGE_BASE: &str = "https://storage.googleapis.com";

/// `STORAGE_EMULATOR_HOST` (`@google-cloud/storage`): endpoint lain (mis. fake-gcs-server),
/// tanpa kredensial (`useAuthWithCustomEndpoint` bawaan `false`). Tanpa skema → `https://`
/// seperti `sanitizeEndpoint`.
fn emulator_base(state: &AppState) -> Option<String> {
    let raw = state.config.var("STORAGE_EMULATOR_HOST")?;
    let raw = raw.trim().trim_end_matches('/');
    Some(if raw.contains("://") {
        raw.to_owned()
    } else {
        format!("https://{raw}")
    })
}

fn storage_base(state: &AppState) -> String {
    emulator_base(state).unwrap_or_else(|| STORAGE_BASE.to_owned())
}

/// Header otorisasi JSON API: bearer token, atau kosong bila memakai emulator.
pub(crate) async fn storage_auth(state: &AppState) -> Result<reqwest::header::HeaderMap, GcsError> {
    let mut headers = reqwest::header::HeaderMap::new();
    if emulator_base(state).is_none() {
        let token = access_token(state).await?;
        let value =
            reqwest::header::HeaderValue::from_str(&format!("Bearer {token}")).map_err(other)?;
        headers.insert(reqwest::header::AUTHORIZATION, value);
    }
    Ok(headers)
}

/// URL objek (`/storage/v1/b/<bucket>/o/<key>`), key di-encode seperti `encodeURIComponent`.
pub fn object_url(bucket: &str, key: &str) -> String {
    object_url_at(STORAGE_BASE, bucket, key)
}

fn object_url_at(base: &str, bucket: &str, key: &str) -> String {
    format!(
        "{base}/storage/v1/b/{}/o/{}",
        encode_uri_component(bucket),
        encode_uri_component(key)
    )
}

/// [`object_url`] dengan endpoint `STORAGE_EMULATOR_HOST` bila diset.
pub(crate) fn object_url_in(state: &AppState, bucket: &str, key: &str) -> String {
    object_url_at(&storage_base(state), bucket, key)
}

fn upload_url_at(base: &str, bucket: &str, key: &str) -> String {
    format!(
        "{base}/upload/storage/v1/b/{}/o?uploadType=media&name={}",
        encode_uri_component(bucket),
        encode_uri_component(key)
    )
}

pub(crate) fn upload_url_in(state: &AppState, bucket: &str, key: &str) -> String {
    upload_url_at(&storage_base(state), bucket, key)
}

/// Email service account untuk signed URL: dari berkas kunci, atau metadata server (ADC).
async fn signer_email(state: &AppState) -> Result<String, GcsError> {
    if let Some(key) = load_key(state).await? {
        return Ok(key.raw.client_email.clone());
    }
    ADC_EMAIL
        .get_or_try_init(|| async {
            let resp = state
                .http
                .get(format!("{METADATA_BASE}/email"))
                .header("Metadata-Flavor", "Google")
                .send()
                .await
                .map_err(other)?;
            if !resp.status().is_success() {
                return Err(other(format!(
                    "metadata email request failed: {}",
                    resp.status()
                )));
            }
            Ok(resp.text().await.map_err(other)?.trim().to_owned())
        })
        .await
        .cloned()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SignBlobResponse {
    signed_blob: String,
}

/// Tanda tangan RSA-SHA256: lokal bila ada kunci, selain itu IAM `signBlob`.
async fn sign_blob(state: &AppState, email: &str, payload: &[u8]) -> Result<Vec<u8>, GcsError> {
    if let Some(key) = load_key(state).await? {
        return Ok(key.signer.sign(payload)?);
    }
    let token = access_token(state).await?;
    let url = format!(
        "https://iamcredentials.googleapis.com/v1/projects/-/serviceAccounts/{}:signBlob",
        encode_uri_component(email)
    );
    let resp = state
        .http
        .post(url)
        .bearer_auth(token)
        .json(&serde_json::json!({ "payload": STANDARD.encode(payload) }))
        .send()
        .await
        .map_err(other)?;
    if !resp.status().is_success() {
        return Err(other(format!("iam signBlob failed: {}", resp.status())));
    }
    let body: SignBlobResponse = resp.json().await.map_err(other)?;
    STANDARD.decode(body.signed_blob).map_err(other)
}

/// Parameter untuk [`sign_url`] (tanpa email, yang ditentukan oleh kredensial).
pub struct UrlRequest<'a> {
    pub action: crate::signer::SignedAction,
    pub bucket: &'a str,
    pub key: &'a str,
    pub accessible_at_ms: i64,
    pub expires_ms: i64,
    pub content_type: Option<&'a str>,
    pub response_type: Option<&'a str>,
    pub response_disposition: Option<&'a str>,
}

/// `file.getSignedUrl({ version: 'v4', ... })`.
pub async fn sign_url(state: &AppState, req: &UrlRequest<'_>) -> Result<String, GcsError> {
    let email = signer_email(state).await?;
    let prepared: PreparedUrl = prepare_v4(&SignParams {
        action: req.action,
        bucket: req.bucket,
        key: req.key,
        client_email: &email,
        accessible_at_ms: req.accessible_at_ms,
        expires_ms: req.expires_ms,
        content_type: req.content_type,
        response_type: req.response_type,
        response_disposition: req.response_disposition,
    })?;
    let sig = sign_blob(state, &email, prepared.string_to_sign.as_bytes()).await?;
    Ok(prepared.finish(&sig))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_url_encodes_slashes() {
        assert_eq!(
            object_url("my-bucket", "user-logs/1/2024-01-15/x.log.gz"),
            "https://storage.googleapis.com/storage/v1/b/my-bucket/o/user-logs%2F1%2F2024-01-15%2Fx.log.gz"
        );
    }

    #[test]
    fn upload_url_carries_name() {
        assert!(upload_url_at(STORAGE_BASE, "b", "a/b").ends_with("?uploadType=media&name=a%2Fb"));
    }

    #[test]
    fn bad_private_key_is_an_error_not_a_panic() {
        let key = ServiceAccountKey {
            client_email: "x@y".into(),
            private_key: "not a pem".into(),
            token_uri: None,
        };
        assert!(signed_assertion(&key, 0).is_err());
    }

    #[test]
    fn assertion_is_signed_with_test_key() {
        let key = ServiceAccountKey {
            client_email: "x@y".into(),
            private_key: crate::signer::test_vectors::TEST_KEY_PEM.to_owned(),
            token_uri: Some("https://example.test/token".into()),
        };
        let (jwt, aud) = signed_assertion(&key, 1_800_000_000).unwrap_or_default();
        assert_eq!(aud, "https://example.test/token");
        assert_eq!(jwt.split('.').count(), 3);
    }
}
