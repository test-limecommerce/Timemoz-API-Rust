//! `ApiError` — menghasilkan body error yang SAMA PERSIS dengan NestJS.
//!
//! Bentuk yang dikirim Nest (dan sudah dipegang klien):
//!
//! | Kode Node                                   | Body                                                   |
//! |---------------------------------------------|--------------------------------------------------------|
//! | `new XxxException('pesan')`                 | `{"message":"pesan","error":"Forbidden","statusCode":403}` |
//! | `new XxxException()`                        | `{"message":"Forbidden","statusCode":403}`             |
//! | `new XxxException({ ...objek })`            | objeknya apa adanya                                    |
//! | `new HttpException('pesan', 402)`           | `{"statusCode":402,"message":"pesan"}`                 |
//! | ValidationPipe                              | `{"message":[...],"error":"Bad Request","statusCode":400}` |
//! | error tak tertangani                        | `{"statusCode":500,"message":"Internal server error"}` |
//! | Prisma P2002 (filter global)                | 409 `{"status":false,"message":"..."}`                  |
//!
//! Urutan kunci di kabel = urutan penyisipan `json!`/struct (workspace memakai `serde_json`
//! dengan fitur `preserve_order`, sama seperti objek JS), jadi literal di bawah ditulis
//! persis dalam urutan Node.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

pub type ApiResult<T> = Result<T, ApiError>;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// `new XxxException('pesan')`.
    #[error("{status}: {message}")]
    Message { status: StatusCode, message: String },
    /// `new XxxException()` tanpa argumen.
    #[error("{0}")]
    Bare(StatusCode),
    /// `new XxxException({...})` / `new HttpException({...}, status)`.
    #[error("{status}: {body}")]
    Body { status: StatusCode, body: Value },
    /// `new HttpException('pesan', status)` — tanpa field `error`.
    #[error("{status}: {message}")]
    Plain { status: StatusCode, message: String },
    /// ValidationPipe.
    #[error("validation: {0:?}")]
    Validation(Vec<String>),
    /// Body JSON rusak (body-parser express): `message` string, bukan array.
    #[error("bad json: {0}")]
    BadJson(String),
    /// Pelanggaran unique (Prisma P2002 → filter global → 409).
    #[error("unique violation: {0}")]
    UniqueViolation(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

fn reason(status: StatusCode) -> &'static str {
    // Teks `error` Nest = nama status HTTP standar.
    status.canonical_reason().unwrap_or("Error")
}

impl ApiError {
    pub fn status(&self) -> StatusCode {
        match self {
            Self::Message { status, .. }
            | Self::Body { status, .. }
            | Self::Plain { status, .. } => *status,
            Self::Bare(status) => *status,
            Self::Validation(_) | Self::BadJson(_) => StatusCode::BAD_REQUEST,
            Self::UniqueViolation(_) => StatusCode::CONFLICT,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    pub fn message(status: StatusCode, message: impl Into<String>) -> Self {
        Self::Message {
            status,
            message: message.into(),
        }
    }
    pub fn body(status: StatusCode, body: Value) -> Self {
        Self::Body { status, body }
    }
    /// `new XxxException({...})` dari struct `Serialize`: urutan field = urutan kunci
    /// (`serde_json` memakai `preserve_order`). Gagal serialisasi → 500.
    pub fn serialized(status: StatusCode, body: &impl serde::Serialize) -> Self {
        match serde_json::to_value(body) {
            Ok(body) => Self::Body { status, body },
            Err(e) => Self::internal(e),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::message(StatusCode::BAD_REQUEST, message)
    }
    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::message(StatusCode::UNAUTHORIZED, message)
    }
    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::message(StatusCode::FORBIDDEN, message)
    }
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::message(StatusCode::NOT_FOUND, message)
    }
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::message(StatusCode::CONFLICT, message)
    }
    pub fn unprocessable(message: impl Into<String>) -> Self {
        Self::message(StatusCode::UNPROCESSABLE_ENTITY, message)
    }

    /// `{ status: false, message }` — bentuk favorit JwtStrategy & banyak service.
    pub fn status_false(status: StatusCode, message: impl Into<String>) -> Self {
        Self::body(
            status,
            json!({ "status": false, "message": message.into() }),
        )
    }

    /// `{ code, message }` — bentuk gerbang (403 NO_ACTIVE_COMPANY, dst).
    pub fn coded(status: StatusCode, code: &str, message: impl Into<String>) -> Self {
        Self::body(status, json!({ "code": code, "message": message.into() }))
    }

    pub fn internal(err: impl Into<anyhow::Error>) -> Self {
        Self::Internal(err.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status();
        let body = match self {
            Self::Message { status, message } => {
                json!({ "message": message, "error": reason(status), "statusCode": status.as_u16() })
            }
            Self::Bare(status) => {
                json!({ "message": reason(status), "statusCode": status.as_u16() })
            }
            Self::Body { body, .. } => body,
            Self::Plain { status, message } => {
                json!({ "statusCode": status.as_u16(), "message": message })
            }
            Self::Validation(messages) => {
                let messages = global_pipe_first(messages);
                json!({ "message": messages, "error": "Bad Request", "statusCode": 400 })
            }
            Self::BadJson(message) => {
                json!({ "message": message, "error": "Bad Request", "statusCode": 400 })
            }
            Self::UniqueViolation(message) => json!({ "status": false, "message": message }),
            Self::Internal(err) => {
                tracing::error!(error = ?err, "unhandled error");
                json!({ "statusCode": 500, "message": "Internal server error" })
            }
        };
        (status, Json(body)).into_response()
    }
}

