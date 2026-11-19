//! Route operator PDF invoice (`src/admin-invoice/admin-invoice.controller.ts`, Mudah-API):
//! `GET /admin-invoices/:uuid/pdf` dan `POST /admin-invoices/:uuid/regenerate-pdf`.
//!
//! Dua route ini dulunya "tetap di Node" di `mz-admin-invoice` karena butuh `InvoiceService`;
//! sekarang PDF-nya ada di Rust (`crate::invoice_pdf`). Orkestrator boleh memindahkan file ini
//! ke `mz-admin-invoice` bila mau.

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use mz_admin_jobs::validate::{Errors, RawBody, RawPath, optional_reason};
use mz_core::gate::{RouteSpec, guard};
use mz_core::permissions::Exempt;
use mz_core::uuid_rules::parse_uuid_pipe;
use mz_core::{ApiResult, AppState};
use mz_operator::{AuditCtx, Operator, OperatorAuditEntry, OperatorRole, record};
use serde_json::{Value, json};

use crate::invoice_pdf::{
    invoice_not_found, regenerate_invoice_pdf, stream_invoice_pdf_for_operator,
};

/// `@PermissionExempt('separate-auth')` + `@AllowWhenSubscriptionLocked()`; autentikasi lewat
/// extractor [`Operator`] (`JwtAdminAuthGuard`).
const SPEC: RouteSpec = RouteSpec::public()
    .exempt(Exempt::SeparateAuth)
    .allow_locked();

const ALL_ROLES: &[OperatorRole] = &[
    OperatorRole::Owner,
    OperatorRole::Finance,
    OperatorRole::Support,
];
const OWNER_FINANCE: &[OperatorRole] = &[OperatorRole::Owner, OperatorRole::Finance];

/// Router kedua route (path penuh, tanpa prefix).
pub fn router(state: &AppState) -> Router<AppState> {
    let g = |m| guard(state, SPEC, m);
    Router::new()
        .route("/admin-invoices/{uuid}/pdf", g(get(h_pdf)))
        .route(
            "/admin-invoices/{uuid}/regenerate-pdf",
            g(post(h_regenerate_pdf)),
        )
}

/// Node: src/admin-invoice/admin-invoice.controller.ts:43 (`pdf`) → `streamInvoicePdfForOperator`.
/// `@OperatorRoles('OWNER','FINANCE','SUPPORT')`; PDF dibuat saat pertama dibuka.
async fn h_pdf(
    State(state): State<AppState>,
    op: Operator,
    RawPath(raw): RawPath,
    headers: HeaderMap,
) -> ApiResult<Response> {
    op.require(ALL_ROLES)?;
    let uuid = parse_uuid_pipe(&raw)?;
    stream_invoice_pdf_for_operator(&state, &uuid, &headers).await
}

/// `OptionalReasonDto` di bawah `ADMIN_VALIDATION_PIPE`.
fn validate_optional_reason(body: &Value) -> ApiResult<Option<String>> {
    let mut e = Errors::default();
    e.unknown_body(body, &["reason"]);
    let reason = optional_reason(&mut e, body);
    e.into_result()?;
    Ok(reason)
}

/// Node: src/admin-invoice/admin-invoice.controller.ts:119 (`regeneratePdf`) →
/// `AdminInvoiceService.regeneratePdf` (admin-invoice.service.ts:223). `@HttpCode(200)`,
/// `@OperatorRoles('OWNER','FINANCE')`. Menulis `operatorAuditLog` `invoice.regenerate_pdf`.
async fn h_regenerate_pdf(
    State(state): State<AppState>,
    op: Operator,
    ctx: AuditCtx,
    RawPath(raw): RawPath,
    RawBody(body): RawBody,
) -> ApiResult<(StatusCode, Json<Value>)> {
    op.require(OWNER_FINANCE)?;
    // Galat body (pipe) muncul sebelum `ParseUUIDPipe`.
    let reason = validate_optional_reason(&body)?;
    let uuid = parse_uuid_pipe(&raw)?;

    let Some(result) = regenerate_invoice_pdf(&state, &uuid).await? else {
        return Err(invoice_not_found());
    };
    let mut tx = state.db.begin().await?;
    record(
        &mut *tx,
        &ctx,
        &OperatorAuditEntry {
            operator_id: op.id,
            action: "invoice.regenerate_pdf",
            target_type: "invoice",
            target_uuid: Some(uuid),
            company_id: Some(result.company_id),
            before: None,
            after: None,
            reason: reason.as_deref(),
        },
    )
    .await?;
    tx.commit().await?;
    Ok((
        StatusCode::OK,
        Json(json!({
            "success": true,
            "message": "Invoice PDF regenerated",
            "data": { "url": result.url },
        })),
    ))
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn router_tanpa_konflik() {
        if let Ok(state) = mz_core::AppState::for_test() {
            let _ = super::router(&state);
        }
    }
}
