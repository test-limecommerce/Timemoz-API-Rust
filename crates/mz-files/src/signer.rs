//! Signed URL V4 GCS (`GOOG4-RSA-SHA256`), port `@google-cloud/storage`
//! `signer.js` (path-style, host `storage.googleapis.com`, payload `UNSIGNED-PAYLOAD`).
//!
//! Dibagi dua tahap supaya penandatangan bisa lokal (kunci service account) atau
//! jarak jauh (IAM `signBlob`): [`prepare_v4`] menghasilkan string-to-sign,
//! [`PreparedUrl::finish`] menempelkan tanda tangan heksadesimal.

use chrono::DateTime;
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use sha2::{Digest, Sha256};

use crate::encode::{fixed_encode_uri_component, gcs_encode_uri};

pub const GCS_HOST: &str = "storage.googleapis.com";
const SEVEN_DAYS_SECONDS: i64 = 604_800;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignError(pub String);

impl std::fmt::Display for SignError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SignError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignedAction {
    Read,
    Write,
}

impl SignedAction {
    fn method(self) -> &'static str {
        match self {
            Self::Read => "GET",
            Self::Write => "PUT",
        }
    }
}

/// Parameter penandatanganan; waktu dalam epoch ms (dibulatkan ke detik, seperti Node).
#[derive(Debug, Clone)]
pub struct SignParams<'a> {
    pub action: SignedAction,
    pub bucket: &'a str,
    pub key: &'a str,
    pub client_email: &'a str,
    pub accessible_at_ms: i64,
    pub expires_ms: i64,
    /// Ikut ditandatangani sebagai header `content-type` (wajib dikirim klien saat PUT).
    pub content_type: Option<&'a str>,
    pub response_type: Option<&'a str>,
    pub response_disposition: Option<&'a str>,
}

#[derive(Debug, Clone)]
pub struct PreparedUrl {
    pub string_to_sign: String,
    url_prefix: String,
}

impl PreparedUrl {
    /// URL akhir dengan `X-Goog-Signature=<hex>` di ujung.
    pub fn finish(self, signature: &[u8]) -> String {
        format!(
            "{}&X-Goog-Signature={}",
            self.url_prefix,
            hex::encode(signature)
        )
    }
}

/// `formatAsUTCISO`: `YYYYMMDD` atau `YYYYMMDDTHHMMSSZ`.
fn format_utc(secs: i64, with_time: bool) -> Result<String, SignError> {
    let dt =
        DateTime::from_timestamp(secs, 0).ok_or_else(|| SignError("invalid timestamp".into()))?;
    Ok(dt
        .format(if with_time {
            "%Y%m%dT%H%M%SZ"
        } else {
            "%Y%m%d"
        })
        .to_string())
}

/// `getCanonicalHeaders` untuk header yang sudah huruf kecil dan terurut.
fn canonical_header_value(v: &str) -> String {
    let mut out = String::new();
    let mut prev_space = 0usize;
    // `trim().replace(/\s{2,}/g, ' ')`
    let trimmed = v.trim();
    let mut run = String::new();
    for c in trimmed.chars() {
        if c.is_whitespace() {
            run.push(c);
            prev_space += 1;
        } else {
            flush_ws(&mut out, &mut run, &mut prev_space);
            out.push(c);
        }
    }
    flush_ws(&mut out, &mut run, &mut prev_space);
    out
}

fn flush_ws(out: &mut String, run: &mut String, count: &mut usize) {
    if *count >= 2 {
        out.push(' ');
    } else {
        out.push_str(run);
    }
    run.clear();
    *count = 0;
}

