//! Bentuk galat modul form: `new XxxException({ code, message, details? })` → body apa adanya.

use axum::http::StatusCode;
use mz_core::ApiError;
use serde_json::{Value, json};

/// `{ code, message }`.
pub fn coded(status: StatusCode, code: &str, message: impl Into<String>) -> ApiError {
    ApiError::coded(status, code, message)
}

/// `{ code, message, details }`.
pub fn coded_details(
    status: StatusCode,
    code: &str,
    message: impl Into<String>,
    details: Value,
) -> ApiError {
    ApiError::body(
        status,
        json!({ "code": code, "message": message.into(), "details": details }),
    )
}

pub fn not_found(code: &str, message: impl Into<String>) -> ApiError {
    coded(StatusCode::NOT_FOUND, code, message)
}

pub fn conflict(code: &str, message: impl Into<String>) -> ApiError {
    coded(StatusCode::CONFLICT, code, message)
}

pub fn unprocessable(code: &str, message: impl Into<String>) -> ApiError {
    coded(StatusCode::UNPROCESSABLE_ENTITY, code, message)
}

pub fn unprocessable_details(code: &str, message: impl Into<String>, details: Value) -> ApiError {
    coded_details(StatusCode::UNPROCESSABLE_ENTITY, code, message, details)
}
