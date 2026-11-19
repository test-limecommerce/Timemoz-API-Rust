//! `POST /settings/webhook/payments` (`SettingsService.processWebhookMidtrans`).
//!
//! Verifikasi Midtrans: `SHA512(order_id + status_code + gross_amount + MIDTRANS_SERVER_KEY)`
//! (hex) harus sama dengan `signature_key` payload. Route publik; satu-satunya otentikasi
//! adalah tanda tangan itu. Semua kegagalan (tanda tangan salah, payload rusak, DB) di Node
//! dilempar sebagai objek biasa → Nest membalas 500 `Internal server error`; sama di sini.

use axum::Json;
use axum::extract::State;
use chrono::{Duration, NaiveDateTime};
use mz_audit::{AuditHandle, Lookup, Model};
use mz_auth::analytics::{UpgradeToPro, spawn_upgrade_to_pro};
use mz_core::extract::{Validate, ValidatedJson};
use mz_core::plan::is_paid_tier;
use mz_core::{ApiError, ApiResult, AppState};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha512};
use subtle::ConstantTimeEq;

/// Body bebas (`@Body() payload: any`).
#[derive(Debug, Deserialize)]
#[serde(transparent)]
pub struct Payload(pub Value);

impl Validate for Payload {}

#[derive(Serialize)]
pub struct Reply {
    success: bool,
    message: &'static str,
}

