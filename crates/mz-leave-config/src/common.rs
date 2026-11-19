//! Bagian bersama handler konfigurasi cuti: company, bentuk `{ id, name, username }`,
//! `{ success: true }`, dan body kosong (`null` dari controller Nest).

use axum::body::Body;
use axum::http::{StatusCode, header};
use axum::response::Response;
use mz_core::{ApiError, ApiResult, AppState, RequestCtx};
use serde::Serialize;
use sqlx::FromRow;

/// `resolveCompanyId(userId)`: 404 di sini (bukan 403 resolver bersama) — kontrak endpoint cuti
/// (`leave-types.service.ts:23`, `leave-policies.service.ts:42`).
pub async fn company_of(state: &AppState, ctx: &RequestCtx) -> ApiResult<i32> {
    ctx.require_user_id()?;
    ctx.company_id(state)
        .await?
        .ok_or_else(|| ApiError::not_found("User is not associated with any company"))
}

/// `select: { id: true, name: true, username: true }`.
#[derive(Debug, Clone, PartialEq, Serialize, FromRow)]
pub struct UserMini {
    pub id: i32,
    pub name: String,
    pub username: String,
}

/// `{ success: true }`.
#[derive(Debug, Serialize)]
pub struct Success {
    pub success: bool,
}

pub const SUCCESS: Success = Success { success: true };

/// Controller Nest yang mengembalikan `null`: `ExpressAdapter.reply` memanggil
/// `response.send()` — status 200, tanpa body dan tanpa `Content-Type`.
pub fn null_body() -> Response {
    let mut res = Response::new(Body::empty());
    *res.status_mut() = StatusCode::OK;
    res.headers_mut().insert(
        header::CONTENT_LENGTH,
        header::HeaderValue::from_static("0"),
    );
    res
}
