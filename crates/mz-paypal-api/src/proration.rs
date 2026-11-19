//! Port `src/paypal/proration-payment.service.ts` — pelunasan invoice PRORATION
//! (PayPal Order intent CAPTURE).
//!
//! Tiga pintu masuk memakai jalur yang sama:
//! - `GET /paypal/proration/return` → [`capture_and_apply`]
//! - webhook `CHECKOUT.ORDER.APPROVED` → [`capture_and_apply`]
//! - webhook `PAYMENT.CAPTURE.COMPLETED` → [`apply_completed_payment`]
//!
//! Keamanan konkurensi berasal dari kunci baris invoice (`FOR UPDATE`) di
//! [`apply_in_transaction`], urutan kunci invoice → subscription (sama dengan Node & cron expiry).

use std::time::Duration;

use mz_audit::{ActorType, AuditHandle, Lookup, Model, Row};
use mz_core::AppState;
use mz_core::jsfmt;
use mz_paypal::{PaypalClient, PaypalError};
use serde_json::{Value, json};

use crate::util::js_round;

/// `PAYPAL_ORDER_ID_PATTERN` = `/^[A-Z0-9]{10,30}$/` (proration-payment.service.ts:15).
pub fn is_paypal_order_id(s: &str) -> bool {
    (10..=30).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

/// `ProrationPaymentOutcome` (proration-payment.service.ts:17).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Paid,
    Reactivated,
    AlreadyPaid,
    PaidNoReactivation,
    NotFound,
    NotCompleted,
    AmountMismatch,
    InvoiceClosed,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Paid => "paid",
            Self::Reactivated => "reactivated",
            Self::AlreadyPaid => "already_paid",
            Self::PaidNoReactivation => "paid_no_reactivation",
            Self::NotFound => "not_found",
            Self::NotCompleted => "not_completed",
            Self::AmountMismatch => "amount_mismatch",
            Self::InvoiceClosed => "invoice_closed",
        }
    }
}

/// `prorationReturnStatus` (proration-payment.service.ts:36): `success|pending|failed`.
pub fn proration_return_status(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Paid
        | Outcome::Reactivated
        | Outcome::AlreadyPaid
        | Outcome::PaidNoReactivation => "success",
        Outcome::NotCompleted => "pending",
        _ => "failed",
    }
}

/// `ProrationPaymentResult`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProrationResult {
    pub outcome: Outcome,
    /// Diisi bila invoice-nya ditemukan.
    pub invoice_number: Option<String>,
}

/// Galat yang di Node dilempar ke pemanggil (controller me-log; webhook tidak menandai processed).
#[derive(Debug)]
pub enum SettleError {
    Paypal(PaypalError),
    Db(sqlx::Error),
    /// Klaim compare-and-set gagal / transaksi melewati batas waktu.
    Invariant(String),
}

impl std::fmt::Display for SettleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Paypal(e) => write!(f, "{e}"),
            Self::Db(e) => write!(f, "{e}"),
            Self::Invariant(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for SettleError {}

impl From<sqlx::Error> for SettleError {
    fn from(e: sqlx::Error) -> Self {
        Self::Db(e)
    }
}

impl From<PaypalError> for SettleError {
    fn from(e: PaypalError) -> Self {
        Self::Paypal(e)
    }
}

impl SettleError {
    /// `error?.response?.data || error?.message` (untuk log controller).
    pub fn detail(&self) -> String {
        match self {
            Self::Paypal(e) => e
                .data
                .as_ref()
                .map_or_else(|| e.message.clone(), Value::to_string),
            other => other.to_string(),
        }
    }
}

/// Issue 422 capture yang berarti "belum bisa dibayar sekarang" (proration-payment.service.ts:74).
const NOT_READY_CAPTURE_ISSUES: [&str; 5] = [
    "ORDER_NOT_APPROVED",
    "INSTRUMENT_DECLINED",
    "PAYER_ACTION_REQUIRED",
    "TRANSACTION_REFUSED",
    "PAYER_CANNOT_PAY",
];

/// Prisma `$transaction({ timeout: 30_000 })`.
const TX_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy)]
enum Mode {
    Capture,
    Verify,
}

/// Node: proration-payment.service.ts:115 `captureAndApply` — capture (idempoten) lalu terapkan.
pub async fn capture_and_apply(
    state: &AppState,
    order_id: &str,
) -> Result<ProrationResult, SettleError> {
    settle(state, order_id, Mode::Capture).await
}

