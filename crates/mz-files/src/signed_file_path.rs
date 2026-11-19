//! Port `src/files/signed-file-path.ts`: tanda tangan HMAC untuk URL proxy file
//! tanpa JWT (`<path>?exp=<unix>&sig=<hmac>`).

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac};
use http::HeaderMap;
use http::header::HeaderValue;
use mz_core::AppState;
use sha2::Sha256;

/// Masa berlaku URL proxy mobile.
pub const SIGNED_FILE_PATH_TTL_SECONDS: i64 = 24 * 60 * 60;
/// `FILE_URL_SECRET` minimal 32 byte.
const MIN_SECRET_BYTES: usize = 32;
/// Toleransi jam untuk `exp` di masa depan.
const FUTURE_SKEW_SECONDS: i64 = 60;

/// `Cache-Control` proxy bertanda tangan.
pub const SIGNED_FILE_CACHE_CONTROL: &str = "private, max-age=86400";

/// `FileUrlSecretMissingError`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileUrlSecretMissingError;

impl std::fmt::Display for FileUrlSecretMissingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "FILE_URL_SECRET is not set or shorter than {MIN_SECRET_BYTES} bytes"
        )
    }
}

impl std::error::Error for FileUrlSecretMissingError {}

impl From<FileUrlSecretMissingError> for mz_core::ApiError {
    fn from(e: FileUrlSecretMissingError) -> Self {
        Self::internal(anyhow::anyhow!("{e}"))
    }
}

fn hmac_sig(secret: &[u8], path: &str, exp: i64) -> String {
    // HMAC menerima kunci panjang berapa pun; `new_from_slice` tidak pernah gagal.
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret) else {
        return String::new();
    };
    mac.update(format!("{path}|{exp}").as_bytes());
    URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
}

fn usable_secret(raw: Option<&str>) -> Option<&[u8]> {
    raw.map(str::as_bytes)
        .filter(|s| s.len() >= MIN_SECRET_BYTES)
}

/// `signFilePath(path, nowMs, ttlSeconds)` dengan secret eksplisit.
pub fn sign_file_path_with(
    secret: Option<&str>,
    path: &str,
    now_ms: i64,
    ttl_seconds: i64,
) -> Result<String, FileUrlSecretMissingError> {
    let secret = usable_secret(secret).ok_or(FileUrlSecretMissingError)?;
    let exp = now_ms.div_euclid(1000) + ttl_seconds;
    Ok(format!(
        "{path}?exp={exp}&sig={}",
        hmac_sig(secret, path, exp)
    ))
}

/// `signFilePath(path)` dengan `FILE_URL_SECRET` dan jam sekarang.
pub fn sign_file_path(state: &AppState, path: &str) -> Result<String, FileUrlSecretMissingError> {
    sign_file_path_with(
        state.config.var("FILE_URL_SECRET").as_deref(),
        path,
        chrono::Utc::now().timestamp_millis(),
        SIGNED_FILE_PATH_TTL_SECONDS,
    )
}