/// Pesan `forbidNonWhitelisted` (`property x should not exist`, juga bersarang
/// `owner.property x …`).
fn is_whitelist_message(m: &str) -> bool {
    m.ends_with(" should not exist") && (m.starts_with("property ") || m.contains(".property "))
}

/// Node memasang `app.useGlobalPipes(new ValidationPipe({ transform: true }))` (main.ts:58), yang
/// berjalan SEBELUM pipe controller/param (`ADMIN_VALIDATION_PIPE` = `whitelist` +
/// `forbidNonWhitelisted`). Pipe global tidak mengenal whitelist: bila DTO punya galat lain, ia
/// melempar lebih dulu dan pesan "property x should not exist" tidak pernah muncul. Pesan whitelist
/// hanya keluar bila semua field lain sudah valid. Validator Rust mengumpulkan keduanya sekaligus;
/// penyaringan di sini meniru urutan dua pipe itu untuk semua crate (body, query, array, bersarang).
fn global_pipe_first(messages: Vec<String>) -> Vec<String> {
    if messages.iter().all(|m| is_whitelist_message(m)) {
        return messages;
    }
    messages
        .into_iter()
        .filter(|m| !is_whitelist_message(m))
        .collect()
}

impl From<sqlx::Error> for ApiError {
    fn from(err: sqlx::Error) -> Self {
        if let sqlx::Error::Database(db) = &err
            && db.is_unique_violation()
        {
            // Teks Prisma P2002 tidak bisa ditiru persis (ia memuat nama field
            // dari skema Prisma); yang jadi kontrak adalah status 409 + `status:false`.
            let target = db.constraint().unwrap_or("unknown");
            return Self::UniqueViolation(format!(
                "Unique constraint failed on the constraint: `{target}`"
            ));
        }
        Self::Internal(err.into())
    }
}

impl From<fred::error::Error> for ApiError {
    fn from(err: fred::error::Error) -> Self {
        Self::Internal(err.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    async fn body_of(err: ApiError) -> (u16, Value) {
        let res = err.into_response();
        let status = res.status().as_u16();
        let bytes = to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap_or_default();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    #[tokio::test]
    async fn whitelist_messages_only_when_fields_valid() {
        let v = |m: &[&str]| ApiError::Validation(m.iter().map(|s| (*s).to_owned()).collect());
        let (_, body) = body_of(v(&[
            "property foo should not exist",
            "owner.property x should not exist",
            "companyUuid must be a UUID",
        ]))
        .await;
        assert_eq!(
            body.get("message"),
            Some(&json!(["companyUuid must be a UUID"]))
        );
        let (_, body) = body_of(v(&[
            "property 0 should not exist",
            "property foo should not exist",
        ]))
        .await;
        assert_eq!(
            body.get("message"),
            Some(&json!([
                "property 0 should not exist",
                "property foo should not exist"
            ]))
        );
    }

    #[tokio::test]
    async fn message_form_matches_nest() {
        let (status, body) = body_of(ApiError::unauthorized("User data not found")).await;
        assert_eq!(status, 401);
        assert_eq!(
            body,
            json!({"message":"User data not found","error":"Unauthorized","statusCode":401})
        );
    }

    #[tokio::test]
    async fn bare_form_matches_nest() {
        let (_, body) = body_of(ApiError::Bare(StatusCode::UNAUTHORIZED)).await;
        assert_eq!(body, json!({"message":"Unauthorized","statusCode":401}));
    }

    async fn text_of(err: ApiError) -> String {
        let res = err.into_response();
        let bytes = to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap_or_default();
        String::from_utf8(bytes.to_vec()).unwrap_or_default()
    }

    /// Urutan kunci di kabel = Node (bukan alfabetis).
    #[tokio::test]
    async fn key_order_matches_nest() {
        assert_eq!(
            text_of(ApiError::internal(anyhow::anyhow!("x"))).await,
            r#"{"statusCode":500,"message":"Internal server error"}"#
        );
        assert_eq!(
            text_of(ApiError::forbidden("x")).await,
            r#"{"message":"x","error":"Forbidden","statusCode":403}"#
        );
        assert_eq!(
            text_of(ApiError::Bare(StatusCode::FORBIDDEN)).await,
            r#"{"message":"Forbidden","statusCode":403}"#
        );
        assert_eq!(
            text_of(ApiError::Validation(vec!["a".into()])).await,
            r#"{"message":["a"],"error":"Bad Request","statusCode":400}"#
        );
        assert_eq!(
            text_of(ApiError::status_false(StatusCode::NOT_FOUND, "x")).await,
            r#"{"status":false,"message":"x"}"#
        );
        assert_eq!(
            text_of(ApiError::coded(StatusCode::FORBIDDEN, "C", "x")).await,
            r#"{"code":"C","message":"x"}"#
        );
        #[derive(serde::Serialize)]
        struct B {
            z: i32,
            a: i32,
        }
        assert_eq!(
            text_of(ApiError::serialized(
                StatusCode::CONFLICT,
                &B { z: 1, a: 2 }
            ))
            .await,
            r#"{"z":1,"a":2}"#
        );
    }

    #[tokio::test]
    async fn plain_form_has_no_error_field() {
        let (status, body) = body_of(ApiError::Plain {
            status: StatusCode::PAYMENT_REQUIRED,
            message: "x".into(),
        })
        .await;
        assert_eq!(status, 402);
        assert_eq!(body, json!({"statusCode":402,"message":"x"}));
    }
}
