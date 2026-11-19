//! Handler HTTP `src/users/users.controller.ts`, dipecah per kelompok.

use axum::extract::FromRequestParts;
use axum::extract::Path;
use axum::http::request::Parts;
use mz_core::ApiError;
use mz_core::uuid_rules::uuid_pipe as parse_uuid;

pub mod account;
pub mod devices;
pub mod invite;
pub mod local_db;
pub mod rates;

/// `@Param('uuid', ParseUUIDPipe)`. Berupa extractor (bukan cek di dalam handler) supaya pipe
/// berjalan SEBELUM `ValidatedJson` — urutan yang sama dengan Nest (parameter kiri ke kanan).
#[derive(Debug, Clone)]
pub struct UuidParam(pub String);

impl<S: Send + Sync> FromRequestParts<S> for UuidParam {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let Path(raw) = Path::<String>::from_request_parts(parts, state)
            .await
            .map_err(|e| ApiError::bad_request(e.body_text()))?;
        parse_uuid(raw).map(Self)
    }
}

/// `@Param('uuid', ParseUUIDPipe)` + `@Param('x', ParseUUIDPipe)` pada satu route.
#[derive(Debug, Clone)]
pub struct UuidPair(pub String, pub String);

impl<S: Send + Sync> FromRequestParts<S> for UuidPair {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let Path((a, b)) = Path::<(String, String)>::from_request_parts(parts, state)
            .await
            .map_err(|e| ApiError::bad_request(e.body_text()))?;
        Ok(Self(parse_uuid(a)?, parse_uuid(b)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_shapes() {
        use mz_core::uuid_rules::is_uuid_shape as is_uuid;
        assert!(is_uuid("123e4567-e89b-12d3-a456-426614174000"));
        assert!(is_uuid("123E4567-E89B-12D3-A456-426614174000"));
        for bad in [
            "",
            "log",
            "revoke",
            "123e4567e89b12d3a456426614174000",
            "123e4567-e89b-12d3-a456-42661417400g",
        ] {
            assert!(!is_uuid(bad), "{bad}");
        }
    }

    #[test]
    fn bad_uuid_message_matches_parse_uuid_pipe() {
        assert!(matches!(
            parse_uuid("nope".into()),
            Err(ApiError::Message { message, .. }) if message == "Validation failed (uuid is expected)"
        ));
    }
}
