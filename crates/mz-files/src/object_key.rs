//! Port `src/files/object-key.ts`: normalisasi referensi file GCS menjadi object key.
//!
//! Menerima URL public lama, URL ber-encode penuh (`%2F`), signed URL, `gs://`,
//! dan key mentah. URL host/bucket lain DITOLAK (cegah SSRF/IDOR).

use url::Url;

use crate::encode::decode_uri_component;

/// `InvalidObjectKeyError` — pesan `Invalid object key: <reason>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidObjectKeyError(pub String);

impl std::fmt::Display for InvalidObjectKeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Invalid object key: {}", self.0)
    }
}

impl std::error::Error for InvalidObjectKeyError {}

fn invalid(reason: &str) -> InvalidObjectKeyError {
    InvalidObjectKeyError(reason.to_owned())
}

/// Batas GCS untuk nama object: 1024 byte UTF-8.
const MAX_KEY_BYTES: usize = 1024;

/// `^[a-z][a-z0-9+.-]*:\/\//i`
fn has_url_scheme(s: &str) -> bool {
    let Some((scheme, _)) = s.split_once("://") else {
        return false;
    };
    let mut chars = scheme.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
}

fn assert_valid_key(key: String) -> Result<String, InvalidObjectKeyError> {
    if key.is_empty() {
        return Err(invalid("empty"));
    }
    if key.chars().any(|c| c <= '\u{1f}' || c == '\u{7f}') {
        return Err(invalid("contains control characters"));
    }
    if key.len() > MAX_KEY_BYTES {
        return Err(invalid("too long"));
    }
    Ok(key)
}

fn decode_key(encoded: &str) -> Result<String, InvalidObjectKeyError> {
    decode_uri_component(encoded).ok_or_else(|| invalid("malformed percent-encoding"))
}

fn key_from_url(value: &str, bucket: &str) -> Result<String, InvalidObjectKeyError> {
    let url = Url::parse(value).map_err(|_| invalid("malformed URL"))?;
    let host = url.host_str().unwrap_or("").to_lowercase();
    // `path()` masih ter-encode dan selalu diawali `/`.
    let path = url.path().strip_prefix('/').unwrap_or_else(|| url.path());

    if url.scheme() == "gs" {
        if host != bucket.to_lowercase() {
            return Err(invalid("foreign bucket"));
        }
        return decode_key(path);
    }

    if url.scheme() != "https" && url.scheme() != "http" {
        return Err(invalid("unsupported URL scheme"));
    }
    if !url.username().is_empty() || url.password().is_some() || url.port().is_some() {
        return Err(invalid("unexpected URL authority"));
    }

    if host == "storage.googleapis.com" || host == "storage.cloud.google.com" {
        let slash = match path.find('/') {
            Some(i) if i > 0 => i,
            _ => return Err(invalid("missing bucket or key")),
        };
        let (bucket_part, rest) = path.split_at(slash);
        if decode_key(bucket_part)? != bucket {
            return Err(invalid("foreign bucket"));
        }
        return decode_key(rest.strip_prefix('/').unwrap_or(rest));
    }

    if host == format!("{}.storage.googleapis.com", bucket.to_lowercase()) {
        return decode_key(path);
    }

    Err(invalid("foreign host"))
}

