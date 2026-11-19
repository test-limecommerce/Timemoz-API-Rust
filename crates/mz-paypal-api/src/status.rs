//! `GET /paypal/subscription/status` dan `GET /paypal/proration/return`.

use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::{RawQuery, State};
use axum::http::HeaderMap;
use axum::response::Response;
use chrono::NaiveDateTime;
use mz_core::gate::RequestCtx;
use mz_core::qs::{self, QVal};
use mz_core::time::JsDate;
use mz_core::{ApiError, ApiResult, AppState};
use mz_paypal::PaypalClient;
use serde_json::{Value, json};

use crate::proration;
use crate::rows;
use crate::util::{form_encode, redirect_302};

/// Long poll Node: cek ulang tiap 2 dtk, maksimal 30 dtk. Bisa dipersingkat lewat
/// `MZ_PAYPAL_STATUS_POLL_MS` / `MZ_PAYPAL_STATUS_MAX_WAIT_MS` (khusus uji, bukan fitur Node).
fn poll_config(state: &AppState) -> (Duration, Duration) {
    let ms = |k: &str, d: u64| {
        state
            .config
            .var(k)
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(d)
    };
    (
        Duration::from_millis(ms("MZ_PAYPAL_STATUS_POLL_MS", 2_000)),
        Duration::from_millis(ms("MZ_PAYPAL_STATUS_MAX_WAIT_MS", 30_000)),
    )
}

fn fail(message: &str) -> Json<Value> {
    Json(json!({ "success": false, "message": message }))
}

/// Node: src/paypal/paypal.controller.ts:129 (`GET /paypal/subscription/status`, 200)
pub async fn subscription_status(
    State(state): State<AppState>,
    ctx: RequestCtx,
    RawQuery(raw): RawQuery,
) -> ApiResult<Json<Value>> {
    let Some(company_id) = ctx.company_id(&state).await? else {
        return Ok(fail("User company not found"));
    };
    let q = qs::parse(raw.as_deref().unwrap_or_default());
    // `@Query('subscription_id')` tanpa validasi: tanpa nilai, Prisma `{ x: undefined }` =
    // tanpa filter (baris pertama tabel); array/objek → galat Prisma (500).
    let subscription_id: Option<String> = match qs::get(&q, "subscription_id") {
        None => None,
        Some(QVal::One(s)) => Some(s.clone()),
        Some(_) => {
            return Err(ApiError::Internal(anyhow_like(
                "Invalid subscription_id query value",
            )));
        }
    };

    let find = |col: &'static str| {
        let state = state.clone();
        let id = subscription_id.clone();
        async move {
            match id {
                Some(id) => {
                    let sql = format!(
                        r#"SELECT id, "companyId", "endDate" FROM subscriptions WHERE {col} = $1 ORDER BY id LIMIT 1"#
                    );
                    sqlx::query_as::<_, (i32, i32, NaiveDateTime)>(&sql)
                        .bind(id)
                        .fetch_optional(&state.db)
                        .await
                }
                None => sqlx::query_as::<_, (i32, i32, NaiveDateTime)>(
                    r#"SELECT id, "companyId", "endDate" FROM subscriptions ORDER BY id LIMIT 1"#,
                )
                .fetch_optional(&state.db)
                .await,
            }
        }
    };
    let mut pending = false;
    let mut sub = find(r#""paypalSubscriptionId""#).await?;
    if sub.is_none() {
        sub = find(r#""pendingPaypalSubscriptionId""#).await?;
        if sub.is_none() {
            return Ok(fail("Subscription not found"));
        }
        pending = true;
    }
    let Some((sub_id, sub_company, end_date)) = sub else {
        return Ok(fail("Subscription not found"));
    };
    if sub_company != company_id {
        return Ok(fail(
            "Unauthorized: subscription does not belong to your company",
        ));
    }

    // Plan change tertunda: hanya invoice UNPAID (upgrade); downgrade belum punya invoice.
    let data = rows::latest_invoice_with_subscription(&state.db, sub_id, pending).await?;
    let Some((invoice_id, status, data)) = data else {
        if pending {
            return Ok(Json(json!({
                "success": true,
                "message": "Plan change scheduled",
                "data": null,
                "isPendingPlanChange": true,
                "scheduledAt": JsDate(end_date),
                "paypalStatus": null,
            })));
        }
        return Ok(fail("Invoice not found for this subscription"));
    };

    // Invoice masih UNPAID: cek status real-time PayPal (balapan redirect vs webhook).
    let mut paypal_status = Value::Null;
    if status == "UNPAID" {
        let pp_id = subscription_id.as_deref().unwrap_or("undefined");
        match PaypalClient::from_state(&state)
            .get_subscription(pp_id)
            .await
        {
            Ok(v) => paypal_status = v.get("status").cloned().unwrap_or(Value::Null),
            Err(err) => tracing::warn!(
                "Failed to fetch PayPal real-time status for {pp_id}: {}",
                err.message
            ),
        }
        if paypal_status.as_str() == Some("ACTIVE") {
            let (interval, max_wait) = poll_config(&state);
            let start = Instant::now();
            while start.elapsed() < max_wait {
                tokio::time::sleep(interval).await;
                if let Some((st, updated)) =
                    rows::invoice_with_subscription(&state.db, invoice_id).await?
                    && st == "PAID"
                {
                    return Ok(Json(json!({
                        "success": true,
                        "message": "Success",
                        "data": updated,
                        "paypalStatus": "ACTIVE",
                    })));
                }
            }
            tracing::warn!(
                "Long poll timeout for invoice {invoice_id} — webhook not yet processed"
            );
        }
    }

    Ok(Json(json!({
        "success": true,
        "message": "Success",
        "data": data,
        "paypalStatus": paypal_status,
    })))
}

fn anyhow_like(msg: &'static str) -> anyhow::Error {
    anyhow::Error::msg(msg)
}

/// Node: src/paypal/paypal.controller.ts:286 (`GET /paypal/proration/return`, 302).
/// Publik (redirect browser dari PayPal tidak membawa JWT); selalu redirect ke dashboard.
pub async fn proration_return(
    State(state): State<AppState>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Response {
    let q = qs::parse(raw.as_deref().unwrap_or_default());
    let token = qs::get(&q, "token").and_then(QVal::as_str);
    let mut status = "failed";
    let mut invoice_number: Option<String> = None;
    match token.filter(|t| proration::is_paypal_order_id(t)) {
        Some(token) => match proration::capture_and_apply(&state, token).await {
            Ok(result) => {
                status = proration::proration_return_status(result.outcome);
                invoice_number = result.invoice_number;
                tracing::info!(
                    "Proration return for order {token}: {}",
                    result.outcome.as_str()
                );
            }
            Err(err) => {
                tracing::error!(
                    "Proration return failed for order {token}: {}",
                    err.detail()
                );
            }
        },
        None => tracing::warn!("Proration return called with invalid token"),
    }
    let dashboard = state
        .config
        .var("FRONTEND_URL")
        .unwrap_or_else(|| "http://localhost:3001".into());
    let mut params = vec![("proration", status)];
    if let Some(n) = invoice_number.as_deref().filter(|n| !n.is_empty()) {
        params.push(("invoice", n));
    }
    redirect_302(
        &headers,
        &format!("{dashboard}/billing?{}", form_encode(&params)),
    )
}
