//! Port `src/invoices/invoices.controller.ts` (11 route). Class-level `@RequireFeature('billing')`
//! + `@AllowWhenSubscriptionLocked()`; tiap route `@UseGuards(JwtAuthGuard, RolesGuard)`.
//!
//! Tanpa `@HttpCode`: POST = 201, GET/PUT = 200 — juga untuk `{ success: false }`.

use axum::Json;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use mz_audit::AuditHandle;
use mz_core::extract::{Validate, ValidatedJson};
use mz_core::uuid_rules::{is_uuid, parse_uuid_pipe};
use mz_core::{ApiError, ApiResult, AppState, RequestCtx};
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

/// `ChangePlanDto`: `@IsString() @IsUUID() newPricingUuid` (pesan urut dekorator bawah → atas).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ChangePlanDto {
    pub new_pricing_uuid: Option<Value>,
}

impl Validate for ChangePlanDto {
    fn validate(&self) -> Vec<String> {
        match self.new_pricing_uuid.as_ref().and_then(Value::as_str) {
            Some(s) if is_uuid(s) => vec![],
            Some(_) => vec!["newPricingUuid must be a UUID".into()],
            None => vec![
                "newPricingUuid must be a UUID".into(),
                "newPricingUuid must be a string".into(),
            ],
        }
    }
}

impl ChangePlanDto {
    /// UUID yang sudah lolos `@IsUUID()` (nil/max ikut; Postgres menerimanya).
    fn uuid(&self) -> ApiResult<Uuid> {
        self.new_pricing_uuid
            .as_ref()
            .and_then(Value::as_str)
            .and_then(|s| Uuid::parse_str(s).ok())
            .ok_or_else(|| ApiError::Validation(self.validate()))
    }
}

fn created(v: Value) -> (StatusCode, Json<Value>) {
    (StatusCode::CREATED, Json(v))
}

/// Node: invoices.controller.ts:42 (`findAll`).
pub async fn h_find_all(
    State(state): State<AppState>,
    ctx: RequestCtx,
    RawQuery(q): RawQuery,
) -> ApiResult<Json<Value>> {
    let uid = ctx.require_user_id()?;
    crate::list::find_all(&state, uid, q.as_deref().unwrap_or_default())
        .await
        .map(Json)
}

/// Node: invoices.controller.ts:51 (`findOne`), `ParseUUIDPipe`.
pub async fn h_find_one(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(raw): Path<String>,
) -> ApiResult<Json<Value>> {
    let uid = ctx.require_user_id()?;
    let uuid = parse_uuid_pipe(&raw)?;
    crate::list::find_one(&state, &uuid, uid).await.map(Json)
}

/// `GET /invoices/<path statis POST>` (mis. `/invoices/change-plan`): Nest mencocokkan
/// `GET /invoices/:uuid` → guard lalu `ParseUUIDPipe` gagal (400).
pub async fn h_find_one_static(ctx: RequestCtx) -> ApiResult<Json<Value>> {
    ctx.require_user_id()?;
    Err(ApiError::bad_request(
        "Validation failed (uuid is expected)",
    ))
}

/// Node: invoices.controller.ts:62 (`renew`).
pub async fn h_renew(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(raw): Path<String>,
) -> ApiResult<Json<Value>> {
    let uid = ctx.require_user_id()?;
    let uuid = parse_uuid_pipe(&raw)?;
    crate::renew::renew(&state, uid, &uuid).await.map(Json)
}

/// Node: invoices.controller.ts:73 (`changePlan`).
pub async fn h_change_plan(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    ValidatedJson(dto): ValidatedJson<ChangePlanDto>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let uid = ctx.require_user_id()?;
    crate::plan::change_plan(&state, &audit, uid, &dto.uuid()?)
        .await
        .map(created)
}

/// Node: invoices.controller.ts:81 (`changeActivePlan`).
pub async fn h_change_active_plan(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    ValidatedJson(dto): ValidatedJson<ChangePlanDto>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let uid = ctx.require_user_id()?;
    crate::plan::change_active_plan(&state, &audit, uid, &dto.uuid()?)
        .await
        .map(created)
}

/// Node: invoices.controller.ts:92 (`subscribe`).
pub async fn h_subscribe(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    ValidatedJson(dto): ValidatedJson<ChangePlanDto>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let uid = ctx.require_user_id()?;
    crate::lifecycle::subscribe(&state, &audit, uid, &dto.uuid()?)
        .await
        .map(created)
}

/// Node: invoices.controller.ts:106 (`cancelSubscription`).
pub async fn h_cancel(
    State(state): State<AppState>,
    ctx: RequestCtx,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let uid = ctx.require_user_id()?;
    crate::lifecycle::cancel(&state, uid).await.map(created)
}

/// Node: invoices.controller.ts:117 (`resumeSubscription`).
pub async fn h_resume(
    State(state): State<AppState>,
    ctx: RequestCtx,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let uid = ctx.require_user_id()?;
    crate::lifecycle::resume(&state, uid).await.map(created)
}

/// Node: invoices.controller.ts:128 (`reactivateSubscription`).
pub async fn h_reactivate(
    State(state): State<AppState>,
    ctx: RequestCtx,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let uid = ctx.require_user_id()?;
    crate::lifecycle::reactivate(&state, uid).await.map(created)
}

/// Node: invoices.controller.ts:139 (`downloadInvoicePdf`) → `generateInvoicePdf`: `data.url` = path
/// proxy ber-JWT `/invoices/:uuid/pdf`.
pub async fn h_download_pdf(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(raw): Path<String>,
) -> ApiResult<Json<Value>> {
    let uid = ctx.require_user_id()?;
    let uuid = parse_uuid_pipe(&raw)?;
    Ok(Json(
        mz_billing::generate_invoice_pdf_response(&state, &uuid.to_string(), uid).await,
    ))
}

/// Node: invoices.controller.ts:158 (`pdf`) → `streamInvoicePdf`: PDF dari bucket privat (dibuat
/// dulu bila belum ada). MANAGER lolos role guard tapi selalu 404.
pub async fn h_pdf(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(raw): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let uid = ctx.require_user_id()?;
    let uuid = parse_uuid_pipe(&raw)?;
    mz_billing::stream_invoice_pdf(&state, &uuid, uid, &headers).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn errs(v: Value) -> Vec<String> {
        serde_json::from_value::<ChangePlanDto>(v)
            .unwrap()
            .validate()
    }

    #[test]
    fn change_plan_dto() {
        assert!(errs(json!({"newPricingUuid": "3f1c2b9e-8d7a-4c5b-9e1f-2a3b4c5d6e7f"})).is_empty());
        assert_eq!(
            errs(json!({})),
            vec![
                "newPricingUuid must be a UUID",
                "newPricingUuid must be a string"
            ]
        );
        assert_eq!(
            errs(json!({"newPricingUuid": 5})),
            vec![
                "newPricingUuid must be a UUID",
                "newPricingUuid must be a string"
            ]
        );
        assert_eq!(
            errs(json!({"newPricingUuid": "12345678-1234-1234-1234-123456789012"})),
            vec!["newPricingUuid must be a UUID"]
        );
    }
}
