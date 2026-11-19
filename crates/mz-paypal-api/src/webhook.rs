//! `POST /paypal/webhook` (paypal.controller.ts:326) + `processWebhookInBackground` dan
//! seluruh handler event.
//!
//! Sama dengan Node: tanda tangan WAJIB terverifikasi lewat PayPal sebelum event diproses
//! (400 palsu/cacat, 503 tidak bisa memverifikasi → PayPal mengirim ulang), respons 200
//! langsung, pemrosesan di latar. Idempotensi: baris `paypalWebhookEvents` (`eventId` unik);
//! event yang sudah `processed` dilewati, yang tersimpan tapi belum selesai diproses ulang.
//! Tulisan per handler TIDAK dalam satu transaksi (sama dengan Node); hanya pelunasan
//! PRORATION yang transaksional (lihat [`crate::proration`]).

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use chrono::NaiveDateTime;
use mz_audit::{ActorType, AuditHandle, Lookup, Model};
use mz_auth::analytics::{UpgradeToPro, spawn_upgrade_to_pro};
use mz_core::extract::{Validate, ValidatedJson};
use mz_core::jsfmt::num_string;
use mz_core::plan::is_paid_tier;
use mz_core::time::now_naive;
use mz_core::{ApiError, ApiResult, AppState};
use mz_paypal::PaypalClient;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{Postgres, QueryBuilder};

use crate::mail;
use crate::proration;
use crate::util::{
    format_dmy, iso_day, js_normalize, next_invoice_date, period_end, quantity_or, truthy_str,
};

type HResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Body mentah (`@Body() event: PayPalWebhookEvent` — interface, tanpa validasi DTO).
#[derive(Deserialize)]
#[serde(transparent)]
pub struct RawBody(pub Value);

impl Validate for RawBody {}

fn header<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
    h.get(name)
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
}

/// Node: src/paypal/paypal.controller.ts:326 (`POST /paypal/webhook`, 200)
pub async fn handle_webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    ValidatedJson(RawBody(event)): ValidatedJson<RawBody>,
) -> ApiResult<Json<Value>> {
    let Some(webhook_id) = state.config.var("PAYPAL_WEBHOOK_ID") else {
        tracing::error!(
            "PAYPAL_WEBHOOK_ID is not set — rejecting PayPal webhook (503 so PayPal retries)"
        );
        return Err(unavailable());
    };
    let names = [
        "paypal-transmission-id",
        "paypal-transmission-time",
        "paypal-cert-url",
        "paypal-auth-algo",
        "paypal-transmission-sig",
    ];
    let values: Vec<Option<&str>> = names.iter().map(|n| header(&headers, n)).collect();
    let event_id = event.get("id").and_then(Value::as_str).map(str::to_owned);
    let event_type = event
        .get("event_type")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let (
        [Some(tid), Some(time), Some(cert), Some(algo), Some(sig)],
        Some(event_id),
        Some(event_type),
    ) = (values.as_slice(), event_id, event_type)
    else {
        tracing::warn!("Rejected PayPal webhook: missing signature headers or event id");
        return Err(ApiError::bad_request("Invalid webhook"));
    };

    // axios mengirim ulang objek hasil JSON.parse: urutan kunci asli, angka gaya JS.
    let event = js_normalize(&event);
    let verified = PaypalClient::from_state(&state)
        .verify_webhook_signature(&webhook_id, tid, time, cert, algo, sig, &event)
        .await;
    match verified {
        Err(err) => {
            tracing::error!("Signature verification unavailable for {event_id}: {err}");
            return Err(unavailable());
        }
        Ok(false) => {
            tracing::warn!("Rejected PayPal webhook {event_id} ({event_type}): invalid signature");
            return Err(ApiError::bad_request("Invalid webhook signature"));
        }
        Ok(true) => {}
    }

    tracing::info!("Received PayPal webhook: {event_type}");
    // Balas 200 segera; proses di latar (mencegah timeout PayPal).
    let bg = state.clone();
    mz_core::bg::spawn(async move { process_event(&bg, &event).await });
    Ok(Json(json!({ "status": "received" })))
}

fn unavailable() -> ApiError {
    ApiError::message(
        StatusCode::SERVICE_UNAVAILABLE,
        "Webhook verification unavailable",
    )
}

/// Node: paypal.controller.ts:401 `processWebhookInBackground` — tidak pernah gagal; galat
/// dicatat dan event TIDAK ditandai `processed` (retry PayPal memprosesnya ulang).
pub async fn process_event(state: &AppState, event: &Value) {
    let id = event.get("id").and_then(Value::as_str).unwrap_or("?");
    if let Err(err) = process_inner(state, event).await {
        tracing::error!("Background webhook processing error for {id}: {err}");
    }
}