/// Node: proration-payment.service.ts:124 `applyCompletedPayment` — tanpa capture ulang;
/// status & nominal tetap dibaca dari PayPal (GET order).
pub async fn apply_completed_payment(
    state: &AppState,
    order_id: &str,
) -> Result<ProrationResult, SettleError> {
    settle(state, order_id, Mode::Verify).await
}

#[derive(sqlx::FromRow)]
struct InvoiceHead {
    id: i32,
    #[sqlx(rename = "invoiceNumber")]
    invoice_number: String,
    status: String,
    #[sqlx(rename = "totalPrice")]
    total_price: f64,
}

/// Node: proration-payment.service.ts:128 `settle`.
async fn settle(
    state: &AppState,
    order_id: &str,
    mode: Mode,
) -> Result<ProrationResult, SettleError> {
    let none = |outcome| ProrationResult {
        outcome,
        invoice_number: None,
    };
    if !is_paypal_order_id(order_id) {
        tracing::warn!("Proration payment: invalid PayPal order id");
        return Ok(none(Outcome::NotFound));
    }

    let invoice: Option<InvoiceHead> = sqlx::query_as(
        r#"SELECT id, "invoiceNumber", status::text AS status, "totalPrice" FROM invoices
            WHERE "paypalPaymentId" = $1 AND "invoiceType" = 'PRORATION'::"InvoiceType"
            ORDER BY id LIMIT 1"#,
    )
    .bind(order_id)
    .fetch_optional(&state.db)
    .await?;
    let Some(invoice) = invoice else {
        tracing::warn!("No PRORATION invoice found for PayPal order {order_id}");
        return Ok(none(Outcome::NotFound));
    };
    let number = invoice.invoice_number.clone();
    let with = |outcome| ProrationResult {
        outcome,
        invoice_number: Some(number.clone()),
    };

    // Cek cepat di luar kunci; keputusan final dibaca ulang di bawah FOR UPDATE.
    if invoice.status == "PAID" {
        return Ok(with(Outcome::AlreadyPaid));
    }
    if matches!(mode, Mode::Capture) && invoice.status == "CANCELLED" {
        tracing::warn!("Proration invoice {number} is CANCELLED — not capturing order {order_id}");
        return Ok(with(Outcome::InvoiceClosed));
    }

    let pp = PaypalClient::from_state(state);
    let order = match mode {
        Mode::Capture => capture_or_fetch(&pp, order_id).await?,
        Mode::Verify => Some(pp.get_order(order_id).await?),
    };

    let capture = order.as_ref().and_then(find_completed_capture);
    let order_status = order
        .as_ref()
        .and_then(|o| o.get("status"))
        .and_then(Value::as_str);
    let Some(capture) = capture.filter(|_| order_status == Some("COMPLETED")) else {
        tracing::warn!(
            "PayPal order {order_id} (invoice {number}) not completed yet — status: {}",
            order_status.unwrap_or("n/a")
        );
        return Ok(with(Outcome::NotCompleted));
    };

    if !amount_matches(capture.get("amount"), invoice.total_price) {
        let amount = capture.get("amount");
        let field = |k: &str| {
            amount
                .and_then(|a| a.get(k))
                .and_then(Value::as_str)
                .unwrap_or("?")
                .to_owned()
        };
        // Sentry.captureMessage → tracing::error (alert).
        tracing::error!(
            order_id,
            invoice_id = invoice.id,
            capture_id = ?capture.get("id"),
            "Proration capture amount mismatch for invoice {number} (order {order_id}): captured {} {}, expected {} USD",
            field("value"),
            field("currency_code"),
            jsfmt::to_fixed(invoice.total_price, 2)
        );
        return Ok(with(Outcome::AmountMismatch));
    }

    let capture_id = capture.get("id").and_then(Value::as_str).map(str::to_owned);
    let result = apply_in_transaction(state, invoice.id).await?;
    // Alert SESUDAH commit — kalau transaksinya rollback, tidak ada yang perlu di-refund.
    report_outcome(&result, order_id, &number, capture_id.as_deref());
    Ok(with(result.outcome()))
}