/// `${value}` JS: `undefined` bila properti tidak ada.
fn js_template(v: Option<&Value>) -> String {
    match v {
        None => "undefined".into(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) => "null".into(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => n
            .as_f64()
            .map_or_else(|| n.to_string(), mz_core::jsfmt::num_string),
        Some(Value::Array(items)) => items
            .iter()
            .map(|i| match i {
                Value::Null => String::new(),
                other => js_template(Some(other)),
            })
            .collect::<Vec<_>>()
            .join(","),
        Some(Value::Object(_)) => "[object Object]".into(),
    }
}

/// Tanda tangan yang diharapkan Midtrans (hex SHA-512).
pub fn expected_signature(payload: &Value, server_key: &str) -> String {
    let key = format!(
        "{}{}{}{}",
        js_template(payload.get("order_id")),
        js_template(payload.get("status_code")),
        js_template(payload.get("gross_amount")),
        server_key
    );
    hex::encode(Sha512::digest(key.as_bytes()))
}

/// `sign !== payload.signature_key` (dibandingkan constant-time; harus string yang persis sama).
pub fn signature_valid(payload: &Value, server_key: &str) -> bool {
    let Some(given) = payload.get("signature_key").and_then(Value::as_str) else {
        return false;
    };
    let expected = expected_signature(payload, server_key);
    expected.as_bytes().ct_eq(given.as_bytes()).into()
}

/// Node: src/settings/settings.controller.ts:151 (`POST /settings/webhook/payments`, 200)
pub async fn midtrans_webhook(
    State(state): State<AppState>,
    audit: AuditHandle,
    ValidatedJson(Payload(payload)): ValidatedJson<Payload>,
) -> ApiResult<Json<Reply>> {
    match process(&state, &audit, &payload).await {
        Ok(()) => Ok(Json(Reply {
            success: true,
            message: "Success",
        })),
        Err(err) => {
            tracing::error!(error = %err, "Midtrans webhook gagal diproses");
            Err(ApiError::internal(anyhow::anyhow!(
                "Failed to process webhook"
            )))
        }
    }
}

type InvoiceRow = (
    i32,            // subscription.id
    i32,            // subscription.companyId
    NaiveDateTime,  // subscription.endDate
    String,         // subscription.billingInterval
    Option<bool>,   // company.isTrial
    Option<String>, // company.currency
    Option<String>, // pricingTier.name
    Option<f64>,    // pricingTier.pricePerUserUsd
    Option<f64>,    // pricingTier.basePrice
    f64,            // invoice.totalPrice
);

async fn process(state: &AppState, audit: &AuditHandle, payload: &Value) -> anyhow::Result<()> {
    // Fail closed bila kunci server tidak dikonfigurasi (Node akan memakai teks "undefined").
    let Some(server_key) = state.config.var("MIDTRANS_SERVER_KEY") else {
        anyhow::bail!("MIDTRANS_SERVER_KEY tidak dikonfigurasi");
    };
    if !signature_valid(payload, &server_key) {
        anyhow::bail!("Unauthorized");
    }

    let transaction_status = payload.get("transaction_status").and_then(Value::as_str);
    let status = match transaction_status {
        Some("expire") => "EXPIRED",
        Some("settlement") => "PAID",
        _ => "UNPAID",
    };

    // `payload.order_id.split('-')` — bukan string = TypeError.
    let Some(order_id) = payload.get("order_id").and_then(Value::as_str) else {
        anyhow::bail!("order_id bukan string");
    };
    let invoice_number = order_id.split('-').take(3).collect::<Vec<_>>().join("-");

    let mut tx = state.db.begin().await?;

    // specs/007 (R-13): langganan MANUAL dicatat operator; webhook tidak boleh menyentuhnya.
    let owner: Option<(String, String)> = sqlx::query_as(
        r#"SELECT s.uuid::text, s."billingSource"::text
             FROM invoices i JOIN subscriptions s ON s.id = i."subscriptionId"
            WHERE i."invoiceNumber" = $1 ORDER BY i.id LIMIT 1"#,
    )
    .bind(&invoice_number)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some((sub_uuid, source)) = &owner
        && source == "MANUAL"
    {
        tracing::warn!(
            "Midtrans webhook {order_id} ignored: subscription {sub_uuid} is MANUAL billing"
        );
        return Ok(());
    }

    // Invoice PAID tidak pernah turun lagi: notifikasi Midtrans tidak dijamin berurutan
    // (`pending` bisa tiba sesudah `settlement`), dan `settlement` yang dikirim ulang tidak boleh
    // memperpanjang langganan dua kali. Postgres menilai ulang `status <> 'PAID'` setelah lock
    // baris dilepas, jadi dari dua notifikasi bersamaan hanya satu yang menang.
    let updated = sqlx::query(
        r#"UPDATE invoices SET status = $2::"InvoiceStatus"
            WHERE "invoiceNumber" = $1 AND status <> 'PAID'::"InvoiceStatus""#,
    )
    .bind(&invoice_number)
    .bind(status)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    // `owner` kosong = invoice tidak ada: jalur lama (`settlement` → "Subscription not found").
    if updated == 0 && owner.is_some() {
        tracing::warn!(
            "Midtrans webhook {order_id} ({}) ignored: invoice {invoice_number} is already PAID",
            transaction_status.unwrap_or("undefined")
        );
        return Ok(());
    }

    let mut signal: Option<UpgradeToPro> = None;
    if transaction_status == Some("settlement") {
        let invoice: Option<InvoiceRow> = sqlx::query_as(
            r#"SELECT s.id, s."companyId", s."endDate", s."billingInterval",
                      c."isTrial", c.currency, t.name, t."pricePerUserUsd", t."basePrice",
                      i."totalPrice"
                 FROM invoices i
                 JOIN subscriptions s ON s.id = i."subscriptionId"
                 LEFT JOIN company c ON c.id = s."companyId"
                 LEFT JOIN "pricingTiers" t ON t.id = s."pricingTierId"
                WHERE i."invoiceNumber" = $1 ORDER BY i.id LIMIT 1"#,
        )
        .bind(&invoice_number)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((
            sub_id,
            company_id,
            end_date,
            interval,
            is_trial,
            currency,
            tier,
            per_user,
            base,
            total,
        )) = invoice
        else {
            anyhow::bail!("Subscription not found");
        };

        // Dibaca sebelum `isTrial` di-false-kan: guard anti-renewal.
        signal = Some(UpgradeToPro {
            company_id,
            // `isPaidTier(subscription.pricingTier)`: tanpa tier (LEFT JOIN) = false.
            is_first_paid_transition: is_trial.unwrap_or(false)
                && tier.is_some()
                && is_paid_tier(tier.as_deref(), per_user, base),
            tier_name: tier,
            billing_interval: Some(interval),
            payment_provider: "midtrans",
            value: Some(total),
            currency,
        });

        sqlx::query(
            r#"UPDATE subscriptions SET "endDate" = $2, status = 'ACTIVE'::"SubscriptionStatus"
                WHERE id = $1"#,
        )
        .bind(sub_id)
        .bind(end_date + Duration::days(30))
        .execute(&mut *tx)
        .await?;

        // `tx.company.update` — model ber-audit (pelaku `system`: route publik).
        let pre = audit.pre_read(Model::Company, Lookup::Id(company_id)).await;
        sqlx::query(r#"UPDATE company SET "isTrial" = false WHERE id = $1"#)
            .bind(company_id)
            .execute(&mut *tx)
            .await?;
        if let Some(row) = audit
            .result_row(&mut *tx, Model::Company, Lookup::Id(company_id))
            .await
        {
            audit.record_update(Model::Company, pre, &row).await;
        }
    }
    tx.commit().await?;

    // Di luar transaksi bisnis: analytics tidak boleh me-rollback pembayaran yang sudah settle.
    if let Some(input) = signal {
        spawn_upgrade_to_pro(state, audit, input);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn signature_matches_midtrans_formula() {
        let mut p =
            json!({"order_id": "INV-1-2-x", "status_code": "200", "gross_amount": "100.00"});
        let sig = hex::encode(Sha512::digest(b"INV-1-2-x200100.00key"));
        assert_eq!(expected_signature(&p, "key"), sig);
        assert!(!signature_valid(&p, "key"));
        p["signature_key"] = json!(sig);
        assert!(signature_valid(&p, "key"));
        assert!(!signature_valid(&p, "other"));
        p["signature_key"] = json!(5);
        assert!(!signature_valid(&p, "key"));
    }

    #[test]
    fn js_template_semantics() {
        assert_eq!(js_template(None), "undefined");
        assert_eq!(js_template(Some(&json!(200))), "200");
        assert_eq!(js_template(Some(&json!(10.5))), "10.5");
        assert_eq!(js_template(Some(&json!({}))), "[object Object]");
    }
}
