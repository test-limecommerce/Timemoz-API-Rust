//! `FileInterceptor('file', { limits: { fileSize } })` + `ImportFileExceptionFilter` (multer di
//! Nest) untuk impor CSV. Bukan multipart → tidak ada berkas (multer melewatkan request itu).

use axum::extract::{FromRequest, Multipart, Request};
use axum::http::StatusCode;

use crate::ApiError;

/// Batas nilai field teks multer (`fieldSize` bawaan 1 MB).
const MAX_FIELD_BYTES: usize = 1024 * 1024;

/// `Express.Multer.File` (`originalname`, `mimetype`, `buffer`).
#[derive(Debug, Clone)]
pub struct Upload {
    pub original_name: String,
    pub mime: String,
    pub bytes: Vec<u8>,
}

/// Windows/browser kirim vnd.ms-excel, curl kirim octet-stream; isi tetap divalidasi parser.
const CSV_MIMETYPES: [&str; 5] = [
    "text/csv",
    "application/csv",
    "text/plain",
    "application/vnd.ms-excel",
    "application/octet-stream",
];

/// `invalidFile(message)`: 422 `{code:'IMPORT_INVALID_FILE', message}`.
pub fn invalid_file(message: impl Into<String>) -> ApiError {
    ApiError::coded(
        StatusCode::UNPROCESSABLE_ENTITY,
        "IMPORT_INVALID_FILE",
        message,
    )
}

/// `ImportFileExceptionFilter(label)`: 413 `{code:'IMPORT_FILE_TOO_LARGE', message}`.
pub fn file_too_large(limit_label: &str) -> ApiError {
    ApiError::coded(
        StatusCode::PAYLOAD_TOO_LARGE,
        "IMPORT_FILE_TOO_LARGE",
        format!("File exceeds the {limit_label} limit"),
    )
}

/// `assertCsvFile(file)` / `assertBalanceCsvFile(file)`.
pub fn assert_csv_file(file: Option<&Upload>) -> Result<(), ApiError> {
    let Some(file) = file.filter(|f| !f.bytes.is_empty()) else {
        return Err(invalid_file("No file uploaded"));
    };
    if !file.original_name.to_ascii_lowercase().ends_with(".csv") {
        return Err(invalid_file("File must have a .csv extension"));
    }
    if !CSV_MIMETYPES.contains(&file.mime.to_lowercase().as_str()) {
        return Err(invalid_file(
            "File type is not supported, upload a CSV file",
        ));
    }
    Ok(())
}

fn is_multipart(req: &Request) -> bool {
    req.headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.to_ascii_lowercase().starts_with("multipart/form-data"))
}

/// `file.originalname`: multer membuang direktori dari nama berkas.
fn base_name(name: &str) -> String {
    name.rsplit(['/', '\\']).next().unwrap_or(name).to_owned()
}

/// `@UploadedFile()` untuk field `file`. Berkas lain / berkas kedua → 400 `Unexpected field`;
/// lebih dari `max_bytes` → 413 `IMPORT_FILE_TOO_LARGE` (`File exceeds the {limit_label}
/// limit`); nama berkas kosong = tanpa berkas.
pub async fn read_upload(
    req: Request,
    max_bytes: usize,
    limit_label: &str,
) -> Result<Option<Upload>, ApiError> {
    if !is_multipart(&req) {
        return Ok(None);
    }
    // Galat multer yang bukan `MulterError` (mis. multipart rusak) = `Error` biasa → 500.
    let broken = |e: &axum::extract::multipart::MultipartError| -> ApiError {
        if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
            return file_too_large(limit_label);
        }
        ApiError::internal(anyhow::anyhow!("multipart: {}", e.body_text()))
    };
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
                    if bytes.len() + chunk.len() > max_bytes {
                        return Err(file_too_large(limit_label));
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
