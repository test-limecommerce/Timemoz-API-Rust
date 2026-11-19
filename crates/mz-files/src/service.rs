//! Port `src/files/files.service.ts` (`FilesService`) di atas klien GCS JSON API.
//!
//! Semua fungsi memakai bucket `GCP_BUCKET_NAME` kecuali parameter `bucket` diberikan.
//! Galat GCS (`GcsError`) dikonversi otomatis ke `ApiError` 500 lewat `?`.

use std::path::Path;

use axum::body::Body;
use axum::response::Response;
use futures::TryStreamExt;
use http::header::{
    ACCEPT_RANGES, CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE,
    CONTENT_SECURITY_POLICY, CONTENT_TYPE, ETAG, HeaderName, HeaderValue, IF_NONE_MATCH, IF_RANGE,
    RANGE, X_CONTENT_TYPE_OPTIONS,
};
use http::{HeaderMap, StatusCode};
use mz_core::{ApiError, ApiResult, AppState};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt as _;
use tokio_util::io::ReaderStream;

use crate::file_response::{
    ByteRange, DEFAULT_PRIVATE_CACHE_CONTROL, DispositionType, RangeParse, content_disposition,
    etag_for, if_none_match_hits, parse_range, safe_content_type,
};
use crate::gcs::{
    GcsError, TRANSFER_TIMEOUT, UrlRequest, default_bucket, object_url_in, other, resolve_bucket,
    sign_url, storage_auth, upload_url_in,
};
use crate::object_key::{InvalidObjectKeyError, to_object_key};
use crate::signed_read_url::{SignedReadUrl, signed_read_window};
use crate::signer::SignedAction;

/// `ObjectInfo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectInfo {
    pub key: String,
    pub content_type: Option<String>,
    pub size: u64,
    pub generation: String,
    pub etag: Option<String>,
}

#[derive(Deserialize)]
struct RawMetadata {
    #[serde(rename = "contentType")]
    content_type: Option<String>,
    size: Option<serde_json::Value>,
    generation: Option<serde_json::Value>,
    etag: Option<String>,
}

fn json_number(v: Option<&serde_json::Value>) -> Option<u64> {
    match v? {
        serde_json::Value::String(s) => s.parse().ok(),
        serde_json::Value::Number(n) => n.as_u64(),
        _ => None,
    }
}

fn json_string(v: Option<&serde_json::Value>) -> String {
    match v {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

/// Opsi `streamObject`.
#[derive(Debug, Clone, Default)]
pub struct StreamObjectOptions {
    /// Default `private, max-age=300`.
    pub cache_control: Option<String>,
    /// Nama file untuk `Content-Disposition`.
    pub filename: Option<String>,
    /// Paksa `attachment` walau tipenya boleh inline.
    pub force_attachment: bool,
    /// Hasil [`get_object_info`] yang sudah diambil caller.
    pub info: Option<ObjectInfo>,
}

/// Respons `getSignedFileUrl`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SignedFileUrl {
    pub success: bool,
    pub message: String,
    pub url: String,
}

/// Respons `deleteFile`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DeleteResult {
    pub success: bool,
    pub message: String,
}

/// Respons `getObjectMetadata`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectMetadata {
    pub exists: bool,
    pub size_bytes: Option<u64>,
}

/// Opsi `getSignedObjectUrl`.
#[derive(Debug, Clone, Default)]
pub struct SignedObjectOpts<'a> {
    /// Node `opts.contentType`: `None` = `undefined` (pakai metadata object);
    /// `Some(None)` = `null`; `Some(Some(t))` = tipe dari klien. Bila `Some`,
    /// tipe disaring `safe_content_type` dan ikut ditandatangani sebagai
    /// `response-content-type`/`response-content-disposition`.
    pub content_type: Option<Option<&'a str>>,
    pub filename: Option<&'a str>,
}

/// `FilesService.toObjectKey` dengan `GCP_BUCKET_NAME` dari konfigurasi.
pub fn object_key_of(state: &AppState, value: &str) -> Result<String, InvalidObjectKeyError> {
    to_object_key(value, state.config.var("GCP_BUCKET_NAME").as_deref())
}