/// Node: proration-payment.service.ts:213 `captureOrFetch`. `None` = belum bisa dibayar.
async fn capture_or_fetch(pp: &PaypalClient, order_id: &str) -> Result<Option<Value>, SettleError> {
    match pp.capture_order(order_id).await {
        Ok(v) => Ok(Some(v)),
        Err(e) => match e.first_issue() {
            Some("ORDER_ALREADY_CAPTURED") => {
                tracing::info!("PayPal order {order_id} already captured — fetching order instead");
                Ok(Some(pp.get_order(order_id).await?))
            }
            Some(issue) if NOT_READY_CAPTURE_ISSUES.contains(&issue) => {
                tracing::warn!("PayPal order {order_id} not capturable: {issue}");
                Ok(None)
            }
            _ => Err(e.into()),
        },
    }
}

/// Capture berstatus COMPLETED pertama (proration-payment.service.ts:238).
fn find_completed_capture(order: &Value) -> Option<&Value> {
    order
        .get("purchase_units")?
        .as_array()?
        .iter()
        .filter_map(|u| u.get("payments")?.get("captures")?.as_array())
        .flatten()
        .find(|c| c.get("status").and_then(Value::as_str) == Some("COMPLETED"))
}

/// `amountMatches`: USD dan `Number(value).toFixed(2) === totalPrice.toFixed(2)`.
fn amount_matches(amount: Option<&Value>, total_price: f64) -> bool {
    let Some(amount) = amount else { return false };
    if amount.get("currency_code").and_then(Value::as_str) != Some("USD") {
        return false;
    }
    let Some(value) = amount.get("value").and_then(Value::as_str) else {
        return false;
    };
    let captured = jsfmt::number(value);
    captured.is_finite() && jsfmt::to_fixed(captured, 2) == jsfmt::to_fixed(total_price, 2)
}

/// `ApplyResult`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ApplyResult {
    NotFound,
    AlreadyPaid,
    Paid,
    InvoiceClosed {
        status: String,
    },
    PaidNoReactivation {
        subscription_id: i32,
        subscription_status: Option<String>,
    },
    Reactivated {
        reactivated: u64,
        prorated_count: i64,
    },
}

impl ApplyResult {
    fn outcome(&self) -> Outcome {
        match self {
            Self::NotFound => Outcome::NotFound,
            Self::AlreadyPaid => Outcome::AlreadyPaid,
            Self::Paid => Outcome::Paid,
            Self::InvoiceClosed { .. } => Outcome::InvoiceClosed,
            Self::PaidNoReactivation { .. } => Outcome::PaidNoReactivation,
            Self::Reactivated { .. } => Outcome::Reactivated,
        }
    }
}

/// Node: proration-payment.service.ts:296 `applyInTransaction` (+ `runAsSystem`: audit
/// `userCompany` ditulis langsung sebagai `system` / `proration-payment`).
async fn apply_in_transaction(
    state: &AppState,
    invoice_id: i32,
) -> Result<ApplyResult, SettleError> {
    let audit = AuditHandle::run_as(
        state.db.clone(),
        ActorType::System,
        Some("proration-payment"),
    );
    let (result, reactivated_users) =
        match tokio::time::timeout(TX_TIMEOUT, apply_tx(state, &audit, invoice_id)).await {
            Ok(r) => r?,
            Err(_) => {
                return Err(SettleError::Invariant(format!(
                    "Proration invoice {invoice_id}: transaction timed out"
                )));
            }
        };
    // Cache in-process Rust (akses & kunci langganan) untuk user yang diaktifkan ulang.
    for uid in reactivated_users {
        state.invalidate_user_access(uid);
        mz_core::subscription_lock::invalidate_user(state, uid);
    }
    Ok(result)
}

