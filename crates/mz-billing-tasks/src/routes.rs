//! Lima route operator `POST /admin/*/force*` (`src/admin/admin.controller.ts`):
//! `invoice-scheduler/force` (:199), `plan-change/force` (:229), `subscription-expiration/force`
//! (:261), `invoice-scheduler/force-proration` (:299), `proration-expiration/force` (:385).
//!
//! Guard: `JwtAdminAuthGuard` (extractor [`Operator`]), tanpa `@OperatorRoles` (semua operator aktif),
//! `@UsePipes(ADMIN_VALIDATION_PIPE)` dengan `ForceJobDto`, `OperatorAuditInterceptor` +
//! `@OperatorAuditAction(..., { targetType: 'system', companyFrom: 'body' })`. Tanpa `@HttpCode` → 201.
//! Pelaku audit tulisan model = operator (konteks request diteruskan apa adanya oleh `runAs` Node).

use std::future::Future;

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use mz_admin_jobs::validate::{Errors, RawBody};
use mz_audit::AuditHandle;
use mz_core::gate::{RouteSpec, guard};
use mz_core::jsfmt::{as_i32, number as js_number};
use mz_core::permissions::Exempt;
use mz_core::{ApiResult, AppState};
use mz_operator::{AuditCtx, AuditMeta, CompanyFrom, Operator, audit_after_success};
use serde_json::Value;

use crate::{Ctx, expiration, plan_change, proration_expiry, scheduler};

/// `@PermissionExempt('separate-auth')` + `@AllowWhenSubscriptionLocked()`.
const SPEC: RouteSpec = RouteSpec::public()
    .exempt(Exempt::SeparateAuth)
    .allow_locked();

pub fn router(state: &AppState) -> Router<AppState> {
    let g = |m| guard(state, SPEC, m);
    Router::new()
        .route("/admin/invoice-scheduler/force", g(post(h_invoice_force)))
        .route("/admin/plan-change/force", g(post(h_plan_change_force)))
        .route(
            "/admin/subscription-expiration/force",
            g(post(h_subscription_expiration_force)),
        )
        .route(
            "/admin/invoice-scheduler/force-proration",
            g(post(h_proration_force)),
        )
        .route(
            "/admin/proration-expiration/force",
            g(post(h_proration_expiration_force)),
        )
}

/// `@Type(() => Number)` (class-transformer `Number(value)`).
fn to_number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => Some(js_number(s)),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

/// `ForceJobDto` (`src/admin/dto/force-job.dto.ts`) di bawah `ADMIN_VALIDATION_PIPE`:
/// `@IsOptional() @Type(Number) @IsInt() @Min(1) companyId?`. Pesan per properti dari dekorator
/// paling bawah (`Min`) ke atas (`IsInt`).
pub fn validate_force(body: &Value) -> ApiResult<Option<i32>> {
    let mut e = Errors::default();
    e.unknown_body(body, &["companyId"]);
    let mut company_id = None;
    if let Some(v) = body.get("companyId").filter(|v| !v.is_null()) {
        let n = to_number(v);
        let is_int = n.is_some_and(|x| x.is_finite() && x.fract() == 0.0);
        let ge1 = n.is_some_and(|x| x >= 1.0);
        if !ge1 {
            e.push("companyId must not be less than 1");
        }
        if !is_int {
            e.push("companyId must be an integer number");
        }
        if ge1 && is_int {
            // Di luar Int32 Prisma melempar; di sini tidak ada company yang cocok.
            company_id = Some(n.and_then(as_i32).unwrap_or(i32::MAX));
        }
    }
    e.into_result()?;
    Ok(company_id)
}

async fn force<F, Fut>(
    state: AppState,
    op: Operator,
    actx: AuditCtx,
    audit: AuditHandle,
    body: Value,
    action: &'static str,
    run: F,
) -> ApiResult<(StatusCode, Json<Value>)>
where
    F: FnOnce(Ctx, Option<i32>) -> Fut,
    Fut: Future<Output = ApiResult<Value>>,
{
    let company_id = validate_force(&body)?;
    let res = run(Ctx::request(&state, audit), company_id).await?;
    audit_after_success(
        &state,
        &op,
        &actx,
        AuditMeta {
            action,
            target_type: "system",
            company_from: Some(CompanyFrom::Body),
        },
        None,
        Some(&body),
    )
    .await;
    Ok((StatusCode::CREATED, Json(res)))
}