/// `getSignedFileUrl(key, expiresIn, contentType)`: signed URL tulis (PUT) V4.
pub async fn get_signed_file_url(
    state: &AppState,
    key: &str,
    expires_in_secs: i64,
    content_type: &str,
) -> Result<SignedFileUrl, GcsError> {
    let bucket = default_bucket(state)?;
    let now = chrono::Utc::now().timestamp_millis();
    // Node: `accessibleAt` default = sekarang (dibulatkan ke detik di signer).
    let url = sign_url(
        state,
        &UrlRequest {
            action: SignedAction::Write,
            bucket: &bucket,
            key,
            accessible_at_ms: now,
            expires_ms: now + expires_in_secs * 1000,
            content_type: Some(content_type),
            response_type: None,
            response_disposition: None,
        },
    )
    .await?;
    Ok(SignedFileUrl {
        success: true,
        message: "success get presigned url".to_owned(),
        url,
    })
}

/// `getSignedObjectUrl(key, opts, nowMs)`: signed URL baca ber-jendela 10 menit.
pub async fn get_signed_object_url(
    state: &AppState,
    key: &str,
    opts: &SignedObjectOpts<'_>,
    now_ms: i64,
) -> Result<SignedReadUrl, GcsError> {
    let (accessible_at, expires) = signed_read_window(now_ms);
    let bucket = default_bucket(state)?;
    let (response_type, response_disposition) = match opts.content_type {
        Some(raw) => {
            let safe = safe_content_type(raw);
            let kind = if safe.inline {
                DispositionType::Inline
            } else {
                DispositionType::Attachment
            };
            (
                Some(safe.content_type),
                Some(content_disposition(kind, opts.filename)),
            )
        }
        None => (None, None),
    };
    let url = sign_url(
        state,
        &UrlRequest {
            action: SignedAction::Read,
            bucket: &bucket,
            key,
            accessible_at_ms: accessible_at,
            expires_ms: expires,
            content_type: None,
            response_type: response_type.as_deref(),
            response_disposition: response_disposition.as_deref(),
        },
    )
    .await?;
    let expires_at = chrono::DateTime::from_timestamp_millis(expires)
        .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .ok_or_else(|| other("invalid expiry"))?;
    Ok(SignedReadUrl { url, expires_at })
}

/// `getSignedReadUrl(key, expiresIn, bucket)`: signed URL baca (berlaku `expires_in_secs`).
pub async fn get_signed_read_url(
    state: &AppState,
    key: &str,
    expires_in_secs: i64,
    bucket: Option<&str>,
) -> Result<String, GcsError> {
    let bucket = resolve_bucket(state, bucket)?;
    let now = chrono::Utc::now().timestamp_millis();
    sign_url(
        state,
        &UrlRequest {
            action: SignedAction::Read,
            bucket: &bucket,
            key,
            accessible_at_ms: now,
            expires_ms: now + expires_in_secs * 1000,
            content_type: None,
            response_type: None,
            response_disposition: None,
        },
    )
    .await
}

async fn metadata(
    state: &AppState,
    bucket: &str,
    key: &str,
) -> Result<Option<RawMetadata>, GcsError> {
    let auth = storage_auth(state).await?;
    let resp = state
        .http
        .get(object_url_in(state, bucket, key))
        .headers(auth)
        .send()
        .await
        .map_err(other)?;
    if resp.status() == StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !resp.status().is_success() {
        return Err(other(format!("gcs metadata failed: {}", resp.status())));
    }
    Ok(Some(resp.json::<RawMetadata>().await.map_err(other)?))
}

/// `getObjectMetadata(key)`: eksistensi + ukuran dalam satu panggilan.
pub async fn get_object_metadata(state: &AppState, key: &str) -> Result<ObjectMetadata, GcsError> {
    let bucket = default_bucket(state)?;
    Ok(match metadata(state, &bucket, key).await? {
        Some(m) => ObjectMetadata {
            exists: true,
            size_bytes: json_number(m.size.as_ref()),
        },
        None => ObjectMetadata {
            exists: false,
            size_bytes: None,
        },
    })
}

/// `getObjectInfo(key)`: `None` bila object tidak ada.
pub async fn get_object_info(state: &AppState, key: &str) -> Result<Option<ObjectInfo>, GcsError> {
    let bucket = default_bucket(state)?;
    Ok(metadata(state, &bucket, key).await?.map(|m| ObjectInfo {
        key: key.to_owned(),
        content_type: m.content_type,
        size: json_number(m.size.as_ref()).unwrap_or(0),
        generation: json_string(m.generation.as_ref()),
        etag: m.etag,
    }))
}