async fn process_inner(state: &AppState, event: &Value) -> HResult {
    let event_id = event
        .get("id")
        .and_then(Value::as_str)
        .ok_or("event id missing")?;
    let event_type = event
        .get("event_type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let existing: Option<bool> =
        sqlx::query_scalar(r#"SELECT processed FROM "paypalWebhookEvents" WHERE "eventId" = $1"#)
            .bind(event_id)
            .fetch_optional(&state.db)
            .await?;
    if existing == Some(true) {
        tracing::info!("Event {event_id} already processed, skipping");
        return Ok(());
    }
    if existing.is_none() {
        // Unique `eventId`: pengiriman kembar yang berlomba gagal di sini (sama dengan Node).
        let resource = event.get("resource");
        sqlx::query(
            r#"INSERT INTO "paypalWebhookEvents" ("eventId", "eventType", "resourceId", "resourceType", payload)
               VALUES ($1, $2, $3, $4, $5)"#,
        )
        .bind(event_id)
        .bind(event_type)
        .bind(resource.and_then(|r| r.get("id")).and_then(Value::as_str))
        .bind(event.get("resource_type").and_then(Value::as_str))
        .bind(serde_json::to_string(&js_normalize(event))?)
        .execute(&state.db)
        .await?;
    }

    // Pelaku audit: `system` (tanpa user; Node: store request webhook yang buffernya sudah flush).
    let audit = AuditHandle::run_as(state.db.clone(), ActorType::System, None);
    let resource = event.get("resource").cloned().unwrap_or(Value::Null);
    let cx = Cx {
        state,
        audit: &audit,
        resource: &resource,
        event_id,
    };
    match event_type {
        "BILLING.SUBSCRIPTION.ACTIVATED" => cx.subscription_activated().await?,
        "BILLING.SUBSCRIPTION.CANCELLED" => cx.subscription_cancelled().await?,
        "BILLING.SUBSCRIPTION.SUSPENDED" => cx.subscription_suspended().await?,
        "BILLING.SUBSCRIPTION.EXPIRED" => cx.subscription_expired().await?,
        "BILLING.SUBSCRIPTION.UPDATED" => cx.subscription_updated().await?,
        "PAYMENT.SALE.COMPLETED" => cx.payment_completed().await?,
        "PAYMENT.SALE.DENIED" | "PAYMENT.SALE.REFUNDED" => cx.payment_failed().await?,
        "CHECKOUT.ORDER.APPROVED" => cx.order_approved().await?,
        "PAYMENT.CAPTURE.COMPLETED" => cx.order_capture_completed().await?,
        other => tracing::info!("Unhandled event type: {other}"),
    }

    sqlx::query(r#"UPDATE "paypalWebhookEvents" SET processed = true WHERE "eventId" = $1"#)
        .bind(event_id)
        .execute(&state.db)
        .await?;
    tracing::info!("Event {event_id} ({event_type}) processed successfully");
    Ok(())
}

// ── data ─────────────────────────────────────────────────────────────────────

/// `subscriptions` + `pricingTier` + `company.isTrial` + user `userCompany` default pertama.
#[derive(sqlx::FromRow)]
struct Sub {
    id: i32,
    uuid: String,
    #[sqlx(rename = "companyId")]
    company_id: i32,
    quantity: i32,
    #[sqlx(rename = "pricePerUser")]
    price_per_user: f64,
    #[sqlx(rename = "billingInterval")]
    billing_interval: String,
    #[sqlx(rename = "paypalSubscriptionId")]
    paypal_subscription_id: Option<String>,
    #[sqlx(rename = "paypalPlanId")]
    paypal_plan_id: Option<String>,
    #[sqlx(rename = "paypalStatus")]
    paypal_status: Option<String>,
    #[sqlx(rename = "autoRenew")]
    auto_renew: bool,
    #[sqlx(rename = "endDate")]
    end_date: NaiveDateTime,
    #[sqlx(rename = "pendingPricingTierId")]
    pending_pricing_tier_id: Option<i32>,
    #[sqlx(rename = "pendingPaypalSubscriptionId")]
    pending_paypal_subscription_id: Option<String>,
    #[sqlx(rename = "pendingPaypalPlanId")]
    pending_paypal_plan_id: Option<String>,
    #[sqlx(rename = "billingSource")]
    billing_source: String,
    #[sqlx(rename = "tierName")]
    tier_name: Option<String>,
    #[sqlx(rename = "tierPrice")]
    tier_price: Option<f64>,
    #[sqlx(rename = "tierBasePrice")]
    tier_base_price: Option<f64>,
    #[sqlx(rename = "tierRetention")]
    tier_retention: Option<i32>,
    #[sqlx(rename = "isTrial")]
    is_trial: Option<bool>,
    #[sqlx(rename = "ownerId")]
    owner_id: Option<i32>,
    #[sqlx(rename = "ownerEmail")]
    owner_email: Option<String>,
    #[sqlx(rename = "ownerName")]
    owner_name: Option<String>,
}

impl Sub {
    fn is_manual(&self) -> bool {
        self.billing_source == "MANUAL"
    }

    /// `isPaidTier(subscription.pricingTier)`.
    fn tier_is_paid(&self) -> bool {
        self.tier_name.is_some()
            && is_paid_tier(
                self.tier_name.as_deref(),
                self.tier_price,
                self.tier_base_price,
            )
    }

    /// `subscription.pricePerUser || pricingTier.pricePerUserUsd || 0`.
    fn price_per_user(&self) -> f64 {
        [Some(self.price_per_user), self.tier_price]
            .into_iter()
            .flatten()
            .find(|p| *p != 0.0 && !p.is_nan())
            .unwrap_or(0.0)
    }

    fn owner(&self) -> Option<(i32, &str, &str)> {
        Some((
            self.owner_id?,
            self.owner_email.as_deref()?,
            self.owner_name.as_deref()?,
        ))
    }
}

#[derive(Clone, Copy)]
enum By {
    Paypal,
    Pending,
}

async fn find_sub(state: &AppState, by: By, paypal_id: &str) -> sqlx::Result<Option<Sub>> {
    let col = match by {
        By::Paypal => r#""paypalSubscriptionId""#,
        By::Pending => r#""pendingPaypalSubscriptionId""#,
    };
    let sql = format!(
        r#"SELECT s.id, s.uuid::text AS uuid, s."companyId", s.quantity, s."pricePerUser",
                  s."billingInterval", s."paypalSubscriptionId", s."paypalPlanId", s."paypalStatus",
                  s."autoRenew", s."endDate", s."pendingPricingTierId", s."pendingPaypalSubscriptionId",
                  s."pendingPaypalPlanId", s."billingSource"::text AS "billingSource",
                  t.name AS "tierName", t."pricePerUserUsd" AS "tierPrice", t."basePrice" AS "tierBasePrice",
                  t."screenshotRetentionDays" AS "tierRetention", c."isTrial" AS "isTrial",
                  o.id AS "ownerId", o.username AS "ownerEmail", o.name AS "ownerName"
             FROM subscriptions s
             LEFT JOIN "pricingTiers" t ON t.id = s."pricingTierId"
             LEFT JOIN company c ON c.id = s."companyId"
             LEFT JOIN LATERAL (
                SELECT u.id, u.username, u.name FROM "userCompany" uc JOIN users u ON u.id = uc."userId"
                 WHERE uc."companyId" = s."companyId" AND uc."isDefault" = true
                 ORDER BY uc.id LIMIT 1) o ON true
            WHERE s.{col} = $1
            ORDER BY s.id LIMIT 1"#
    );
    sqlx::query_as(&sql)
        .bind(paypal_id)
        .fetch_optional(&state.db)
        .await
}

#[derive(sqlx::FromRow)]
struct Tier {
    id: i32,
    name: String,
    #[sqlx(rename = "pricePerUserUsd")]
    price_per_user_usd: f64,
    #[sqlx(rename = "basePrice")]
    base_price: f64,
    #[sqlx(rename = "billingInterval")]
    billing_interval: String,
    #[sqlx(rename = "paypalPlanId")]
    paypal_plan_id: Option<String>,
    #[sqlx(rename = "screenshotRetentionDays")]
    screenshot_retention_days: Option<i32>,
}

async fn find_tier(state: &AppState, id: Option<i32>) -> sqlx::Result<Option<Tier>> {
    let Some(id) = id else { return Ok(None) };
    sqlx::query_as(
        r#"SELECT id, name, "pricePerUserUsd", "basePrice", "billingInterval", "paypalPlanId",
                  "screenshotRetentionDays"
             FROM "pricingTiers" WHERE id = $1"#,
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await
}

#[derive(sqlx::FromRow)]
struct Invoice {
    id: i32,
    #[sqlx(rename = "invoiceNumber")]
    invoice_number: String,
    status: String,
    #[sqlx(rename = "invoiceType")]
    invoice_type: String,
    #[sqlx(rename = "paymentUrl")]
    payment_url: String,
}

/// `invoices.findFirst({ where: { subscriptionId, [status: UNPAID] }, orderBy: { createdAt: 'desc' } })`.
async fn latest_invoice(
    state: &AppState,
    subscription_id: i32,
    only_unpaid: bool,
) -> sqlx::Result<Option<Invoice>> {
    sqlx::query_as(
        r#"SELECT id, "invoiceNumber", status::text AS status, "invoiceType"::text AS "invoiceType",
                  "paymentUrl"
             FROM invoices
            WHERE "subscriptionId" = $1 AND (NOT $2::boolean OR status = 'UNPAID'::"InvoiceStatus")
            ORDER BY "createdAt" DESC, id DESC LIMIT 1"#,
    )
    .bind(subscription_id)
    .bind(only_unpaid)
    .fetch_optional(&state.db)
    .await
}

fn to_i32(n: i64) -> HResult<i32> {
    i32::try_from(n).map_err(|_| format!("value {n} out of Int range").into())
}

/// `x.toString()` untuk harga di deskripsi (`$${pricePerUser}`).
fn price(x: f64) -> String {
    num_string(x)
}

// ── handler ──────────────────────────────────────────────────────────────────

struct Cx<'a> {
    state: &'a AppState,
    audit: &'a AuditHandle,
    resource: &'a Value,
    event_id: &'a str,
}

impl Cx<'_> {
    /// `skipIfManual` (paypal.controller.ts:491).
    fn skip_if_manual(&self, sub: &Sub) -> bool {
        if !sub.is_manual() {
            return false;
        }
        tracing::warn!(
            "PayPal webhook {} ignored: subscription {} is MANUAL billing",
            self.event_id,
            sub.uuid
        );
        true
    }

    /// `skipIfManualOrder` (paypal.controller.ts:505): resolve lewat invoice ber-`paypalPaymentId`.
    async fn skip_if_manual_order(&self, order_id: &str) -> HResult<bool> {
        let row: Option<(String, String)> = sqlx::query_as(
            r#"SELECT s.uuid::text, s."billingSource"::text FROM invoices i
                 JOIN subscriptions s ON s.id = i."subscriptionId"
                WHERE i."paypalPaymentId" = $1 ORDER BY i.id LIMIT 1"#,
        )
        .bind(order_id)
        .fetch_optional(&self.state.db)
        .await?;
        Ok(match row {
            Some((uuid, source)) if source == "MANUAL" => {
                tracing::warn!(
                    "PayPal webhook {} ignored: subscription {uuid} is MANUAL billing",
                    self.event_id
                );
                true
            }
            _ => false,
        })
    }

    /// `resource.id` sebagai id langganan PayPal.
    fn resource_id(&self) -> Option<&str> {
        self.resource.get("id").and_then(Value::as_str)
    }

    /// `resource.id` — tanpa id, Node mencari `{ paypalSubscriptionId: undefined }` (= baris
    /// PERTAMA tabel). Di sini dianggap tidak ditemukan.
    async fn sub_by_resource(&self, by: By) -> HResult<Option<Sub>> {
        match self.resource_id() {
            Some(id) => Ok(find_sub(self.state, by, id).await?),
            None => Ok(None),
        }
    }

    /// `company.update({ where: { id }, data })` lewat audit (`company` ber-audit).
    async fn company_update(
        &self,
        company_id: i32,
        from_tier: Option<i32>,
        build: impl FnOnce(&mut QueryBuilder<'_, Postgres>),
    ) -> HResult {
        let pre = self
            .audit
            .pre_read(Model::Company, Lookup::Id(company_id))
            .await;
        let mut qb = QueryBuilder::<Postgres>::new("UPDATE company SET ");
        build(&mut qb);
        if let Some(tier_id) = from_tier {
            qb.push(r#" FROM "pricingTiers" t WHERE t.id = "#)
                .push_bind(tier_id)
                .push(" AND company.id = ");
        } else {
            qb.push(" WHERE company.id = ");
        }
        qb.push_bind(company_id);
        let n = qb.build().execute(&self.state.db).await?.rows_affected();
        if n == 0 {
            // Prisma `update` melempar P2025 bila baris tidak ada.
            return Err(format!("company {company_id} not found").into());
        }
        if let Some(row) = self
            .audit
            .result_row(&self.state.db, Model::Company, Lookup::Id(company_id))
            .await
        {
            self.audit.record_update(Model::Company, pre, &row).await;
        }
        Ok(())
    }

    fn analytics(&self, input: UpgradeToPro) {
        spawn_upgrade_to_pro(self.state, self.audit, input);
    }

    /// Node: paypal.controller.ts:518 `handleSubscriptionActivated`.
    async fn subscription_activated(&self) -> HResult {
        let sid = self.resource_id().unwrap_or("undefined");
        tracing::info!("Subscription activated: {sid}");
        let mut sub = self.sub_by_resource(By::Paypal).await?;
        if sub.is_none() {
            let pending = self.sub_by_resource(By::Pending).await?;
            if let Some(p) = &pending {
                if self.skip_if_manual(p) {
                    return Ok(());
                }
                // Perubahan plan tertunda: tier/tanggal diterapkan di PAYMENT.SALE.COMPLETED.
                tracing::info!(
                    "Pending plan change activated: {sid} — waiting for PAYMENT.SALE.COMPLETED"
                );
                return Ok(());
            }
            sub = pending;
        }
        let Some(sub) = sub else {
            tracing::warn!("Subscription not found: {sid}");
            return Ok(());
        };
        if self.skip_if_manual(&sub) {
            return Ok(());
        }

        let quantity = to_i32(quantity_or(self.resource.get("quantity"), sub.quantity))?;
        let price_per_user = sub.price_per_user();
        let interval = if sub.billing_interval.is_empty() {
            "MONTH".to_owned()
        } else {
            sub.billing_interval.clone()
        };
        let yearly = interval == "YEAR";

        // autoRenew dibaca ulang: ACTIVATED yang terlambat tidak boleh menimpa cancel terbaru.
        let auto_renew: bool =
            sqlx::query_scalar(r#"SELECT "autoRenew" FROM subscriptions WHERE id = $1"#)
                .bind(sub.id)
                .fetch_one(&self.state.db)
                .await?;
        // Hanya status & quantity; tanggal dikelola PAYMENT.SALE.COMPLETED.
        sqlx::query(
            r#"UPDATE subscriptions SET status = 'ACTIVE'::"SubscriptionStatus", "paypalStatus" = 'ACTIVE',
                   quantity = $2, "autoRenew" = $3, "lastBilledQuantity" = $2
             WHERE id = $1"#,
        )
        .bind(sub.id)
        .bind(quantity)
        .bind(auto_renew)
        .execute(&self.state.db)
        .await?;
        // isTrial=false hanya di handlePaymentCompleted (guard tanggalnya membaca isTrial).
        self.company_update(sub.company_id, None, |qb| {
            qb.push(r#""maxUsers" = "#).push_bind(quantity);
        })
        .await?;

        // Tanggal hanya untuk email.
        let start = now_naive();
        let end = period_end(start, yearly);

        let is_trial = sub.is_trial.unwrap_or(false);
        let invoice = latest_invoice(self.state, sub.id, true).await?;
        if let Some(inv) = invoice.as_ref().filter(|_| !is_trial) {
            sqlx::query(r#"UPDATE invoices SET status = 'PAID'::"InvoiceStatus" WHERE id = $1"#)
                .bind(inv.id)
                .execute(&self.state.db)
                .await?;
        }

        let multiplier = if yearly { 12.0 } else { 1.0 };
        let total = f64::from(quantity) * price_per_user * multiplier;
        if !is_trial && let Some((_, email, name)) = sub.owner() {
            let tier_name = if yearly {
                format!(
                    "{quantity} user(s) @ ${}/user/month (billed yearly)",
                    price(price_per_user)
                )
            } else {
                format!("{quantity} user(s) @ ${}/user", price(price_per_user))
            };
            mail::send(
                self.state,
                mail::invoice_confirmation(mail::InvoiceConfirmation {
                    email: email.to_owned(),
                    name: name.to_owned(),
                    tier_name,
                    invoice_number: invoice
                        .as_ref()
                        .map(|i| i.invoice_number.clone())
                        .filter(|n| !n.is_empty())
                        .unwrap_or_else(|| "N/A".into()),
                    period: format!("{} - {}", format_dmy(start), format_dmy(end)),
                    total_price: total,
                    expired_at: format_dmy(end),
                }),
            );
        }

        // Hanya trial→berbayar yang dihitung upgrade (isTrial dibaca SEBELUM di-false-kan).
        self.analytics(UpgradeToPro {
            company_id: sub.company_id,
            is_first_paid_transition: is_trial && sub.tier_is_paid(),
            tier_name: sub.tier_name.clone(),
            billing_interval: Some(interval),
            payment_provider: "paypal",
            value: Some(total),
            currency: Some("USD".into()),
        });
        Ok(())
    }

    /// Node: paypal.controller.ts:689 `handleSubscriptionCancelled`.
    async fn subscription_cancelled(&self) -> HResult {
        let sid = self.resource_id().unwrap_or("undefined");
        tracing::info!("Subscription cancelled: {sid}");
        let Some(sub) = self.sub_by_resource(By::Paypal).await? else {
            tracing::warn!("Subscription not found: {sid}");
            return Ok(());
        };
        if self.skip_if_manual(&sub) {
            return Ok(());
        }
        let expected = sub.pending_paypal_subscription_id.is_some() || !sub.auto_renew;
        if sub.pending_paypal_subscription_id.is_some() {
            tracing::info!(
                "Intentional plan-change cancellation for subscription {} — status preserved",
                sub.id
            );
        } else if !sub.auto_renew {
            tracing::info!(
                "Expected cancellation for subscription {} (autoRenew=false)",
                sub.id
            );
        }
        let sql = if expected {
            r#"UPDATE subscriptions SET "paypalStatus" = 'CANCELLED' WHERE id = $1"#
        } else {
            tracing::warn!(
                "Unexpected cancellation for subscription {} (autoRenew was true)",
                sub.id
            );
            r#"UPDATE subscriptions SET status = 'CANCELLED'::"SubscriptionStatus",
                   "paypalStatus" = 'CANCELLED', "autoRenew" = false WHERE id = $1"#
        };
        sqlx::query(sql)
            .bind(sub.id)
            .execute(&self.state.db)
            .await?;
        Ok(())
    }

    /// Node: paypal.controller.ts:744 `handleSubscriptionSuspended`.
    async fn subscription_suspended(&self) -> HResult {
        let sid = self.resource_id().unwrap_or("undefined");
        tracing::info!("Subscription suspended: {sid}");
        let Some(sub) = self.sub_by_resource(By::Paypal).await? else {
            tracing::warn!("Subscription not found: {sid}");
            return Ok(());
        };
        if self.skip_if_manual(&sub) {
            return Ok(());
        }
        let sql = if sub.auto_renew {
            tracing::warn!("Subscription {sid} suspended unexpectedly — setting SUSPENDED");
            r#"UPDATE subscriptions SET status = 'SUSPENDED'::"SubscriptionStatus", "paypalStatus" = 'SUSPENDED'
                WHERE id = $1"#
        } else {
            // Cancel anggun: tetap ACTIVE sampai endDate (cron expiry yang menonaktifkan).
            tracing::info!(
                "Subscription {sid} suspended (graceful cancel) — keeping ACTIVE until endDate"
            );
            r#"UPDATE subscriptions SET "paypalStatus" = 'SUSPENDED' WHERE id = $1"#
        };
        sqlx::query(sql)
            .bind(sub.id)
            .execute(&self.state.db)
            .await?;
        Ok(())
    }

    /// Node: paypal.controller.ts:785 `handleSubscriptionExpired`.
    async fn subscription_expired(&self) -> HResult {
        let sid = self.resource_id().unwrap_or("undefined");
        tracing::info!("Subscription expired: {sid}");
        let Some(sub) = self.sub_by_resource(By::Paypal).await? else {
            tracing::warn!("Subscription not found: {sid}");
            return Ok(());
        };
        if self.skip_if_manual(&sub) {
            return Ok(());
        }
        sqlx::query(
            r#"UPDATE subscriptions SET status = 'INACTIVE'::"SubscriptionStatus", "paypalStatus" = 'EXPIRED'
                WHERE id = $1"#,
        )
        .bind(sub.id)
        .execute(&self.state.db)
        .await?;
        Ok(())
    }

    /// Node: paypal.controller.ts:809 `handleSubscriptionUpdated`.
    async fn subscription_updated(&self) -> HResult {
        let sid = self.resource_id().unwrap_or("undefined");
        tracing::info!("Subscription updated: {sid}");
        let Some(sub) = self.sub_by_resource(By::Paypal).await? else {
            tracing::warn!("Subscription not found: {sid}");
            return Ok(());
        };
        if self.skip_if_manual(&sub) {
            return Ok(());
        }
        let quantity = to_i32(quantity_or(self.resource.get("quantity"), sub.quantity))?;
        let new_status = truthy_str(self.resource.get("status"))
            .map(str::to_owned)
            .or_else(|| sub.paypal_status.clone());
        let new_plan = truthy_str(self.resource.get("plan_id"))
            .map(str::to_owned)
            .or_else(|| sub.paypal_plan_id.clone());

        // Revisi plan tertunda yang sudah di-approve.
        if sub.pending_paypal_plan_id.is_some() && sub.pending_paypal_plan_id == new_plan {
            tracing::info!(
                "Pending upgrade approved for subscription {} — applying tier change",
                sub.id
            );
            let Some(tier) = find_tier(self.state, sub.pending_pricing_tier_id).await? else {
                tracing::error!(
                    "Pending pricing tier not found: {:?}",
                    sub.pending_pricing_tier_id
                );
                return Ok(());
            };
            let interval = if tier.billing_interval.is_empty() {
                "MONTH"
            } else {
                tier.billing_interval.as_str()
            };
            sqlx::query(
                r#"UPDATE subscriptions SET "pricingTierId" = $2, "pricePerUser" = $3, "billingInterval" = $4,
                       "paypalPlanId" = $5, "paypalStatus" = $6, quantity = $7,
                       "pendingPricingTierId" = NULL, "pendingPaypalSubscriptionId" = NULL,
                       "pendingPaypalPlanId" = NULL
                 WHERE id = $1"#,
            )
            .bind(sub.id)
            .bind(tier.id)
            .bind(tier.price_per_user_usd)
            .bind(interval)
            .bind(&tier.paypal_plan_id)
            .bind(&new_status)
            .bind(quantity)
            .execute(&self.state.db)
            .await?;
            let tier_id = tier.id;
            self.company_update(sub.company_id, Some(tier_id), |qb| {
                qb.push(
                    r#""maxUsers" = t."maxUsers", "maxProject" = t."maxProject",
                       "useIdleDetection" = t."useIdleDetection", "useCheatDetection" = t."useCheatDetection",
                       "useReports" = t."useReports", "useTimesheetApproval" = t."useTimesheetApproval",
                       "useActivityLevel" = t."useActivityLevel", "useIntegrations" = t."useIntegrations",
                       "screenshotRetentionDays" = t."screenshotRetentionDays""#,
                );
            })
            .await?;
            // Override paket per company (spec 007) menang atas batas tier yang baru disalin.
            mz_admin_company::plan_limits::reapply_plan_override_after_tier_sync(
                self.state,
                self.audit,
                sub.company_id,
            )
            .await;
            if tier.screenshot_retention_days.is_none() && sub.tier_retention.is_some() {
                mz_billing::cleanup_compressed_screenshots(self.state, sub.company_id).await;
            }
            tracing::info!(
                "Upgrade applied: subscription {} switched to tier {}",
                sub.id,
                tier.name
            );
            return Ok(());
        }

        sqlx::query(
            r#"UPDATE subscriptions SET "paypalStatus" = $2, "paypalPlanId" = $3, quantity = $4 WHERE id = $1"#,
        )
        .bind(sub.id)
        .bind(&new_status)
        .bind(&new_plan)
        .bind(quantity)
        .execute(&self.state.db)
        .await?;
        self.company_update(sub.company_id, None, |qb| {
            qb.push(r#""maxUsers" = "#).push_bind(quantity);
        })
        .await?;

        // Invoice terbaru (status apa pun) diperbarui, tidak pernah membuat baru.
        if let Some(inv) = latest_invoice(self.state, sub.id, false).await? {
            let approval = self
                .resource
                .get("links")
                .and_then(Value::as_array)
                .and_then(|links| {
                    links
                        .iter()
                        .find(|l| l.get("rel").and_then(Value::as_str) == Some("approve"))
                })
                .and_then(|l| truthy_str(l.get("href")))
                .map(str::to_owned);
            // PERBAIKAN (keputusan pemilik 2026-11-17, deviasi): Node menandai invoice terbaru PAID bila
            // status langganan ACTIVE (`paypal.controller.ts:928-931`), apa pun status invoicenya — invoice
            // UNPAID (perpanjangan/proration) bisa tercatat lunas tanpa pembayaran. Di Rust event UPDATED
            // tidak mengubah status invoice; pelunasan hanya dari event pembayaran.
            let status = inv.status.clone();
            sqlx::query(
                r#"UPDATE invoices SET status = $2::"InvoiceStatus", "paymentUrl" = $3,
                       "paypalPaymentId" = COALESCE($4, "paypalPaymentId")
                 WHERE id = $1"#,
            )
            .bind(inv.id)
            .bind(status)
            .bind(approval.unwrap_or(inv.payment_url))
            .bind(self.resource_id())
            .execute(&self.state.db)
            .await?;
            tracing::info!(
                "Updated invoice {} for subscription update",
                inv.invoice_number
            );
        }
        tracing::info!("Updated subscription quantity to {quantity}");
        Ok(())
    }

    /// Node: paypal.controller.ts:945 `handlePaymentCompleted`.
    async fn payment_completed(&self) -> HResult {
        let agreement = truthy_str(self.resource.get("billing_agreement_id"));
        tracing::info!(
            "Payment completed for subscription: {}",
            agreement.unwrap_or("undefined")
        );
        let Some(agreement) = agreement else {
            tracing::warn!("No billing agreement ID in payment");
            return Ok(());
        };
        let mut pending = false;
        let mut sub = find_sub(self.state, By::Paypal, agreement).await?;
        if sub.is_none() {
            sub = find_sub(self.state, By::Pending, agreement).await?;
            pending = sub.is_some();
        }
        let Some(sub) = sub else {
            tracing::warn!("Subscription not found: {agreement}");
            return Ok(());
        };
        if self.skip_if_manual(&sub) {
            return Ok(());
        }
        if pending {
            return self.apply_pending_plan_change(&sub).await;
        }

        let sale_id = self.resource_id();
        let existing = latest_invoice(self.state, sub.id, true).await?;

        // PRORATION: hanya tandai PAID, jangan perpanjang tanggal.
        if let Some(inv) = existing.as_ref().filter(|i| i.invoice_type == "PRORATION") {
            sqlx::query(
                r#"UPDATE invoices SET status = 'PAID'::"InvoiceStatus",
                       "paypalPaymentId" = COALESCE($2, "paypalPaymentId") WHERE id = $1"#,
            )
            .bind(inv.id)
            .bind(sale_id)
            .execute(&self.state.db)
            .await?;
            tracing::info!(
                "Proration invoice {} marked PAID via webhook — no date extension",
                inv.invoice_number
            );
            return Ok(());
        }

        let interval = if sub.billing_interval.is_empty() {
            "MONTH".to_owned()
        } else {
            sub.billing_interval.clone()
        };
        let yearly = interval == "YEAR";
        let is_trial = sub.is_trial.unwrap_or(false);
        let now = now_naive();

        // Pembayaran sebelum siklus berakhir (non-trial) hanya diproses bila ada invoice tertunda.
        if !is_trial && now < sub.end_date {
            match &existing {
                None => {
                    tracing::info!(
                        "Pre-cycle-end payment for non-trial sub {} — skipping (no pending invoice, endDate: {})",
                        sub.id,
                        sub.end_date
                    );
                    return Ok(());
                }
                Some(inv) => tracing::info!(
                    "Renewal payment within cycle for sub {} — processing pending invoice {} (endDate: {})",
                    sub.id,
                    inv.invoice_number,
                    sub.end_date
                ),
            }
        }

        let paid_count: i64 = sqlx::query_scalar(
            r#"SELECT COUNT(*) FROM invoices WHERE "subscriptionId" = $1 AND status = 'PAID'::"InvoiceStatus""#,
        )
        .bind(sub.id)
        .fetch_one(&self.state.db)
        .await?;
        let first_payment = paid_count == 0;

        let start = now;
        let end = period_end(start, yearly);
        let next_invoice = next_invoice_date(now, end, yearly);
        sqlx::query(
            r#"UPDATE subscriptions SET "startDate" = $2, "endDate" = $3, "nextInvoiceDate" = $4,
                   status = 'ACTIVE'::"SubscriptionStatus" WHERE id = $1"#,
        )
        .bind(sub.id)
        .bind(start)
        .bind(end)
        .bind(next_invoice)
        .execute(&self.state.db)
        .await?;
        // SATU-SATUNYA tempat isTrial=false.
        let sub_qty = sub.quantity;
        self.company_update(sub.company_id, None, |qb| {
            qb.push(r#""isTrial" = false, "maxUsers" = "#)
                .push_bind(sub_qty);
        })
        .await?;

        let quantity = if sub.quantity == 0 { 1 } else { sub.quantity };
        let price_per_user = sub.price_per_user();
        self.analytics(UpgradeToPro {
            company_id: sub.company_id,
            is_first_paid_transition: is_trial && first_payment && sub.tier_is_paid(),
            tier_name: sub.tier_name.clone(),
            billing_interval: Some(interval.clone()),
            payment_provider: "paypal",
            value: Some(f64::from(quantity) * price_per_user),
            currency: Some("USD".into()),
        });

        let owner = sub.owner();
        if is_trial && first_payment {
            // Trial→berbayar: invoice dibuat sekarang dan langsung PAID.
            let number = format!(
                "INV-{}-{}",
                now.format("%Y%m%d"),
                now.and_utc().timestamp_millis()
            );
            let multiplier = if yearly { 12.0 } else { 1.0 };
            let unit_price = sub.tier_base_price.unwrap_or(0.0) * multiplier;
            let sub_price = unit_price * f64::from(quantity);
            let label = if yearly { "Yearly" } else { "Monthly" };
            let invoice_id: i32 = sqlx::query_scalar(
                r#"INSERT INTO invoices ("subscriptionId", "invoiceNumber", "paymentUrl", "subPrice", "taxPrice",
                       "totalPrice", status, "paypalPaymentId", "expiredAt", "paymentProvider")
                   VALUES ($1, $2, '', $3, 0, $3, 'PAID'::"InvoiceStatus", $4, $5, 'paypal') RETURNING id"#,
            )
            .bind(sub.id)
            .bind(&number)
            .bind(sub_price)
            .bind(sale_id)
            .bind(end)
            .fetch_one(&self.state.db)
            .await?;
            let description = format!(
                "{label} Subscription - {quantity} user(s) @ ${}/user{}",
                price(price_per_user),
                if yearly { "/month (billed yearly)" } else { "" }
            );
            sqlx::query(
                r#"INSERT INTO "invoiceItems" ("invoiceId", description, quantity, "unitPrice", "totalPrice")
                   VALUES ($1, $2, $3, $4, $5)"#,
            )
            .bind(invoice_id)
            .bind(description)
            .bind(f64::from(quantity))
            .bind(unit_price)
            .bind(sub_price)
            .execute(&self.state.db)
            .await?;
            tracing::info!(
                "Trial→paid: invoice {number} created and marked PAID for subscription {}",
                sub.id
            );
            if let Some((_, email, name)) = owner {
                mail::send(
                    self.state,
                    mail::invoice_confirmation(mail::InvoiceConfirmation {
                        email: email.to_owned(),
                        name: name.to_owned(),
                        tier_name: format!(
                            "{label} - {quantity} user(s) @ ${}/user",
                            price(price_per_user)
                        ),
                        invoice_number: number,
                        period: format!("{} – {}", iso_day(start), iso_day(end)),
                        total_price: sub_price,
                        expired_at: iso_day(end),
                    }),
                );
            }
        } else if let Some(inv) = &existing {
            sqlx::query(
                r#"UPDATE invoices SET status = 'PAID'::"InvoiceStatus",
                       "paypalPaymentId" = COALESCE($2, "paypalPaymentId"), "expiredAt" = $3
                 WHERE id = $1"#,
            )
            .bind(inv.id)
            .bind(sale_id)
            .bind(end)
            .execute(&self.state.db)
            .await?;
            tracing::info!(
                "Invoice {} marked as PAID — startDate: {start}, endDate: {end} ({})",
                inv.invoice_number,
                if yearly { "yearly" } else { "monthly" }
            );
        } else {
            tracing::info!(
                "No UNPAID invoice for subscription {} — startDate: {start}, endDate: {end}",
                sub.id
            );
        }

        if first_payment && let Some((_, email, name)) = owner {
            mail::send(self.state, mail::welcome_to_the_team(email, name));
            tracing::info!("Welcome email sent to {email} (first payment)");
        }
        Ok(())
    }

    /// Cabang `isPendingPlanChange` handlePaymentCompleted (paypal.controller.ts:1001).
    async fn apply_pending_plan_change(&self, sub: &Sub) -> HResult {
        tracing::info!("Applying pending plan change for subscription {}", sub.id);
        let Some(tier) = find_tier(self.state, sub.pending_pricing_tier_id).await? else {
            tracing::error!(
                "Pending pricing tier not found: {:?}",
                sub.pending_pricing_tier_id
            );
            return Ok(());
        };

        // Batalkan langganan PayPal LAMA (hanya bila masih bisa dibatalkan).
        if let Some(old) = &sub.paypal_subscription_id {
            if matches!(sub.paypal_status.as_deref(), Some("ACTIVE" | "SUSPENDED")) {
                if let Err(err) = PaypalClient::from_state(self.state)
                    .cancel_subscription(
                        old,
                        Some("Plan changed — old subscription replaced after payment"),
                    )
                    .await
                {
                    tracing::warn!("Failed to cancel old PayPal sub on plan change: {err}");
                }
            } else {
                tracing::info!(
                    "Skipping cancel for old sub {old} — status: {} (not cancellable)",
                    sub.paypal_status.as_deref().unwrap_or("null")
                );
            }
        }

        let interval = if tier.billing_interval.is_empty() {
            "MONTH".to_owned()
        } else {
            tier.billing_interval.clone()
        };
        let yearly = interval == "YEAR";
        let now = now_naive();
        let start = now;
        let end = period_end(start, yearly);
        let next_invoice = next_invoice_date(now, end, yearly);
        let quantity = if sub.quantity == 0 { 1 } else { sub.quantity };

        sqlx::query(
            r#"UPDATE subscriptions SET "paypalSubscriptionId" = $2, "paypalPlanId" = $3, "pricingTierId" = $4,
                   "pricePerUser" = $5, "billingInterval" = $6, "paypalStatus" = 'ACTIVE',
                   status = 'ACTIVE'::"SubscriptionStatus", "autoRenew" = true, "startDate" = $7,
                   "endDate" = $8, "nextInvoiceDate" = $9, quantity = $10, "lastBilledQuantity" = $10,
                   "pendingPricingTierId" = NULL, "pendingPaypalSubscriptionId" = NULL,
                   "pendingPaypalPlanId" = NULL
             WHERE id = $1"#,
        )
        .bind(sub.id)
        .bind(&sub.pending_paypal_subscription_id)
        .bind(&sub.pending_paypal_plan_id)
        .bind(tier.id)
        .bind(tier.price_per_user_usd)
        .bind(&interval)
        .bind(start)
        .bind(end)
        .bind(next_invoice)
        .bind(quantity)
        .execute(&self.state.db)
        .await?;

        let tier_id = tier.id;
        self.company_update(sub.company_id, Some(tier_id), |qb| {
            qb.push(
                r#""maxUsers" = t."maxUsers", "maxProject" = t."maxProject",
                   "screenshotResolution" = t."screenshotResolution",
                   "screenshotRetentionDays" = t."screenshotRetentionDays",
                   "reportExportsPerMonth" = t."reportExportsPerMonth", "reportExportsUsedThisMonth" = 0,
                   "reportExportsResetAt" = "#,
            )
            .push_bind(now)
            .push(
                r#", "dataHistoryMonths" = t."dataHistoryMonths",
                   "budgetAlertDeliveryChannel" = t."budgetAlertDeliveryChannel",
                   "clientProofExportAllowed" = t."clientProofExportAllowed""#,
            );
        })
        .await?;
        mz_admin_company::plan_limits::reapply_plan_override_after_tier_sync(
            self.state,
            self.audit,
            sub.company_id,
        )
        .await;
        if tier.screenshot_retention_days.is_none() && sub.tier_retention.is_some() {
            mz_billing::cleanup_compressed_screenshots(self.state, sub.company_id).await;
        }

        // `sub` = tier LAMA (dibaca sebelum update): Free→berbayar ≠ ganti antar tier berbayar.
        self.analytics(UpgradeToPro {
            company_id: sub.company_id,
            is_first_paid_transition: !sub.tier_is_paid()
                && is_paid_tier(
                    Some(&tier.name),
                    Some(tier.price_per_user_usd),
                    Some(tier.base_price),
                ),
            tier_name: Some(tier.name.clone()),
            billing_interval: Some(interval.clone()),
            payment_provider: "paypal",
            value: Some(f64::from(quantity) * tier.price_per_user_usd),
            currency: Some("USD".into()),
        });

        // Perubahan tier tampil di GET /auth/me berikutnya.
        if let Some((uid, _, _)) = sub.owner() {
            mz_core::invalidation::forget_user_me(self.state, uid).await;
        }

        let sale_id = self.resource_id();
        if let Some(inv) = latest_invoice(self.state, sub.id, true).await? {
            sqlx::query(
                r#"UPDATE invoices SET status = 'PAID'::"InvoiceStatus",
                       "paypalPaymentId" = COALESCE($2, "paypalPaymentId"), "expiredAt" = $3
                 WHERE id = $1"#,
            )
            .bind(inv.id)
            .bind(sale_id)
            .bind(end)
            .execute(&self.state.db)
            .await?;
            tracing::info!(
                "Plan change invoice {} marked PAID — tier: {}, endDate: {end}",
                inv.invoice_number,
                tier.name
            );
        } else {
            // Cadangan: buat invoice PAID baru.
            let multiplier = if yearly { 12.0 } else { 1.0 };
            let sub_price = f64::from(quantity) * tier.price_per_user_usd * multiplier;
            let number = format!(
                "INV-{}-{}",
                start.format("%d%m%Y"),
                now.and_utc().timestamp_millis().div_euclid(1000)
            );
            let label = if yearly { "Yearly" } else { "Monthly" };
            let invoice_id: i32 = sqlx::query_scalar(
                r#"INSERT INTO invoices ("subscriptionId", "invoiceNumber", "paymentUrl", "subPrice", "taxPrice",
                       "totalPrice", status, "paymentProvider", "paypalPaymentId", "expiredAt")
                   VALUES ($1, $2, '', $3, 0, $3, 'PAID'::"InvoiceStatus", 'paypal', $4, $5) RETURNING id"#,
            )
            .bind(sub.id)
            .bind(&number)
            .bind(sub_price)
            .bind(sale_id)
            .bind(end)
            .fetch_one(&self.state.db)
            .await?;
            let description = format!(
                "{label} Subscription (plan change to {}) - {quantity} user(s) @ ${}/user{}",
                tier.name,
                price(tier.price_per_user_usd),
                if yearly { "/month (billed yearly)" } else { "" }
            );
            sqlx::query(
                r#"INSERT INTO "invoiceItems" ("invoiceId", description, quantity, "unitPrice", "totalPrice")
                   VALUES ($1, $2, $3, $4, $5)"#,
            )
            .bind(invoice_id)
            .bind(description)
            .bind(f64::from(quantity))
            .bind(tier.price_per_user_usd * multiplier)
            .bind(sub_price)
            .execute(&self.state.db)
            .await?;
            tracing::info!(
                "Plan change fallback invoice created PAID — tier: {}, endDate: {end}",
                tier.name
            );
        }
        tracing::info!(
            "Plan change applied: subscription {} → tier {}, startDate: {start}, endDate: {end}",
            sub.id,
            tier.name
        );
        Ok(())
    }

    /// Node: paypal.controller.ts:1455 `handleOrderApproved` — capture (idempoten) lalu apply.
    async fn order_approved(&self) -> HResult {
        let Some(order_id) = self.resource_id() else {
            tracing::warn!("CHECKOUT.ORDER.APPROVED without order id");
            return Ok(());
        };
        if self.skip_if_manual_order(order_id).await? {
            return Ok(());
        }
        let result = proration::capture_and_apply(self.state, order_id).await?;
        tracing::info!("Order approved {order_id}: {}", result.outcome.as_str());
        Ok(())
    }

    /// Node: paypal.controller.ts:1477 `handleOrderCaptureCompleted` — `resource` = CAPTURE;
    /// id order di `supplementary_data.related_ids.order_id`. Tanpa capture ulang.
    async fn order_capture_completed(&self) -> HResult {
        let order_id = self
            .resource
            .get("supplementary_data")
            .and_then(|s| s.get("related_ids"))
            .and_then(|r| r.get("order_id"))
            .and_then(Value::as_str);
        let capture_id = self.resource.get("id");
        tracing::info!(
            "Order capture completed: capture {:?}, order {:?}",
            capture_id,
            order_id
        );
        let Some(order_id) = order_id else {
            tracing::warn!("PAYMENT.CAPTURE.COMPLETED {capture_id:?} without related order id");
            return Ok(());
        };
        if self.skip_if_manual_order(order_id).await? {
            return Ok(());
        }
        let result = proration::apply_completed_payment(self.state, order_id).await?;
        tracing::info!("Order capture {order_id}: {}", result.outcome.as_str());
        Ok(())
    }

    /// Node: paypal.controller.ts:1497 `handlePaymentFailed` (DENIED / REFUNDED).
    async fn payment_failed(&self) -> HResult {
        let agreement = truthy_str(self.resource.get("billing_agreement_id"));
        tracing::info!(
            "Payment failed for subscription: {}",
            agreement.unwrap_or("undefined")
        );
        let Some(agreement) = agreement else {
            return Ok(());
        };
        let Some(sub) = find_sub(self.state, By::Paypal, agreement).await? else {
            return Ok(());
        };
        if self.skip_if_manual(&sub) {
            return Ok(());
        }
        sqlx::query(
            r#"UPDATE subscriptions SET status = 'SUSPENDED'::"SubscriptionStatus",
                   "paypalStatus" = 'PAYMENT_FAILED', "updatedAt" = $2 WHERE id = $1"#,
        )
        .bind(sub.id)
        .bind(now_naive())
        .execute(&self.state.db)
        .await?;
        if let Some((_, email, name)) = sub.owner() {
            mail::send(self.state, mail::payment_failed(self.state, email, name));
            tracing::info!("Payment failed email sent to {email}");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn price_like_js() {
        assert_eq!(price(5.0), "5");
        assert_eq!(price(4.99), "4.99");
    }
}
