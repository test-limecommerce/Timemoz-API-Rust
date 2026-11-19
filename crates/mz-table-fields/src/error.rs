//! Galat handler crate ini = [`ApiError`] mz-core. Badan `HttpException({ code, message,
//! details })` Node dibangun dengan `json!` dalam urutan literal Node — urutan penyisipan
//! terjaga di kabel (workspace memakai `serde_json` dengan fitur `preserve_order`).

use mz_core::ApiError;

pub type ReqError = ApiError;

/// Untuk uji: status + badan JSON (teks, urutan kunci seperti di kabel).
#[cfg(test)]
pub(crate) fn parts(e: &ApiError) -> (http::StatusCode, String) {
    match e {
        ApiError::Body { status, body } => (*status, body.to_string()),
        other => (other.status(), String::new()),
    }
}