/// `toObjectKey(value, bucket)`. `bucket` = `GCP_BUCKET_NAME` (dibaca pemanggil).
pub fn to_object_key(value: &str, bucket: Option<&str>) -> Result<String, InvalidObjectKeyError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(invalid("empty"));
    }

    if has_url_scheme(trimmed) {
        let bucket = bucket
            .filter(|b| !b.is_empty())
            .ok_or_else(|| invalid("bucket is not configured"))?;
        return assert_valid_key(key_from_url(trimmed, bucket)?);
    }

    // Key mentah disimpan apa adanya (tidak di-decode).
    assert_valid_key(trimmed.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUCKET: &str = "mudah-prod";
    const KEY: &str = "screenshots/42/2026-09-24/abc.jpg";

    #[test]
    fn accepts_all_known_forms() {
        let cases = [
            KEY.to_owned(),
            format!("https://storage.googleapis.com/{BUCKET}/{KEY}"),
            format!(
                "https://storage.googleapis.com/{BUCKET}/{KEY}?X-Goog-Algorithm=GOOG4-RSA-SHA256&X-Goog-Signature=abc"
            ),
            format!("https://storage.googleapis.com/{BUCKET}/{KEY}#x"),
            format!("https://storage.cloud.google.com/{BUCKET}/{KEY}"),
            format!("https://{BUCKET}.storage.googleapis.com/{KEY}"),
            format!("gs://{BUCKET}/{KEY}"),
            format!("  https://storage.googleapis.com/{BUCKET}/{KEY}\n"),
            format!("https://STORAGE.googleapis.com/{BUCKET}/{KEY}"),
        ];
        for c in cases {
            assert_eq!(to_object_key(&c, Some(BUCKET)).as_deref(), Ok(KEY), "{c}");
        }
    }

    #[test]
    fn fully_encoded_and_escaped() {
        let enc = KEY.replace('/', "%2F");
        assert_eq!(
            to_object_key(
                &format!("https://storage.googleapis.com/{BUCKET}/{enc}"),
                Some(BUCKET)
            )
            .as_deref(),
            Ok(KEY)
        );
        assert_eq!(
            to_object_key(
                &format!("https://storage.googleapis.com/{BUCKET}/manual/a%20b.pdf"),
                Some(BUCKET)
            )
            .as_deref(),
            Ok("manual/a b.pdf")
        );
    }

    #[test]
    fn raw_key_is_not_decoded() {
        assert_eq!(
            to_object_key("manual/a%20b.pdf", Some(BUCKET)).as_deref(),
            Ok("manual/a%20b.pdf")
        );
    }

    #[test]
    fn rejects_foreign_and_malformed() {
        let cases = [
            String::new(),
            "   ".to_owned(),
            format!("https://storage.googleapis.com/other-bucket/{KEY}"),
            format!("https://storage.googleapis.com/{BUCKET}-backup/{KEY}"),
            format!("https://evil.storage.googleapis.com/{KEY}"),
            format!("https://example.com/{BUCKET}/{KEY}"),
            format!("https://storage.googleapis.com.evil.com/{BUCKET}/{KEY}"),
            format!("https://x@storage.googleapis.com/{BUCKET}/{KEY}"),
            format!("https://storage.googleapis.com:8443/{BUCKET}/{KEY}"),
            format!("gs://other/{KEY}"),
            format!("ftp://storage.googleapis.com/{BUCKET}/{KEY}"),
            "file:///etc/passwd".to_owned(),
            format!("https://storage.googleapis.com/{BUCKET}/"),
            "https://storage.googleapis.com/".to_owned(),
            format!("https://storage.googleapis.com/{BUCKET}/%E0%A4%A"),
            "screenshots/a\r\nb.jpg".to_owned(),
            format!("https://storage.googleapis.com/{BUCKET}/a%0Ab.jpg"),
            "a".repeat(1025),
        ];
        for c in cases {
            assert!(to_object_key(&c, Some(BUCKET)).is_err(), "{c:?}");
        }
    }

    #[test]
    fn url_without_configured_bucket() {
        let u = format!("https://storage.googleapis.com/{BUCKET}/{KEY}");
        assert!(to_object_key(&u, Some("")).is_err());
        assert!(to_object_key(&u, None).is_err());
    }

    #[test]
    fn error_message_matches_node() {
        assert_eq!(
            to_object_key("", None)
                .err()
                .map(|e| e.to_string())
                .as_deref(),
            Some("Invalid object key: empty")
        );
    }
}