/// `deleteFile(key)`. Object tidak ada → `GcsError::NotFound` (di Node: galat → 500).
pub async fn delete_file(state: &AppState, key: &str) -> Result<DeleteResult, GcsError> {
    let auth = storage_auth(state).await?;
    let resp = state
        .http
        .delete(object_url_in(state, &default_bucket(state)?, key))
        .headers(auth)
        .send()
        .await
        .map_err(other)?;
    if resp.status() == StatusCode::NOT_FOUND {
        return Err(GcsError::NotFound);
    }
    if !resp.status().is_success() {
        return Err(other(format!("gcs delete failed: {}", resp.status())));
    }
    Ok(DeleteResult {
        success: true,
        message: "file deleted successfully".to_owned(),
    })
}

/// `uploadBuffer(key, buffer, mimeType)` (upload sederhana, tidak resumable).
pub async fn upload_buffer(
    state: &AppState,
    key: &str,
    data: impl Into<reqwest::Body>,
    mime_type: &str,
) -> Result<(), GcsError> {
    let auth = storage_auth(state).await?;
    let resp = state
        .http
        .post(upload_url_in(state, &default_bucket(state)?, key))
        .headers(auth)
        .header(CONTENT_TYPE, mime_type)
        .timeout(TRANSFER_TIMEOUT)
        .body(data)
        .send()
        .await
        .map_err(other)?;
    if !resp.status().is_success() {
        return Err(other(format!("gcs upload failed: {}", resp.status())));
    }
    Ok(())
}

/// `uploadLocalFile(localPath, key, { bucket, contentType })`: di-stream dari disk.
pub async fn upload_local_file(
    state: &AppState,
    local_path: &Path,
    key: &str,
    bucket: &str,
    content_type: Option<&str>,
) -> Result<(), GcsError> {
    let file = tokio::fs::File::open(local_path).await.map_err(other)?;
    let len = file.metadata().await.map_err(other)?.len();
    let auth = storage_auth(state).await?;
    let resp = state
        .http
        .post(upload_url_in(state, bucket, key))
        .headers(auth)
        .header(
            CONTENT_TYPE,
            content_type.unwrap_or("application/octet-stream"),
        )
        .header(CONTENT_LENGTH, len)
        .timeout(TRANSFER_TIMEOUT)
        .body(reqwest::Body::wrap_stream(ReaderStream::new(file)))
        .send()
        .await
        .map_err(other)?;
    if !resp.status().is_success() {
        return Err(other(format!("gcs upload failed: {}", resp.status())));
    }
    Ok(())
}

async fn get_media(
    state: &AppState,
    bucket: &str,
    key: &str,
    range: Option<ByteRange>,
) -> Result<reqwest::Response, GcsError> {
    let auth = storage_auth(state).await?;
    let mut req = state
        .http
        .get(format!("{}?alt=media", object_url_in(state, bucket, key)))
        .headers(auth)
        .timeout(TRANSFER_TIMEOUT);
    if let Some(r) = range {
        req = req.header(RANGE, format!("bytes={}-{}", r.start, r.end));
    }
    let resp = req.send().await.map_err(other)?;
    if resp.status() == StatusCode::NOT_FOUND {
        return Err(GcsError::NotFound);
    }
    if !resp.status().is_success() {
        return Err(other(format!("gcs download failed: {}", resp.status())));
    }
    Ok(resp)
}

/// `downloadBuffer(key)`: seluruh object ke memori (untuk proxy endpoint sensitif).
pub async fn download_buffer(state: &AppState, key: &str) -> Result<Vec<u8>, GcsError> {
    let resp = get_media(state, &default_bucket(state)?, key, None).await?;
    resp.bytes().await.map(|b| b.to_vec()).map_err(other)
}

/// `downloadToFile(key, destination, { bucket })`: di-stream ke disk.
pub async fn download_to_file(
    state: &AppState,
    key: &str,
    destination: &Path,
    bucket: &str,
) -> Result<(), GcsError> {
    let resp = get_media(state, bucket, key, None).await?;
    let mut file = tokio::fs::File::create(destination).await.map_err(other)?;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.try_next().await.map_err(other)? {
        file.write_all(&chunk).await.map_err(other)?;
    }
    file.flush().await.map_err(other)?;
    Ok(())
}

