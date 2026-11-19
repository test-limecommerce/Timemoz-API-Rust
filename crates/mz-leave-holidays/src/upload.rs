//! `FileInterceptor('file', { limits: { fileSize: 256 KB } })` + `HOLIDAY_IMPORT_FILE_FILTER`
//! (multer di Nest). Bukan multipart → tidak ada berkas (multer melewatkan request itu).

use axum::extract::{FromRequest, Multipart, Request};
use axum::http::StatusCode;
use mz_core::ApiError;

use crate::import::{MAX_IMPORT_BYTES, Upload, file_too_large};

/// Batas nilai field teks multer (`fieldSize` bawaan 1 MB).
const MAX_FIELD_BYTES: usize = 1024 * 1024;

fn is_multipart(req: &Request) -> bool {
    req.headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.to_ascii_lowercase().starts_with("multipart/form-data"))
}

/// Galat multer yang bukan `MulterError` (mis. multipart rusak) = `Error` biasa → 500.
fn broken(e: &axum::extract::multipart::MultipartError) -> ApiError {
    if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
        return file_too_large();
    }
    ApiError::internal(anyhow::anyhow!("multipart: {}", e.body_text()))
}

/// `file.originalname`: multer membuang direktori dari nama berkas.
fn base_name(name: &str) -> String {
    name.rsplit(['/', '\\']).next().unwrap_or(name).to_owned()
}

/// `@UploadedFile()` untuk field `file`. Berkas lain / berkas kedua → 400 `Unexpected field`;
/// lebih dari 256 KB → 413 `IMPORT_FILE_TOO_LARGE`; nama berkas kosong = tanpa berkas.
pub async fn read_upload(req: Request) -> Result<Option<Upload>, ApiError> {
    if !is_multipart(&req) {
        return Ok(None);
    }
    let mut mp = Multipart::from_request(req, &())
        .await
        .map_err(|e| ApiError::internal(anyhow::anyhow!("multipart: {}", e.body_text())))?;
    let mut upload: Option<Upload> = None;
    while let Some(mut field) = mp.next_field().await.map_err(|e| broken(&e))? {
        let name = field.name().unwrap_or_default().to_owned();
        let file_name = field.file_name().map(str::to_owned);
        match file_name {
            Some(file_name) => {
                let mime = field
                    .content_type()
                    .filter(|m| !m.is_empty())
                    .unwrap_or("text/plain")
                    .to_owned();
                if file_name.is_empty() {
                    // multer: bagian tanpa nama berkas dibuang.
                    while field.chunk().await.map_err(|e| broken(&e))?.is_some() {}
                    continue;
                }
                if name != "file" || upload.is_some() {
                    return Err(ApiError::bad_request("Unexpected field"));
                }
                let mut bytes = Vec::new();
                while let Some(chunk) = field.chunk().await.map_err(|e| broken(&e))? {
                    if bytes.len() + chunk.len() > MAX_IMPORT_BYTES {
                        return Err(file_too_large());
                    }
                    bytes.extend_from_slice(&chunk);
                }
                upload = Some(Upload {
                    original_name: base_name(&file_name),
                    mime,
                    bytes,
                });
            }
            None => {
                let mut size = 0usize;
                while let Some(chunk) = field.chunk().await.map_err(|e| broken(&e))? {
                    size += chunk.len();
                    if size > MAX_FIELD_BYTES {
                        return Err(ApiError::bad_request("Field value too long"));
                    }
                }
            }
        }
    }
    Ok(upload)
}
