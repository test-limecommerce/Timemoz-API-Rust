//! Body `POST /mobile/leaves` dan `PUT /mobile/leaves/:uuid/revise`:
//! `FileInterceptor('document')` (multer 2 / busboy) + body-parser express.
//!
//! - `multipart/form-data`: field teks → `req.body` (nama kembar → array, `append-field`),
//!   berkas `document` → `@UploadedFile()`. Berkas di field lain / berkas kedua →
//!   400 `Unexpected field` (`LIMIT_UNEXPECTED_FILE`).
//! - Selain itu body JSON / urlencoded (100 kB) seperti express; tanpa berkas.
//!
//! DUPLIKAT (diadaptasi) dari mz_attendance::body::ClockForm — sengaja dibiarkan (dicek 2026-11-18):
//! deteksi part berkas berbeda (di sini part `application/octet-stream` tanpa `filename` juga
//! berkas, mime bawaan `text/plain`, `originalname` disimpan; di absensi hanya part ber-`filename`,
//! mime bawaan `application/octet-stream`).

use axum::extract::{FromRequest, Multipart, Request};
use mz_core::ApiError;
use serde_json::{Map, Value};

/// Batas `express.json` / `express.urlencoded` bawaan `main.ts`.
pub const JSON_LIMIT: usize = 100 * 1024;

/// `Express.Multer.File` yang dipakai service.
#[derive(Debug, Clone)]
pub struct Document {
    pub bytes: Vec<u8>,
    /// `mimetype` (busboy: bawaan `text/plain` bila part tanpa `Content-Type`).
    pub mime: String,
    /// `originalname`.
    pub original_name: String,
}

impl Document {
    /// `document.originalname.split('.').pop() ?? 'pdf'`: segmen sesudah titik terakhir
    /// (tanpa titik = seluruh nama).
    pub fn ext(&self) -> &str {
        self.original_name.rsplit('.').next().unwrap_or("pdf")
    }
}

/// `req.body` + `@UploadedFile() document`.
#[derive(Debug, Default)]
pub struct LeaveForm {
    pub fields: Map<String, Value>,
    pub document: Option<Document>,
}

fn content_type(req: &Request) -> String {
    req.headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase()
}

/// Body JSON/urlencoded ala express (non-JSON → `{}`; array → `{}` seperti `ValidationPipe`).
pub async fn read_plain_body(req: Request) -> Result<Map<String, Value>, ApiError> {
    let ct = content_type(&req);
    let is_json = ct.starts_with("application/json") || ct.contains("+json");
    let is_form = ct.starts_with("application/x-www-form-urlencoded");
    let bytes = axum::body::Bytes::from_request(req, &())
        .await
        .map_err(mz_core::extract::bytes_rejection)?;
    if (is_json || is_form) && bytes.len() > JSON_LIMIT {
        return Err(mz_core::extract::payload_too_large());
    }
    if is_form {
        return Ok(crate::dto::query_object(Some(&String::from_utf8_lossy(
            &bytes,
        ))));
    }
    if !is_json || bytes.is_empty() {
        return Ok(Map::new());
    }
    let value: Value = serde_json::from_slice(&bytes).map_err(|e| {
        let s = e.to_string();
        ApiError::BadJson(s.split(" at line ").next().unwrap_or(&s).to_owned())
    })?;
    match value {
        Value::Object(m) => Ok(m),
        Value::Array(_) => Ok(Map::new()),
        other => Err(ApiError::BadJson(format!(
            "Unexpected token in JSON: {other}"
        ))),
    }
}

impl<S: Send + Sync> FromRequest<S> for LeaveForm {
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        if !content_type(&req).starts_with("multipart/form-data") {
            return Ok(Self {
                fields: read_plain_body(req).await?,
                document: None,
            });
        }
        let mut mp = Multipart::from_request(req, state)
            .await
            .map_err(|e| ApiError::message(e.status(), e.body_text()))?;
        let mut form = Self::default();
        let mp_err = |e: axum::extract::multipart::MultipartError| {
            ApiError::message(e.status(), e.body_text())
        };
        while let Some(field) = mp.next_field().await.map_err(mp_err)? {
            let name = field.name().unwrap_or_default().to_owned();
            let mime = field
                .content_type()
                .filter(|m| !m.is_empty())
                .map(str::to_owned);
            // busboy: part berkas = punya `filename` ATAU bertipe application/octet-stream.
            let is_file =
                field.file_name().is_some() || mime.as_deref() == Some("application/octet-stream");
            if is_file {
                if name != "document" || form.document.is_some() {
                    return Err(ApiError::bad_request("Unexpected field"));
                }
                let original_name = field.file_name().unwrap_or_default().to_owned();
                let bytes = field.bytes().await.map_err(mp_err)?.to_vec();
                form.document = Some(Document {
                    bytes,
                    mime: mime.unwrap_or_else(|| "text/plain".to_owned()),
                    original_name,
                });
            } else {
                let text = field.text().await.map_err(mp_err)?;
                match form.fields.get_mut(&name) {
                    Some(Value::Array(items)) => items.push(Value::String(text)),
                    Some(existing) => {
                        let prev = existing.take();
                        *existing = Value::Array(vec![prev, Value::String(text)]);
                    }
                    None => {
                        form.fields.insert(name, Value::String(text));
                    }
                }
            }
        }
        Ok(form)
    }
}

/// Body JSON polos (`@Body()` route review): `{}` bila bukan JSON.
#[derive(Debug, Default)]
pub struct JsonObject(pub Map<String, Value>);

impl<S: Send + Sync> FromRequest<S> for JsonObject {
    type Rejection = ApiError;

    async fn from_request(req: Request, _state: &S) -> Result<Self, Self::Rejection> {
        let ct = content_type(&req);
        // Route review tidak memasang multer: multipart = body kosong (`{}`).
        if ct.starts_with("multipart/form-data") {
            return Ok(Self(Map::new()));
        }
        read_plain_body(req).await.map(Self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ext_like_split_pop() {
        let doc = |n: &str| Document {
            bytes: Vec::new(),
            mime: String::new(),
            original_name: n.to_owned(),
        };
        assert_eq!(doc("surat.dokter.PDF").ext(), "PDF");
        assert_eq!(doc("surat").ext(), "surat");
        assert_eq!(doc("").ext(), "");
    }
}