/// Tahap 1: rakit canonical request dan string-to-sign.
pub fn prepare_v4(p: &SignParams<'_>) -> Result<PreparedUrl, SignError> {
    let accessible_s = p.accessible_at_ms.div_euclid(1000);
    let expires_s = p.expires_ms.div_euclid(1000);
    if expires_s < accessible_s {
        return Err(SignError(
            "An expiration date cannot be before accessible date.".into(),
        ));
    }
    let period = expires_s - accessible_s;
    if period > SEVEN_DAYS_SECONDS {
        return Err(SignError(format!(
            "Max allowed expiration is seven days ({SEVEN_DAYS_SECONDS} seconds)."
        )));
    }

    // Header yang ditandatangani, urut abjad: content-type, host.
    let mut headers: Vec<(&str, String)> = vec![("host", GCS_HOST.to_owned())];
    if let Some(ct) = p.content_type.filter(|c| !c.is_empty()) {
        headers.push(("content-type", ct.to_owned()));
    }
    headers.sort_by(|a, b| a.0.cmp(b.0));
    let signed_headers = headers.iter().map(|h| h.0).collect::<Vec<_>>().join(";");
    let canonical_headers: String = headers
        .iter()
        .map(|(k, v)| format!("{k}:{}\n", canonical_header_value(v)))
        .collect();

    let datestamp = format_utc(accessible_s, false)?;
    let date_iso = format_utc(accessible_s, true)?;
    let scope = format!("{datestamp}/auto/storage/goog4_request");

    // Urutan penyisipan = urutan di URL akhir (Node `Object.assign`).
    let mut query: Vec<(String, String)> = vec![
        ("X-Goog-Algorithm".into(), "GOOG4-RSA-SHA256".into()),
        (
            "X-Goog-Credential".into(),
            format!("{}/{scope}", p.client_email),
        ),
        ("X-Goog-Date".into(), date_iso.clone()),
        ("X-Goog-Expires".into(), period.to_string()),
        ("X-Goog-SignedHeaders".into(), signed_headers.clone()),
    ];
    if let Some(t) = p.response_type {
        query.push(("response-content-type".into(), t.to_owned()));
    }
    if let Some(d) = p.response_disposition {
        query.push(("response-content-disposition".into(), d.to_owned()));
    }
    let encoded: Vec<(String, String)> = query
        .iter()
        .map(|(k, v)| (fixed_encode_uri_component(k), fixed_encode_uri_component(v)))
        .collect();
    let mut sorted = encoded.clone();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let canonical_query = sorted
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");

    let resource_path = format!("/{}/{}", p.bucket, gcs_encode_uri(p.key, false));
    let canonical_request = [
        p.action.method(),
        &resource_path,
        &canonical_query,
        &canonical_headers,
        &signed_headers,
        "UNSIGNED-PAYLOAD",
    ]
    .join("\n");
    let hash = hex::encode(Sha256::digest(canonical_request.as_bytes()));
    let string_to_sign = ["GOOG4-RSA-SHA256", &date_iso, &scope, &hash].join("\n");

    let qs = encoded
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");
    Ok(PreparedUrl {
        string_to_sign,
        url_prefix: format!("https://{GCS_HOST}{resource_path}?{qs}"),
    })
}

/// Kunci RSA service account (PKCS#8 `PRIVATE KEY` atau PKCS#1 `RSA PRIVATE KEY`).
pub struct RsaSigner(RsaKeyPair);

impl RsaSigner {
    pub fn from_pem(private_key: &str) -> Result<Self, SignError> {
        let parsed =
            pem::parse(private_key).map_err(|e| SignError(format!("bad private key: {e}")))?;
        let pair = match parsed.tag() {
            "PRIVATE KEY" => RsaKeyPair::from_pkcs8(parsed.contents()),
            "RSA PRIVATE KEY" => RsaKeyPair::from_der(parsed.contents()),
            other => return Err(SignError(format!("unsupported key type: {other}"))),
        }
        .map_err(|e| SignError(format!("bad private key: {e}")))?;
        Ok(Self(pair))
    }

    /// RSASSA-PKCS1-v1_5 + SHA-256 (deterministik).
    pub fn sign(&self, message: &[u8]) -> Result<Vec<u8>, SignError> {
        let mut sig = vec![0u8; self.0.public().modulus_len()];
        self.0
            .sign(&RSA_PKCS1_SHA256, &SystemRandom::new(), message, &mut sig)
            .map_err(|_| SignError("rsa signing failed".into()))?;
        Ok(sig)
    }

    #[cfg(test)]
    fn public_key_der(&self) -> Vec<u8> {
        self.0.public().as_ref().to_vec()
    }
}