async fn apply_tx(
    state: &AppState,
    audit: &AuditHandle,
    invoice_id: i32,
) -> Result<(ApplyResult, Vec<i32>), SettleError> {
    let mut tx = state.db.begin().await?;
    sqlx::query(r#"SELECT "id" FROM "invoices" WHERE "id" = $1 FOR UPDATE"#)
        .bind(invoice_id)
        .execute(&mut *tx)
        .await?;
    let locked: Option<(String, i32, Vec<i32>)> = sqlx::query_as(
        r#"SELECT status::text, "subscriptionId", "prorationDeactivatedUserCompanyIds"
             FROM invoices WHERE id = $1"#,
    )
    .bind(invoice_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((status, subscription_id, ids)) = locked else {
        tx.commit().await?;
        return Ok((ApplyResult::NotFound, Vec::new()));
    };

    let result = match status.as_str() {
        "PAID" => (ApplyResult::AlreadyPaid, Vec::new()),
        "UNPAID" => {
            claim(&mut tx, invoice_id, "UNPAID").await?;
            (ApplyResult::Paid, Vec::new())
        }
        "EXPIRED" => {
            claim(&mut tx, invoice_id, "EXPIRED").await?;
            // Kunci subscription sebelum membaca angka yang akan ditambah.
            sqlx::query(r#"SELECT "id" FROM "subscriptions" WHERE "id" = $1 FOR UPDATE"#)
                .bind(subscription_id)
                .execute(&mut *tx)
                .await?;
            let sub: Option<(i32, i32, String, i32, i32)> = sqlx::query_as(
                r#"SELECT id, "companyId", status::text, quantity, "lastBilledQuantity"
                     FROM subscriptions WHERE id = $1"#,
            )
            .bind(subscription_id)
            .fetch_optional(&mut *tx)
            .await?;
            match sub {
                Some((sid, company_id, sub_status, quantity, last_billed))
                    if sub_status == "ACTIVE" =>
                {
                    let quantities: Vec<f64> = sqlx::query_scalar(
                        r#"SELECT quantity FROM "invoiceItems" WHERE "invoiceId" = $1 ORDER BY id"#,
                    )
                    .bind(invoice_id)
                    .fetch_all(&mut *tx)
                    .await?;
                    let sum: f64 = quantities.iter().sum();
                    #[allow(clippy::cast_possible_truncation)]
                    let prorated_count = js_round(sum) as i64;
                    let (reactivated, users) = if ids.is_empty() {
                        (0, Vec::new())
                    } else {
                        reactivate_users(&mut tx, audit, &ids, company_id).await?
                    };
                    let new_last = i64::from(last_billed) + prorated_count;
                    let new_qty = i64::from(quantity) + prorated_count;
                    let (Ok(new_last), Ok(new_qty)) =
                        (i32::try_from(new_last), i32::try_from(new_qty))
                    else {
                        return Err(SettleError::Invariant(format!(
                            "Proration invoice {invoice_id}: seat count out of range"
                        )));
                    };
                    sqlx::query(
                        r#"UPDATE subscriptions SET "lastBilledQuantity" = $2, quantity = $3 WHERE id = $1"#,
                    )
                    .bind(sid)
                    .bind(new_last)
                    .bind(new_qty)
                    .execute(&mut *tx)
                    .await?;
                    (
                        ApplyResult::Reactivated {
                            reactivated,
                            prorated_count,
                        },
                        users,
                    )
                }
                other => (
                    // Uang sudah masuk tapi seat tidak bisa dipulihkan → PAID saja (refund manual).
                    ApplyResult::PaidNoReactivation {
                        subscription_id,
                        subscription_status: other.map(|s| s.2),
                    },
                    Vec::new(),
                ),
            }
        }
        // CANCELLED: tidak diubah (uang sudah di-capture → alert refund manual).
        other => (
            ApplyResult::InvoiceClosed {
                status: other.to_owned(),
            },
            Vec::new(),
        ),
    };
    tx.commit().await?;
    Ok(result)
}

/// `userCompany.updateMany({ where: { id: { in: ids }, companyId, status: 0 }, data: { status: 1 } })`.
async fn reactivate_users(
    tx: &mut sqlx::PgConnection,
    audit: &AuditHandle,
    ids: &[i32],
    company_id: i32,
) -> Result<(u64, Vec<i32>), SettleError> {
    let ids_owned = ids.to_vec();
    let pre = audit
        .pre_read_many(
            Model::UserCompany,
            Lookup::filter(move |qb| {
                qb.push("id = ANY(")
                    .push_bind(ids_owned.clone())
                    .push(r#") AND "companyId" = "#)
                    .push_bind(company_id)
                    .push(" AND status = 0");
            }),
        )
        .await;
    let users: Vec<i32> = sqlx::query_scalar(
        r#"UPDATE "userCompany" SET status = 1
            WHERE id = ANY($1) AND "companyId" = $2 AND status = 0 RETURNING "userId""#,
    )
    .bind(ids)
    .bind(company_id)
    .fetch_all(&mut *tx)
    .await?;
    let count = users.len() as u64;
    audit
        .record_update_many(
            Model::UserCompany,
            pre,
            count,
            &Row::new().with("status", 1),
            json!({ "id": { "in": ids }, "companyId": company_id, "status": 0 }),
        )
        .await;
    Ok((count, users))
}

/// Compare-and-set status invoice → PAID (proration-payment.service.ts:396).
async fn claim(
    tx: &mut sqlx::PgConnection,
    invoice_id: i32,
    expected: &str,
) -> Result<(), SettleError> {
    let n = sqlx::query(
        r#"UPDATE invoices SET status = 'PAID'::"InvoiceStatus"
            WHERE id = $1 AND status = $2::"InvoiceStatus""#,
    )
    .bind(invoice_id)
    .bind(expected)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if n != 1 {
        return Err(SettleError::Invariant(format!(
            "Proration invoice {invoice_id}: expected status {expected} under lock, claim failed"
        )));
    }
    Ok(())
}

/// Node: proration-payment.service.ts:412 `reportOutcome` (Sentry → `tracing::error!`).
fn report_outcome(result: &ApplyResult, order_id: &str, number: &str, capture_id: Option<&str>) {
    match result {
        ApplyResult::Paid => {
            tracing::info!("Proration invoice {number} marked PAID (order {order_id})");
        }
        ApplyResult::AlreadyPaid => {
            tracing::info!("Proration invoice {number} already PAID (order {order_id})");
        }
        ApplyResult::Reactivated {
            reactivated,
            prorated_count,
        } => tracing::info!(
            "Expired proration invoice {number} paid (order {order_id}) — {reactivated} user(s) reactivated, +{prorated_count} billed seat(s)"
        ),
        ApplyResult::PaidNoReactivation {
            subscription_id,
            subscription_status,
        } => tracing::error!(
            order_id,
            capture_id,
            subscription_id,
            "Expired proration invoice {number} paid (order {order_id}) but subscription {subscription_id} is {} — users NOT reactivated, manual refund required",
            subscription_status.as_deref().unwrap_or("missing")
        ),
        ApplyResult::InvoiceClosed { status } => tracing::error!(
            order_id,
            capture_id,
            "PayPal order {order_id} captured but proration invoice {number} is {status} — not applied, manual refund required"
        ),
        ApplyResult::NotFound => {
            tracing::warn!("Proration invoice {number} disappeared before apply (order {order_id})")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_id_pattern() {
        assert!(is_paypal_order_id("5O190127TN364715T"));
        assert!(!is_paypal_order_id("5o190127tn364715t"));
        assert!(!is_paypal_order_id("ABC123"));
        assert!(!is_paypal_order_id("5O190127TN36&x=1"));
        assert!(!is_paypal_order_id(&"A".repeat(31)));
        assert!(is_paypal_order_id(&"A".repeat(30)));
    }

    #[test]
    fn return_status_mapping() {
        use Outcome::*;
        for o in [Paid, Reactivated, AlreadyPaid, PaidNoReactivation] {
            assert_eq!(proration_return_status(o), "success");
        }
        assert_eq!(proration_return_status(NotCompleted), "pending");
        for o in [NotFound, AmountMismatch, InvoiceClosed] {
            assert_eq!(proration_return_status(o), "failed");
        }
    }

    #[test]
    fn amount_and_capture_rules() {
        assert!(amount_matches(
            Some(&json!({"currency_code":"USD","value":"12.50"})),
            12.5
        ));
        assert!(amount_matches(
            Some(&json!({"currency_code":"USD","value":"12.5"})),
            12.499
        ));
        assert!(!amount_matches(
            Some(&json!({"currency_code":"EUR","value":"12.50"})),
            12.5
        ));
        assert!(!amount_matches(
            Some(&json!({"currency_code":"USD","value":12.5})),
            12.5
        ));
        assert!(!amount_matches(
            Some(&json!({"currency_code":"USD","value":"abc"})),
            12.5
        ));
        assert!(!amount_matches(None, 12.5));
        let order = json!({"status":"COMPLETED","purchase_units":[
            {"payments":{"captures":[{"id":"A","status":"PENDING"}]}},
            {"payments":{"captures":[{"id":"B","status":"COMPLETED"}]}}]});
        assert_eq!(
            find_completed_capture(&order).and_then(|c| c.get("id")),
            Some(&json!("B"))
        );
        assert!(find_completed_capture(&json!({"status":"COMPLETED"})).is_none());
    }
}