/// Node: src/admin/admin.controller.ts:199 (`triggerInvoiceSchedulerForce`).
async fn h_invoice_force(
    State(state): State<AppState>,
    op: Operator,
    actx: AuditCtx,
    audit: AuditHandle,
    RawBody(body): RawBody,
) -> ApiResult<(StatusCode, Json<Value>)> {
    force(
        state,
        op,
        actx,
        audit,
        body,
        "billing_force.invoice",
        |ctx, c| async move { scheduler::invoice_scheduler_force(&ctx, c).await },
    )
    .await
}

/// Node: src/admin/admin.controller.ts:229 (`triggerPendingPlanChangeForce`).
async fn h_plan_change_force(
    State(state): State<AppState>,
    op: Operator,
    actx: AuditCtx,
    audit: AuditHandle,
    RawBody(body): RawBody,
) -> ApiResult<(StatusCode, Json<Value>)> {
    force(
        state,
        op,
        actx,
        audit,
        body,
        "billing_force.plan_change",
        |ctx, c| async move { plan_change::run(&ctx, c).await },
    )
    .await
}

/// Node: src/admin/admin.controller.ts:261 (`triggerSubscriptionExpirationForce`).
async fn h_subscription_expiration_force(
    State(state): State<AppState>,
    op: Operator,
    actx: AuditCtx,
    audit: AuditHandle,
    RawBody(body): RawBody,
) -> ApiResult<(StatusCode, Json<Value>)> {
    force(
        state,
        op,
        actx,
        audit,
        body,
        "billing_force.subscription_expiration",
        |ctx, c| async move {
            tracing::info!(
                "[FORCE-EXPIRATION] Running{}...",
                c.map(|c| format!(" for companyId={c}"))
                    .unwrap_or_else(|| " (all companies)".to_owned())
            );
            expiration::run(&ctx, c).await
        },
    )
    .await
}

/// Node: src/admin/admin.controller.ts:299 (`triggerAnnualProrationForce`).
async fn h_proration_force(
    State(state): State<AppState>,
    op: Operator,
    actx: AuditCtx,
    audit: AuditHandle,
    RawBody(body): RawBody,
) -> ApiResult<(StatusCode, Json<Value>)> {
    force(
        state,
        op,
        actx,
        audit,
        body,
        "billing_force.proration",
        |ctx, c| async move { scheduler::annual_proration_force(&ctx, c).await },
    )
    .await
}

/// Node: src/admin/admin.controller.ts:385 (`triggerProratedUsersExpirationForce`).
async fn h_proration_expiration_force(
    State(state): State<AppState>,
    op: Operator,
    actx: AuditCtx,
    audit: AuditHandle,
    RawBody(body): RawBody,
) -> ApiResult<(StatusCode, Json<Value>)> {
    force(
        state,
        op,
        actx,
        audit,
        body,
        "billing_force.proration_expiration",
        |ctx, c| async move { proration_expiry::run(&ctx, c, true).await },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use mz_core::ApiError;
    use serde_json::json;

    fn msgs(r: ApiResult<Option<i32>>) -> Vec<String> {
        match r {
            Err(ApiError::Validation(m)) => m,
            other => vec![format!("{other:?}")],
        }
    }

    #[test]
    fn force_dto() {
        assert_eq!(validate_force(&json!({})).ok(), Some(None));
        assert_eq!(validate_force(&json!({"companyId": null})).ok(), Some(None));
        assert_eq!(validate_force(&json!({"companyId": 3})).ok(), Some(Some(3)));
        assert_eq!(
            validate_force(&json!({"companyId": "7"})).ok(),
            Some(Some(7))
        );
        assert_eq!(
            msgs(validate_force(&json!({"companyId": 0}))),
            ["companyId must not be less than 1"]
        );
        assert_eq!(
            msgs(validate_force(&json!({"companyId": "abc"}))),
            [
                "companyId must not be less than 1",
                "companyId must be an integer number"
            ]
        );
        assert_eq!(
            msgs(validate_force(&json!({"companyId": 2.5}))),
            ["companyId must be an integer number"]
        );
        assert_eq!(
            msgs(validate_force(&json!({"companyId": 1, "x": 1}))),
            ["property x should not exist"]
        );
    }
}
