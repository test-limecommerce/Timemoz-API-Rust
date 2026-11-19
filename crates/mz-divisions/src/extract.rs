//! Extractor path yang meniru `@Param('uuid', new ParseUUIDPipe())`: galat pipe (400
//! `Validation failed (uuid is expected)`) terjadi SEBELUM validasi query/body, sesuai urutan
//! parameter handler Nest.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use mz_core::ApiError;
use mz_core::extract::PathParams;
use mz_core::uuid_rules::uuid_pipe;

/// `/{uuid}`.
pub struct UuidParam(pub String);

impl<S: Send + Sync> FromRequestParts<S> for UuidParam {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let PathParams(raw) = PathParams::<String>::from_request_parts(parts, state).await?;
        uuid_pipe(raw).map(Self)
    }
}

/// `/{uuid}/members/{userUuid}` — `uuid` diperiksa lebih dulu, lalu `userUuid`.
pub struct UuidPair(pub String, pub String);

impl<S: Send + Sync> FromRequestParts<S> for UuidPair {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let PathParams((a, b)) =
            PathParams::<(String, String)>::from_request_parts(parts, state).await?;
        let a = uuid_pipe(a)?;
        let b = uuid_pipe(b)?;
        Ok(Self(a, b))
    }
}