fn is_sig_shape(sig: &str) -> bool {
    sig.len() == 43
        && sig
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn is_exp_shape(exp: &str) -> bool {
    (1..=12).contains(&exp.len()) && exp.bytes().all(|b| b.is_ascii_digit())
}

/// Perbandingan waktu-konstan untuk string berpanjang sama.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// `verifyFilePathSignature(path, exp, sig, nowMs)` dengan secret eksplisit.
/// `exp`/`sig` = nilai query mentah; `None` bila tidak ada atau bukan string tunggal.
pub fn verify_file_path_signature_with(
    secret: Option<&str>,
    path: &str,
    exp: Option<&str>,
    sig: Option<&str>,
    now_ms: i64,
) -> bool {
    let (Some(exp), Some(sig)) = (exp, sig) else {
        return false;
    };
    if !is_exp_shape(exp) || !is_sig_shape(sig) {
        return false;
    }
    let Ok(exp_seconds) = exp.parse::<i64>() else {
        return false;
    };
    let now_seconds = now_ms.div_euclid(1000);
    if exp_seconds <= now_seconds {
        return false;
    }
    if exp_seconds > now_seconds + SIGNED_FILE_PATH_TTL_SECONDS + FUTURE_SKEW_SECONDS {
        return false;
    }
    let Some(secret) = usable_secret(secret) else {
        return false;
    };
    ct_eq(
        hmac_sig(secret, path, exp_seconds).as_bytes(),
        sig.as_bytes(),
    )
}

/// `verifyFilePathSignature(path, exp, sig)` dengan `FILE_URL_SECRET` dan jam sekarang.
pub fn verify_file_path_signature(
    state: &AppState,
    path: &str,
    exp: Option<&str>,
    sig: Option<&str>,
) -> bool {
    verify_file_path_signature_with(
        state.config.var("FILE_URL_SECRET").as_deref(),
        path,
        exp,
        sig,
        chrono::Utc::now().timestamp_millis(),
    )
}

/// `setUnauthenticatedFileHeaders`: `Referrer-Policy: no-referrer`, `X-Robots-Tag: noindex`.
pub fn set_unauthenticated_file_headers(headers: &mut HeaderMap) {
    headers.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    headers.insert("x-robots-tag", HeaderValue::from_static("noindex"));
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx";
    const PATH: &str = "/v1/mobile/store-visits/a/photos/b/file";
    const NOW: i64 = 1_800_000_000_000;

    fn parts(url: &str) -> (String, String, String) {
        let (path, query) = url.split_once('?').unwrap_or((url, ""));
        let mut exp = String::new();
        let mut sig = String::new();
        for kv in query.split('&') {
            if let Some(v) = kv.strip_prefix("exp=") {
                exp = v.to_owned();
            } else if let Some(v) = kv.strip_prefix("sig=") {
                sig = v.to_owned();
            }
        }
        (path.to_owned(), exp, sig)
    }

    fn signed() -> (String, String) {
        let url = sign_file_path_with(Some(SECRET), PATH, NOW, SIGNED_FILE_PATH_TTL_SECONDS)
            .unwrap_or_default();
        let (p, exp, sig) = parts(&url);
        assert_eq!(p, PATH);
        (exp, sig)
    }

    fn verify(path: &str, exp: &str, sig: &str, now: i64) -> bool {
        verify_file_path_signature_with(Some(SECRET), path, Some(exp), Some(sig), now)
    }

    #[test]
    fn round_trip() {
        let (exp, sig) = signed();
        assert_eq!(
            exp.parse::<i64>().ok(),
            Some(NOW / 1000 + SIGNED_FILE_PATH_TTL_SECONDS)
        );
        assert!(verify(PATH, &exp, &sig, NOW));
    }

    #[test]
    fn known_vector_matches_node_hmac() {
        // node: createHmac('sha256','x'.repeat(32)).update(PATH+'|1800086400').digest('base64url')
        let url = sign_file_path_with(Some(SECRET), PATH, NOW, SIGNED_FILE_PATH_TTL_SECONDS)
            .unwrap_or_default();
        assert_eq!(url, format!("{PATH}?exp=1800086400&sig={KNOWN_SIG}"));
    }

    const KNOWN_SIG: &str = "eGx64iIVLZM-4M6DoWKECewbHXdUfRZxwxJlcbubuyE";

    #[test]
    fn rejects_other_path_and_tampered_exp() {
        let (exp, sig) = signed();
        assert!(!verify(
            "/v1/mobile/store-visits/a/photos/c/file",
            &exp,
            &sig,
            NOW
        ));
        let bumped = (exp.parse::<i64>().unwrap_or(0) + 1).to_string();
        assert!(!verify(PATH, &bumped, &sig, NOW));
    }

    #[test]
    fn expiry_boundaries() {
        let (exp, sig) = signed();
        let exp_s = exp.parse::<i64>().unwrap_or(0);
        assert!(!verify(PATH, &exp, &sig, exp_s * 1000));
        assert!(verify(PATH, &exp, &sig, exp_s * 1000 - 1000));
    }

    #[test]
    fn rejects_exp_beyond_ttl_even_if_signed() {
        let url = sign_file_path_with(Some(SECRET), PATH, NOW, 10 * 24 * 3600).unwrap_or_default();
        let (_, exp, sig) = parts(&url);
        assert!(!verify(PATH, &exp, &sig, NOW));
    }

    #[test]
    fn rotated_secret_kills_old_urls() {
        let (exp, sig) = signed();
        let other = "y".repeat(32);
        assert!(!verify_file_path_signature_with(
            Some(&other),
            PATH,
            Some(&exp),
            Some(&sig),
            NOW
        ));
    }

    #[test]
    fn malformed_input() {
        let ok_sig = "a".repeat(43);
        let near = (NOW / 1000 + 60).to_string();
        for (exp, sig) in [
            (Some("1e12"), Some(ok_sig.as_str())),
            (Some("-1"), Some(ok_sig.as_str())),
            (Some(near.as_str()), None),
            (None, Some(ok_sig.as_str())),
            (Some(near.as_str()), Some("abc")),
            (
                Some(near.as_str()),
                Some(format!("{}=", "a".repeat(42)).as_str()),
            ),
        ] {
            assert!(!verify_file_path_signature_with(
                Some(SECRET),
                PATH,
                exp,
                sig,
                NOW
            ));
        }
    }

    #[test]
    fn short_or_missing_secret() {
        let (exp, sig) = signed();
        assert!(!verify_file_path_signature_with(
            Some("short"),
            PATH,
            Some(&exp),
            Some(&sig),
            NOW
        ));
        assert!(!verify_file_path_signature_with(
            None,
            PATH,
            Some(&exp),
            Some(&sig),
            NOW
        ));
        assert_eq!(
            sign_file_path_with(Some("short"), PATH, NOW, 60),
            Err(FileUrlSecretMissingError)
        );
        assert_eq!(
            sign_file_path_with(None, PATH, NOW, 60),
            Err(FileUrlSecretMissingError)
        );
    }

    #[test]
    fn unauthenticated_headers() {
        let mut h = HeaderMap::new();
        set_unauthenticated_file_headers(&mut h);
        assert_eq!(
            h.get("referrer-policy").and_then(|v| v.to_str().ok()),
            Some("no-referrer")
        );
        assert_eq!(
            h.get("x-robots-tag").and_then(|v| v.to_str().ok()),
            Some("noindex")
        );
    }
}