// ─── streamObject ──────────────────────────────────────────────────────────────

/// Keputusan respons sebelum GCS dipanggil (dipisah supaya bisa dites tanpa GCS).
#[derive(Debug)]
pub(crate) enum Plan {
    /// 304 / 416: hanya header.
    Empty {
        status: StatusCode,
        headers: HeaderMap,
    },
    /// 200 / 206: header + byte dari GCS (seluruhnya, atau `range`).
    Body {
        status: StatusCode,
        headers: HeaderMap,
        range: Option<ByteRange>,
    },
}

fn header_str(h: Option<&HeaderMap>, name: HeaderName) -> Option<&str> {
    h?.get(name)?.to_str().ok()
}

fn set(h: &mut HeaderMap, name: HeaderName, value: &str) {
    if let Ok(v) = HeaderValue::from_str(value) {
        h.insert(name, v);
    }
}

pub(crate) fn plan_response(
    info: &ObjectInfo,
    opts: &StreamObjectOptions,
    req: Option<&HeaderMap>,
) -> Plan {
    let safe = safe_content_type(info.content_type.as_deref());
    let kind = if safe.inline && !opts.force_attachment {
        DispositionType::Inline
    } else {
        DispositionType::Attachment
    };
    let etag = (!info.generation.is_empty()).then(|| etag_for(&info.generation));

    let mut headers = HeaderMap::new();
    set(&mut headers, CONTENT_TYPE, &safe.content_type);
    let disposition = content_disposition(kind, opts.filename.as_deref());
    set(&mut headers, CONTENT_DISPOSITION, &disposition);
    set(&mut headers, X_CONTENT_TYPE_OPTIONS, "nosniff");
    set(&mut headers, CONTENT_SECURITY_POLICY, "sandbox");
    set(
        &mut headers,
        CACHE_CONTROL,
        opts.cache_control
            .as_deref()
            .unwrap_or(DEFAULT_PRIVATE_CACHE_CONTROL),
    );
    set(&mut headers, ACCEPT_RANGES, "bytes");
    if let Some(e) = &etag {
        set(&mut headers, ETAG, e);
    }

    if let Some(e) = &etag
        && if_none_match_hits(header_str(req, IF_NONE_MATCH), e)
    {
        return Plan::Empty {
            status: StatusCode::NOT_MODIFIED,
            headers,
        };
    }

    // `If-Range` yang tidak cocok = file sudah berubah; kirim utuh.
    let if_range = header_str(req, IF_RANGE).filter(|v| !v.is_empty());
    let range_allowed = match if_range {
        None => true,
        Some(v) => etag.as_deref() == Some(v),
    };
    let range = if range_allowed {
        parse_range(header_str(req, RANGE), info.size)
    } else {
        RangeParse::Ignore
    };

    match range {
        RangeParse::Unsatisfiable => {
            set(
                &mut headers,
                CONTENT_RANGE,
                &format!("bytes */{}", info.size),
            );
            headers.remove(CONTENT_DISPOSITION);
            Plan::Empty {
                status: StatusCode::RANGE_NOT_SATISFIABLE,
                headers,
            }
        }
        RangeParse::Range(r) => {
            set(
                &mut headers,
                CONTENT_RANGE,
                &format!("bytes {}-{}/{}", r.start, r.end, info.size),
            );
            set(
                &mut headers,
                CONTENT_LENGTH,
                &(r.end - r.start + 1).to_string(),
            );
            Plan::Body {
                status: StatusCode::PARTIAL_CONTENT,
                headers,
                range: Some(r),
            }
        }
        RangeParse::Ignore => {
            set(&mut headers, CONTENT_LENGTH, &info.size.to_string());
            Plan::Body {
                status: StatusCode::OK,
                headers,
                range: None,
            }
        }
    }
}

fn into_response(status: StatusCode, headers: HeaderMap, body: Body) -> Response {
    let mut resp = Response::new(body);
    *resp.status_mut() = status;
    *resp.headers_mut() = headers;
    resp
}