#[cfg(test)]
pub(crate) mod test_vectors {
    //! Kunci RSA-2048 KHUSUS TEST (dibuat dengan `openssl genpkey`, bukan rahasia)
    //! dan URL keluaran `@google-cloud/storage` asli (Node 22, Storage dengan
    //! `credentials` ini) untuk objek/tanggal yang sama dengan test di bawah.
    pub const TEST_KEY_PEM: &str = include_str!("test_key.pem");
    pub const EMAIL: &str = "sa@proj.iam.gserviceaccount.com";
    pub const NODE_READ: &str = include_str!("vectors/read.txt");
    pub const NODE_PLAIN: &str = include_str!("vectors/plain.txt");
    pub const NODE_WRITE: &str = include_str!("vectors/write.txt");
}

#[cfg(test)]
mod tests {
    use super::test_vectors::*;
    use super::*;
    use ring::signature::{RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};

    // 2026-11-16T08:00:00Z
    const ACCESSIBLE: i64 = 1_794_816_000_000;
    const KEY: &str = "companies/1/shots/a b+(1)!'é.jpg";

    fn params(action: SignedAction) -> SignParams<'static> {
        SignParams {
            action,
            bucket: "my-bucket",
            key: KEY,
            client_email: EMAIL,
            accessible_at_ms: ACCESSIBLE,
            expires_ms: ACCESSIBLE + 25 * 60_000,
            content_type: None,
            response_type: None,
            response_disposition: None,
        }
    }

    fn signed(p: &SignParams<'_>) -> String {
        let signer = RsaSigner::from_pem(TEST_KEY_PEM).expect("key");
        let prepared = prepare_v4(p).expect("prepare");
        let sig = signer
            .sign(prepared.string_to_sign.as_bytes())
            .expect("sign");
        prepared.finish(&sig)
    }

    #[test]
    fn matches_node_plain_read() {
        assert_eq!(signed(&params(SignedAction::Read)), NODE_PLAIN.trim());
    }

    #[test]
    fn matches_node_read_with_response_overrides() {
        let mut p = params(SignedAction::Read);
        p.response_type = Some("image/png");
        p.response_disposition = Some("inline; filename=\"x.png\"; filename*=UTF-8''x.png");
        assert_eq!(signed(&p), NODE_READ.trim());
    }

    #[test]
    fn matches_node_write_with_content_type() {
        let p = SignParams {
            action: SignedAction::Write,
            bucket: "my-bucket",
            key: "up/x.jpg",
            client_email: EMAIL,
            accessible_at_ms: ACCESSIBLE,
            expires_ms: ACCESSIBLE + 900_000,
            content_type: Some("image/jpeg"),
            response_type: None,
            response_disposition: None,
        };
        assert_eq!(signed(&p), NODE_WRITE.trim());
    }

    #[test]
    fn signature_verifies_with_public_key() {
        let signer = RsaSigner::from_pem(TEST_KEY_PEM).expect("key");
        let msg = b"GOOG4-RSA-SHA256\n20261116T080000Z\nscope\nhash";
        let sig = signer.sign(msg).expect("sign");
        let pk = UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, signer.public_key_der());
        assert!(pk.verify(msg, &sig).is_ok());
        assert_eq!(
            sig,
            signer.sign(msg).expect("sign"),
            "PKCS1 v1.5 deterministik"
        );
    }

    #[test]
    fn rejects_bad_windows() {
        let mut p = params(SignedAction::Read);
        p.expires_ms = ACCESSIBLE - 1000;
        assert!(prepare_v4(&p).is_err());
        p.expires_ms = ACCESSIBLE + 8 * 24 * 3600 * 1000;
        assert!(prepare_v4(&p).is_err());
    }

    #[test]
    fn bad_key_is_error() {
        assert!(RsaSigner::from_pem("not a pem").is_err());
        assert!(
            RsaSigner::from_pem("-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n")
                .is_err()
        );
    }

    #[test]
    fn header_value_collapse() {
        assert_eq!(canonical_header_value("  a   b \t c  "), "a b c");
        assert_eq!(canonical_header_value("a b"), "a b");
    }
}
