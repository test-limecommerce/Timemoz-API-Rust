//! `POST /users/invite` dan `POST /users/invite/batch` (keduanya `@HttpCode(200)`).

use axum::Json;
use axum::extract::State;
use mz_audit::AuditHandle;
use mz_core::extract::ValidatedJson;
use mz_core::{ApiResult, AppState, RequestCtx};
use serde_json::Value;

use crate::dto::{InviteBatchBody, InviteBody};
use crate::service::invite;

/// Node: src/users/users.controller.ts:152 (`invite`)
pub async fn invite(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    ValidatedJson(body): ValidatedJson<InviteBody>,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    Ok(Json(
        invite::invite(&state, &audit, &body.input(), user_id).await?,
    ))
}

/// Node: src/users/users.controller.ts:339 (`inviteBatch`)
pub async fn invite_batch(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    ValidatedJson(body): ValidatedJson<InviteBatchBody>,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    Ok(Json(
        invite::invite_batch(&state, &audit, &body.items(), user_id).await?,
    ))
}