/// `streamObject(key, res, opts)`: stream object ke klien tanpa memuat seluruh file.
///
/// - Object tidak ada → `404 {"message":"File not found",...}` (sebelum header apa pun).
/// - `If-None-Match` cocok → 304; `Range` satu rentang → 206 / 416.
/// - Galat sesudah byte pertama: koneksi diputus dan dicatat (`tracing::error!`).
///
/// Caller WAJIB sudah memeriksa hak akses dan menurunkan `key` dari DB.
/// `req_headers` = header request (`If-None-Match`, `Range`, `If-Range`); `None`
/// berarti selalu 200 utuh.
pub async fn stream_object(
    state: &AppState,
    key: &str,
    req_headers: Option<&HeaderMap>,
    opts: StreamObjectOptions,
) -> ApiResult<Response> {
    let info = match opts.info.clone() {
        Some(i) => i,
        None => get_object_info(state, key)
            .await?
            .ok_or_else(|| ApiError::not_found("File not found"))?,
    };

    match plan_response(&info, &opts, req_headers) {
        Plan::Empty { status, headers } => Ok(into_response(status, headers, Body::empty())),
        Plan::Body {
            status,
            headers,
            range,
        } => {
            // Header respons GCS diterima SEBELUM header kita dikirim, jadi 404
            // masih bisa dijawab sebagai JSON biasa.
            let bucket = default_bucket(state)?;
            let resp = match get_media(state, &bucket, key, range).await {
                Ok(r) => r,
                Err(GcsError::NotFound) => return Err(ApiError::not_found("File not found")),
                Err(e) => return Err(e.into()),
            };
            let owned_key = key.to_owned();
            let stream = resp.bytes_stream().inspect_err(move |e| {
                tracing::error!("Stream of {owned_key} failed mid-response: {e}");
            });
            Ok(into_response(status, headers, Body::from_stream(stream)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(ct: Option<&str>, size: u64, generation: &str) -> ObjectInfo {
        ObjectInfo {
            key: "k".into(),
            content_type: ct.map(str::to_owned),
            size,
            generation: generation.into(),
            etag: None,
        }
    }

    fn hdr(h: &HeaderMap, n: HeaderName) -> Option<String> {
        h.get(n).and_then(|v| v.to_str().ok()).map(str::to_owned)
    }

    fn req(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                HeaderName::from_static(k),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    #[test]
    fn full_response_headers() {
        let plan = plan_response(
            &info(Some("image/png"), 10, "99"),
            &StreamObjectOptions::default(),
            None,
        );
        let Plan::Body {
            status,
            headers,
            range,
        } = plan
        else {
            panic!("body")
        };
        assert_eq!(status, StatusCode::OK);
        assert!(range.is_none());
        assert_eq!(hdr(&headers, CONTENT_TYPE).as_deref(), Some("image/png"));
        assert_eq!(
            hdr(&headers, CONTENT_DISPOSITION).as_deref(),
            Some("inline")
        );
        assert_eq!(
            hdr(&headers, X_CONTENT_TYPE_OPTIONS).as_deref(),
            Some("nosniff")
        );
        assert_eq!(
            hdr(&headers, CONTENT_SECURITY_POLICY).as_deref(),
            Some("sandbox")
        );
        assert_eq!(
            hdr(&headers, CACHE_CONTROL).as_deref(),
            Some("private, max-age=300")
        );
        assert_eq!(hdr(&headers, ACCEPT_RANGES).as_deref(), Some("bytes"));
        assert_eq!(hdr(&headers, ETAG).as_deref(), Some("\"99\""));
        assert_eq!(hdr(&headers, CONTENT_LENGTH).as_deref(), Some("10"));
    }

    #[test]
    fn unsafe_type_forced_to_attachment_and_options_apply() {
        let opts = StreamObjectOptions {
            filename: Some("a.html".into()),
            cache_control: Some("private, max-age=86400".into()),
            ..Default::default()
        };
        let Plan::Body { headers, .. } =
            plan_response(&info(Some("text/html"), 1, ""), &opts, None)
        else {
            panic!("body")
        };
        assert_eq!(
            hdr(&headers, CONTENT_TYPE).as_deref(),
            Some("application/octet-stream")
        );
        assert_eq!(
            hdr(&headers, CONTENT_DISPOSITION).as_deref(),
            Some("attachment; filename=\"a.html\"; filename*=UTF-8''a.html")
        );
        assert_eq!(
            hdr(&headers, CACHE_CONTROL).as_deref(),
            Some("private, max-age=86400")
        );
        assert!(
            headers.get(ETAG).is_none(),
            "tanpa generation tidak ada ETag"
        );

        let forced = StreamObjectOptions {
            force_attachment: true,
            ..Default::default()
        };
        let Plan::Body { headers, .. } =
            plan_response(&info(Some("image/png"), 1, "1"), &forced, None)
        else {
            panic!("body")
        };
        assert_eq!(
            hdr(&headers, CONTENT_DISPOSITION).as_deref(),
            Some("attachment")
        );
    }

    #[test]
    fn if_none_match_gives_304_with_headers() {
        let r = req(&[("if-none-match", "\"99\"")]);
        let plan = plan_response(
            &info(Some("image/png"), 10, "99"),
            &StreamObjectOptions::default(),
            Some(&r),
        );
        let Plan::Empty { status, headers } = plan else {
            panic!("empty")
        };
        assert_eq!(status, StatusCode::NOT_MODIFIED);
        assert_eq!(hdr(&headers, ETAG).as_deref(), Some("\"99\""));
        assert_eq!(
            hdr(&headers, CACHE_CONTROL).as_deref(),
            Some("private, max-age=300")
        );
    }

    #[test]
    fn range_gives_206() {
        let r = req(&[("range", "bytes=2-4")]);
        let plan = plan_response(
            &info(Some("application/pdf"), 10, "7"),
            &StreamObjectOptions::default(),
            Some(&r),
        );
        let Plan::Body {
            status,
            headers,
            range,
        } = plan
        else {
            panic!("body")
        };
        assert_eq!(status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(range, Some(ByteRange { start: 2, end: 4 }));
        assert_eq!(
            hdr(&headers, CONTENT_RANGE).as_deref(),
            Some("bytes 2-4/10")
        );
        assert_eq!(hdr(&headers, CONTENT_LENGTH).as_deref(), Some("3"));
    }

    #[test]
    fn unsatisfiable_gives_416_without_disposition() {
        let r = req(&[("range", "bytes=10-")]);
        let plan = plan_response(
            &info(Some("image/png"), 10, "7"),
            &StreamObjectOptions::default(),
            Some(&r),
        );
        let Plan::Empty { status, headers } = plan else {
            panic!("empty")
        };
        assert_eq!(status, StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(hdr(&headers, CONTENT_RANGE).as_deref(), Some("bytes */10"));
        assert!(headers.get(CONTENT_DISPOSITION).is_none());
    }

    #[test]
    fn if_range_mismatch_sends_full_file() {
        let o = StreamObjectOptions::default();
        let i = info(Some("image/png"), 10, "7");
        let stale = req(&[("range", "bytes=2-4"), ("if-range", "\"6\"")]);
        let Plan::Body { status, range, .. } = plan_response(&i, &o, Some(&stale)) else {
            panic!("body")
        };
        assert_eq!((status, range), (StatusCode::OK, None));
        let fresh = req(&[("range", "bytes=2-4"), ("if-range", "\"7\"")]);
        let Plan::Body { status, .. } = plan_response(&i, &o, Some(&fresh)) else {
            panic!("body")
        };
        assert_eq!(status, StatusCode::PARTIAL_CONTENT);
        // Tanpa ETag (generation kosong), If-Range apa pun = utuh.
        let no_etag = info(Some("image/png"), 10, "");
        let Plan::Body { status, .. } = plan_response(&no_etag, &o, Some(&fresh)) else {
            panic!("body")
        };
        assert_eq!(status, StatusCode::OK);
    }

    #[test]
    fn raw_metadata_parsing() {
        let m: RawMetadata = serde_json::from_str(
            r#"{"contentType":"image/jpeg","size":"1234","generation":"1700000000000001","etag":"CAE="}"#,
        )
        .unwrap();
        assert_eq!(json_number(m.size.as_ref()), Some(1234));
        assert_eq!(json_string(m.generation.as_ref()), "1700000000000001");
        let m: RawMetadata = serde_json::from_str("{}").unwrap();
        assert_eq!(json_number(m.size.as_ref()), None);
        assert_eq!(json_string(m.generation.as_ref()), "");
    }

    #[test]
    fn dto_json_shapes() {
        let v = serde_json::to_value(DeleteResult {
            success: true,
            message: "file deleted successfully".into(),
        })
        .unwrap();
        assert_eq!(
            v,
            serde_json::json!({"success":true,"message":"file deleted successfully"})
        );
    }
}
