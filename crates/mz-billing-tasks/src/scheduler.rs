//! Invoice scheduler (tasks.service.ts:864–2020): `handleInvoiceScheduler`,
//! `handleInvoiceSchedulerForce`, `handleAnnualProrationForce`, `processSubscriptionInvoice`,
//! `processMonthlyInvoice`, `processAnnualInvoice`, `createAnnualSubscriptionInvoice`,
//! `processAnnualProration`, `buildProrationPlan`.
//!
//! Idempotensi: invoice SUBSCRIPTION dicegah ganda oleh cek "invoice < 24 jam" (kecuali force);
//! proration annual menghitung rencana, membuat order PayPal, lalu menulis invoice + subscription
//! dalam SATU transaksi yang mengunci baris subscription dan menghitung ulang rencananya
//! (berubah = tidak menulis apa pun, run berikutnya mengulang).

use chrono::NaiveDateTime;
use mz_billing::generate_invoice_number;
use mz_core::jsfmt::{num_string, to_fixed};
use mz_core::{ApiError, ApiResult};
use serde_json::{Value, json};
use sqlx::{FromRow, PgConnection, Postgres, QueryBuilder};

use crate::jsdate::{
    add_days, days_until, dmy, next_invoice_date, remaining_months_from, start_of_day, ymd,
};
use crate::mail::{InvoiceConfirmation, due_date, invoice_confirmation};
use crate::{Ctx, OWNER_LATERAL};

/// `PRORATION_GRACE_DAYS` (tasks.service.ts:42).
pub const PRORATION_GRACE_DAYS: i64 = 7;

/// Subscription + relasi yang dimuat `findMany({ include: { company: { include: { userCompany } }, pricingTier } })`.
#[derive(Debug, Clone, FromRow)]
pub struct DueSub {
    pub id: i32,
    #[sqlx(rename = "companyId")]
    pub company_id: i32,
    #[sqlx(rename = "paypalSubscriptionId")]
    pub paypal_subscription_id: Option<String>,
    #[sqlx(rename = "pricePerUser")]
    pub price_per_user: f64,
    #[sqlx(rename = "billingInterval")]
    pub billing_interval: String,
    #[sqlx(rename = "endDate")]
    pub end_date: NaiveDateTime,
    #[sqlx(rename = "lastBilledQuantity")]
    pub last_billed_quantity: i32,
    pub tier_price: Option<f64>,
    #[sqlx(rename = "dontChargeInvite")]
    pub dont_charge_invite: bool,
    pub owner_email: Option<String>,
    pub owner_name: Option<String>,
}

impl DueSub {
    /// `subscription.pricePerUser || subscription.pricingTier.pricePerUserUsd || 0`.
    fn price(&self) -> f64 {
        if self.price_per_user != 0.0 && !self.price_per_user.is_nan() {
            return self.price_per_user;
        }
        self.tier_price
            .filter(|p| *p != 0.0 && !p.is_nan())
            .unwrap_or(0.0)
    }

    /// `subscription.paypalSubscriptionId` di URL axios (`null` → "null", seperti template string Node).
    fn paypal_id(&self) -> &str {
        self.paypal_subscription_id.as_deref().unwrap_or("null")
    }
}

#[derive(Clone, Copy)]
enum Filter {
    /// `handleInvoiceScheduler`: `nextInvoiceDate <= now`.
    Due,
    /// `handleInvoiceSchedulerForce`.
    All,
    /// `handleAnnualProrationForce`: `billingInterval = 'YEAR'`.
    Annual,
}

async fn load(ctx: &Ctx, filter: Filter, company_id: Option<i32>) -> ApiResult<Vec<DueSub>> {
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(format!(
        r#"SELECT s.id, s."companyId", s."paypalSubscriptionId", s."pricePerUser", s."billingInterval",
                  s."endDate", s."lastBilledQuantity", pt."pricePerUserUsd" AS tier_price,
                  c."dontChargeInvite", o.owner_email, o.owner_name
             FROM subscriptions s
             JOIN company c ON c.id = s."companyId"
             LEFT JOIN "pricingTiers" pt ON pt.id = s."pricingTierId"
             {OWNER_LATERAL}
            WHERE s."billingSource" = 'PAYPAL'::"SubscriptionBillingSource"
              AND s.status = 'ACTIVE'::"SubscriptionStatus" AND s."autoRenew" = true
              AND c."isTrial" = false"#
    ));
    match filter {
        Filter::Due => {
            qb.push(r#" AND s."nextInvoiceDate" <= "#)
                .push_bind(ctx.now);
        }
        Filter::Annual => {
            qb.push(r#" AND s."billingInterval" = 'YEAR'"#);
        }
        Filter::All => {}
    }
    if let Some(c) = company_id {
        qb.push(r#" AND s."companyId" = "#).push_bind(c);
    }
    qb.push(" ORDER BY s.id");
    Ok(qb.build_query_as().fetch_all(&ctx.state.db).await?)
}

/// `handleInvoiceScheduler` (:865) — cron 02:00 WIB.
pub async fn invoice_scheduler(ctx: &Ctx) {
    tracing::info!("Running invoice scheduler...");
    let subs = match load(ctx, Filter::Due, None).await {
        Ok(s) => s,
        Err(err) => {
            tracing::error!(error = %err, "Invoice scheduler failed");
            return;
        }
    };
    tracing::info!("Found {} subscriptions due for billing", subs.len());
    for sub in &subs {
        if let Err(err) = process_subscription_invoice(ctx, sub, false).await {
            tracing::error!(
                "Failed to process invoice for subscription {}: {err}",
                sub.id
            );
        }
    }
}

/// `handleInvoiceSchedulerForce` (:1320) — tanpa cek `nextInvoiceDate` dan tanpa cek duplikat 24 jam.
pub async fn invoice_scheduler_force(ctx: &Ctx, company_id: Option<i32>) -> ApiResult<Value> {
    tracing::info!(
        "[FORCE] Running invoice scheduler (no due date check){}...",
        company_id
            .map(|c| format!(" for companyId={c}"))
            .unwrap_or_default()
    );
    let subs = load(ctx, Filter::All, company_id).await?;
    tracing::info!("[FORCE] Found {} active subscription(s)", subs.len());
    for sub in &subs {
        if let Err(err) = process_subscription_invoice(ctx, sub, true).await {
            tracing::error!(
                "[FORCE] Failed to process invoice for subscription {}: {err}",
                sub.id
            );
        }
    }
    Ok(json!({
        "success": true,
        "message": format!("Processed {} subscription(s)", subs.len()),
    }))
}

fn detail(sub: &DueSub, status: &str, message: &str) -> Value {
    json!({
        "subscriptionId": sub.id,
        "companyId": sub.company_id,
        "status": status,
        "message": message,
    })
}

/// `handleAnnualProrationForce` (:1164).
pub async fn annual_proration_force(ctx: &Ctx, company_id: Option<i32>) -> ApiResult<Value> {
    tracing::info!(
        "[FORCE-PRORATION] Running annual proration force{}...",
        company_id
            .map(|c| format!(" for companyId={c}"))
            .unwrap_or_else(|| " (all companies)".to_owned())
    );
    let subs = load(ctx, Filter::Annual, company_id).await?;
    tracing::info!(
        "[FORCE-PRORATION] Found {} annual subscription(s)",
        subs.len()
    );
    let mut details = Vec::new();
    let (mut processed, mut skipped) = (0, 0);
    for sub in &subs {
        match proration_force_one(ctx, sub).await {
            Ok((status, msg)) => {
                if status == "processed" {
                    processed += 1;
                } else if status == "skipped" {
                    skipped += 1;
                }
                details.push(detail(sub, status, &msg));
            }
            Err(err) => {
                let msg = format!("Error: {err}");
                tracing::error!("[FORCE-PRORATION] Subscription {}: {msg}", sub.id);
                details.push(detail(sub, "error", &msg));
            }
        }
    }
    tracing::info!("[FORCE-PRORATION] Done — processed: {processed}, skipped: {skipped}");
    Ok(json!({
        "success": true,
        "processed": processed,
        "skipped": skipped,
        "details": details,
    }))
}

async fn proration_force_one(ctx: &Ctx, sub: &DueSub) -> ApiResult<(&'static str, String)> {
    let active = active_users(ctx, sub.company_id).await?;
    let last_billed = sub.last_billed_quantity;
    // `pricePerUser || pricingTier?.pricePerUserUsd || 0`.
    let price = sub.price();
    if active <= i64::from(last_billed) {
        let msg = format!("No new users (active: {active}, lastBilled: {last_billed}) — skipped");
        tracing::info!("[FORCE-PRORATION] Subscription {}: {msg}", sub.id);
        return Ok(("skipped", msg));
    }
    if sub.dont_charge_invite {
        let msg = "dontChargeInvite=true — proration skipped".to_owned();
        tracing::info!("[FORCE-PRORATION] Subscription {}: {msg}", sub.id);
        return Ok(("skipped", msg));
    }
    let outcome = process_annual_proration(ctx, sub, last_billed, price, sub.end_date).await?;
    Ok(match outcome {
        ProrationOutcome::Created => (
            "processed",
            format!(
                "Proration processed — {} new user(s) billed",
                active - i64::from(last_billed)
            ),
        ),
        ProrationOutcome::PaypalFailed => {
            ("error", "PayPal order failed — nothing written".to_owned())
        }
        ProrationOutcome::StateChanged => (
            "skipped",
            "Subscription changed concurrently — nothing written, retry later".to_owned(),
        ),
        ProrationOutcome::NoNewUsers => (
            "skipped",
            "No new users after lastBilledQuantity — skipped".to_owned(),
        ),
    })
}

async fn active_users(ctx: &Ctx, company_id: i32) -> ApiResult<i64> {
    Ok(sqlx::query_scalar(
        r#"SELECT COUNT(*) FROM "userCompany" WHERE "companyId" = $1 AND status = 1"#,
    )
    .bind(company_id)
    .fetch_one(&ctx.state.db)
    .await?)
}

/// `processSubscriptionInvoice` (:1370).
pub async fn process_subscription_invoice(ctx: &Ctx, sub: &DueSub, force: bool) -> ApiResult<()> {
    if sub.dont_charge_invite {
        tracing::info!(
            "Skipping invoice for subscription {} — dontChargeInvite=true",
            sub.id
        );
        return Ok(());
    }
    if !force {
        let one_day_ago = add_days(ctx.now, -1);
        let recent: Option<String> = sqlx::query_scalar(
            r#"SELECT "invoiceNumber" FROM invoices
                WHERE "subscriptionId" = $1 AND "invoiceType" = 'SUBSCRIPTION'::"InvoiceType"
                  AND status IN ('UNPAID'::"InvoiceStatus", 'PAID'::"InvoiceStatus")
                  AND "createdAt" >= $2
                LIMIT 1"#,
        )
        .bind(sub.id)
        .bind(one_day_ago)
        .fetch_optional(&ctx.state.db)
        .await?;
        if let Some(n) = recent {
            tracing::info!(
                "Skipping subscription {} — recent invoice exists: {n}",
                sub.id
            );
            return Ok(());
        }
    }
    let active = active_users(ctx, sub.company_id).await?;
    if sub.tier_price.is_none() && (sub.price_per_user == 0.0 || sub.price_per_user.is_nan()) {
        // `subscription.pricingTier.pricePerUserUsd` pada relasi null → TypeError di Node.
        return Err(ApiError::internal(anyhow::anyhow!(
            "Cannot read properties of null (reading 'pricePerUserUsd')"
        )));
    }
    let price = sub.price();
    let interval = if sub.billing_interval.is_empty() {
        "MONTH"
    } else {
        sub.billing_interval.as_str()
    };
    if interval == "MONTH" {
        process_monthly_invoice(ctx, sub, active, price).await
    } else {
        process_annual_invoice(ctx, sub, active, price).await
    }
}

/// Satu baris `invoiceItems` (`items: { create: [...] }`).
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub description: String,
    pub quantity: f64,
    pub unit_price: f64,
    pub total_price: f64,
}

/// Invoice baru (kolom yang ditulis Node).
pub(crate) struct NewInvoice<'a> {
    pub(crate) subscription_id: i32,
    pub(crate) invoice_number: &'a str,
    pub(crate) payment_url: &'a str,
    pub(crate) paypal_payment_id: Option<&'a str>,
    pub(crate) sub_price: f64,
    pub(crate) total_price: f64,
    pub(crate) invoice_type: &'static str,
    pub(crate) status: &'static str,
    pub(crate) expired_at: NaiveDateTime,
    pub(crate) created_at: NaiveDateTime,
}

/// `invoices.create` + `invoiceItems.create`. `createdAt` diisi jam run (Node: `NOW()` DB) supaya
/// cek 24 jam / masa tenggang proration memakai jam yang sama.
pub(crate) async fn insert_invoice(
    conn: &mut PgConnection,
    inv: &NewInvoice<'_>,
    items: &[Item],
) -> sqlx::Result<i32> {
    let id: i32 = sqlx::query_scalar(
        r#"INSERT INTO invoices ("subscriptionId", "invoiceNumber", "paymentUrl", "paypalPaymentId",
                "subPrice", "taxPrice", "totalPrice", "invoiceType", status, "paymentProvider",
                "expiredAt", "createdAt")
           VALUES ($1, $2, $3, $4, $5, 0, $6, $7::"InvoiceType", $8::"InvoiceStatus", 'paypal', $9, $10)
           RETURNING id"#,
    )
    .bind(inv.subscription_id)
    .bind(inv.invoice_number)
    .bind(inv.payment_url)
    .bind(inv.paypal_payment_id)
    .bind(inv.sub_price)
    .bind(inv.total_price)
    .bind(inv.invoice_type)
    .bind(inv.status)
    .bind(inv.expired_at)
    .bind(inv.created_at)
    .fetch_one(&mut *conn)
    .await?;
    for it in items {
        sqlx::query(
            r#"INSERT INTO "invoiceItems" ("invoiceId", description, quantity, "unitPrice", "totalPrice")
               VALUES ($1, $2, $3, $4, $5)"#,
        )
        .bind(id)
        .bind(&it.description)
        .bind(it.quantity)
        .bind(it.unit_price)
        .bind(it.total_price)
        .execute(&mut *conn)
        .await?;
    }
    Ok(id)
}

async fn set_quantity(ctx: &Ctx, sub_id: i32, qty: i64) -> ApiResult<()> {
    let q = i32::try_from(qty).unwrap_or(i32::MAX);
    sqlx::query(
        r#"UPDATE subscriptions SET quantity = $2, "lastBilledQuantity" = $2 WHERE id = $1"#,
    )
    .bind(sub_id)
    .bind(q)
    .execute(&ctx.state.db)
    .await?;
    Ok(())
}

async fn update_paypal_quantity(ctx: &Ctx, sub: &DueSub, qty: i64, annual: bool) {
    match ctx
        .paypal
        .update_subscription_quantity(sub.paypal_id(), qty)
        .await
    {
        Ok(_) if annual => tracing::info!(
            "Updated PayPal quantity to {qty} for annual subscription {}",
            sub.id
        ),
        Ok(_) => tracing::info!(
            "Updated PayPal quantity to {qty} (billable) for subscription {}",
            sub.id
        ),
        Err(err) => tracing::error!(
            "Failed to update PayPal quantity for subscription {}: {err}",
            sub.id
        ),
    }
}

/// `processMonthlyInvoice` (:1419).
async fn process_monthly_invoice(
    ctx: &Ctx,
    sub: &DueSub,
    active: i64,
    price: f64,
) -> ApiResult<()> {
    let end_date = sub.end_date;
    let cutoff = add_days(end_date, -7);
    let billable: i64 = sqlx::query_scalar(
        r#"SELECT COUNT(*) FROM "userCompany"
            WHERE "companyId" = $1 AND status = 1 AND "createdAt" <= $2"#,
    )
    .bind(sub.company_id)
    .bind(cutoff)
    .fetch_one(&ctx.state.db)
    .await?;
    let deferred = active - billable;
    if deferred > 0 {
        tracing::info!(
            "Monthly billing for subscription {}: {billable} billable, {deferred} deferred to next month (joined within 7 days of endDate)",
            sub.id
        );
    }
    if billable == 0 {
        tracing::info!(
            "No billable users for subscription {} — skipping invoice, all {active} user(s) deferred",
            sub.id
        );
        return set_quantity(ctx, sub.id, active).await;
    }

    #[allow(clippy::cast_precision_loss)]
    let sub_price = billable as f64 * price;
    let total_price = sub_price;
    let invoice_number = generate_invoice_number(ctx.now_utc());
    let expired_at = add_days(ctx.now, 15);

    // PayPal quantity = billable supaya auto-charge cocok dengan invoice ini.
    update_paypal_quantity(ctx, sub, billable, false).await;

    let description = format!(
        "Monthly Subscription - {billable} user(s) @ ${}/user{}",
        num_string(price),
        if deferred > 0 {
            format!(" ({deferred} user(s) deferred to next month)")
        } else {
            String::new()
        }
    );
    let mut tx = ctx.state.db.begin().await?;
    #[allow(clippy::cast_precision_loss)]
    insert_invoice(
        &mut tx,
        &NewInvoice {
            subscription_id: sub.id,
            invoice_number: &invoice_number,
            payment_url: "",
            paypal_payment_id: None,
            sub_price,
            total_price,
            invoice_type: "SUBSCRIPTION",
            status: "UNPAID",
            expired_at,
            created_at: ctx.now,
        },
        &[Item {
            description,
            quantity: billable as f64,
            unit_price: price,
            total_price: sub_price,
        }],
    )
    .await?;
    tx.commit().await?;

    // quantity = semua user aktif supaya bulan depan user yang ditunda ikut ditagih.
    set_quantity(ctx, sub.id, active).await?;

    if let Some(email) = &sub.owner_email {
        ctx.mail.fire(
            &ctx.state,
            due_date(
                email,
                sub.owner_name.as_deref().unwrap_or_default(),
                &dmy(end_date),
                &invoice_number,
                total_price,
            ),
        );
    }
    tracing::info!(
        "Monthly invoice {invoice_number} created for subscription {} — {billable} billable user(s) of {active} total, ${}, awaiting PayPal auto-charge",
        sub.id,
        to_fixed(total_price, 2)
    );
    Ok(())
}

/// `processAnnualInvoice` (:1555).
async fn process_annual_invoice(ctx: &Ctx, sub: &DueSub, active: i64, price: f64) -> ApiResult<()> {
    let now = ctx.now;
    let end_date = sub.end_date;
    let days_until_end = days_until(now, end_date);
    let last_billed = sub.last_billed_quantity;
    #[allow(clippy::cast_precision_loss)]
    let full = active as f64 * price * 12.0;

    if last_billed == 0 {
        let description = format!(
            "Annual Subscription - {active} user(s) @ ${}/user/month (billed yearly)",
            num_string(price)
        );
        return create_annual_subscription_invoice(
            ctx,
            sub,
            active,
            price,
            full,
            description,
            false,
        )
        .await;
    }
    if days_until_end <= 3 {
        let description = format!(
            "Annual Renewal - {active} user(s) @ ${}/user/month (billed yearly)",
            num_string(price)
        );
        return create_annual_subscription_invoice(
            ctx,
            sub,
            active,
            price,
            full,
            description,
            true,
        )
        .await;
    }
    if active > i64::from(last_billed) {
        if sub.dont_charge_invite {
            tracing::info!(
                "Skipping annual proration for subscription {} — dontChargeInvite=true ({} new user(s) not billed)",
                sub.id,
                active - i64::from(last_billed)
            );
            let next = next_invoice_date(now, end_date);
            let mut qb: QueryBuilder<Postgres> =
                QueryBuilder::new(r#"UPDATE subscriptions SET "lastBilledQuantity" = "#);
            qb.push_bind(i32::try_from(active).unwrap_or(i32::MAX));
            if next > now {
                qb.push(r#", "nextInvoiceDate" = "#).push_bind(next);
            }
            qb.push(" WHERE id = ").push_bind(sub.id);
            qb.build().execute(&ctx.state.db).await?;
        } else {
            process_annual_proration(ctx, sub, last_billed, price, end_date).await?;
        }
        return Ok(());
    }
    tracing::info!(
        "Skipping annual subscription {} — no billing needed (active: {active}, lastBilled: {last_billed})",
        sub.id
    );
    let next = next_invoice_date(now, end_date);
    if next > now {
        sqlx::query(r#"UPDATE subscriptions SET "nextInvoiceDate" = $2 WHERE id = $1"#)
            .bind(sub.id)
            .bind(next)
            .execute(&ctx.state.db)
            .await?;
    }
    Ok(())
}

/// `createAnnualSubscriptionInvoice` (:1658).
async fn create_annual_subscription_invoice(
    ctx: &Ctx,
    sub: &DueSub,
    active: i64,
    price: f64,
    sub_price: f64,
    description: String,
    is_renewal: bool,
) -> ApiResult<()> {
    let total_price = sub_price;
    let invoice_number = generate_invoice_number(ctx.now_utc());
    let expired_at = add_days(ctx.now, 15);
    let mut tx = ctx.state.db.begin().await?;
    #[allow(clippy::cast_precision_loss)]
    insert_invoice(
        &mut tx,
        &NewInvoice {
            subscription_id: sub.id,
            invoice_number: &invoice_number,
            payment_url: "",
            paypal_payment_id: None,
            sub_price,
            total_price,
            invoice_type: "SUBSCRIPTION",
            status: "UNPAID",
            expired_at,
            created_at: ctx.now,
        },
        &[Item {
            description,
            quantity: active as f64,
            unit_price: price * 12.0,
            total_price: sub_price,
        }],
    )
    .await?;
    tx.commit().await?;

    update_paypal_quantity(ctx, sub, active, true).await;
    set_quantity(ctx, sub.id, active).await?;

    if is_renewal && let Some(email) = &sub.owner_email {
        ctx.mail.fire(
            &ctx.state,
            due_date(
                email,
                sub.owner_name.as_deref().unwrap_or_default(),
                &dmy(sub.end_date),
                &invoice_number,
                total_price,
            ),
        );
    }
    tracing::info!(
        "Annual {} invoice {invoice_number} created for subscription {} — {active} users, ${}, awaiting PayPal auto-charge",
        if is_renewal { "renewal" } else { "first" },
        sub.id,
        to_fixed(total_price, 2)
    );
    Ok(())
}

/// Hasil satu percobaan proration annual; hanya `Created` yang menulis ke DB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProrationOutcome {
    Created,
    NoNewUsers,
    PaypalFailed,
    StateChanged,
}

/// `ProrationPlan` (tasks.service.ts:50).
#[derive(Debug, Clone, PartialEq)]
pub struct ProrationPlan {
    pub new_user_count: usize,
    pub items: Vec<Item>,
    pub total_price: f64,
}

/// Bagian murni `buildProrationPlan` (:1968): `users` = `(createdAt)` user aktif urut
/// `createdAt, id`; yang di luar `last_billed` digroup per tanggal join.
pub fn proration_plan(
    users: &[NaiveDateTime],
    last_billed: i32,
    price: f64,
    end_date: NaiveDateTime,
) -> Option<ProrationPlan> {
    // `Array.prototype.slice(n)`: n negatif dihitung dari belakang.
    let start = if last_billed >= 0 {
        usize::try_from(last_billed).unwrap_or(usize::MAX)
    } else {
        users
            .len()
            .saturating_sub(usize::try_from(-i64::from(last_billed)).unwrap_or(usize::MAX))
    };
    let new_users = users.get(start..).unwrap_or(&[]);
    if new_users.is_empty() {
        return None;
    }
    // Map JS mempertahankan urutan sisip.
    let mut groups: Vec<(String, usize, NaiveDateTime)> = Vec::new();
    for created in new_users {
        let key = ymd(*created);
        match groups.iter_mut().find(|g| g.0 == key) {
            Some(g) => g.1 += 1,
            None => groups.push((key, 1, start_of_day(*created))),
        }
    }
    let end_str = ymd(end_date);
    let mut items = Vec::with_capacity(groups.len());
    let mut total = 0.0;
    for (key, count, join) in groups {
        let months = remaining_months_from(join, end_date);
        #[allow(clippy::cast_precision_loss)]
        let (count_f, months_f) = (count as f64, months as f64);
        let item_sub = count_f * price * months_f;
        items.push(Item {
            description: format!(
                "Add-on: {count} user(s) × ${}/user × {months} month(s) ({key} – {end_str})",
                num_string(price)
            ),
            quantity: count_f,
            unit_price: price * months_f,
            total_price: item_sub,
        });
        total += item_sub;
    }
    Some(ProrationPlan {
        new_user_count: new_users.len(),
        items,
        total_price: total,
    })
}

async fn build_proration_plan(
    conn: &mut PgConnection,
    company_id: i32,
    last_billed: i32,
    price: f64,
    end_date: NaiveDateTime,
) -> sqlx::Result<Option<ProrationPlan>> {
    let users: Vec<NaiveDateTime> = sqlx::query_scalar(
        r#"SELECT "createdAt" FROM "userCompany" WHERE "companyId" = $1 AND status = 1
            ORDER BY "createdAt" ASC, id ASC"#,
    )
    .bind(company_id)
    .fetch_all(&mut *conn)
    .await?;
    Ok(proration_plan(&users, last_billed, price, end_date))
}

/// `encodeURIComponent`.
fn enc(s: &str) -> String {
    mz_files::encode::encode_uri_component(s)
}

/// `processAnnualProration` (:1781): rencana → order PayPal → satu transaksi (lock subscription,
/// hitung ulang, tulis invoice PRORATION + item + subscription) → email link bayar.
pub async fn process_annual_proration(
    ctx: &Ctx,
    sub: &DueSub,
    last_billed: i32,
    price: f64,
    end_date: NaiveDateTime,
) -> ApiResult<ProrationOutcome> {
    let now = ctx.now;
    let plan = {
        let mut conn = ctx.state.db.acquire().await?;
        build_proration_plan(&mut conn, sub.company_id, last_billed, price, end_date).await?
    };
    let Some(plan) = plan else {
        tracing::warn!(
            "Proration triggered for subscription {} but no new users found after slice({last_billed})",
            sub.id
        );
        return Ok(ProrationOutcome::NoNewUsers);
    };

    let invoice_number = generate_invoice_number(ctx.now_utc());
    let api_base = ctx
        .state
        .config
        .var("HOST_BE")
        .unwrap_or_else(|| "http://localhost:3000".to_owned());
    let dashboard = ctx
        .state
        .config
        .var("FRONTEND_URL")
        .unwrap_or_else(|| "http://localhost:3001".to_owned());
    let return_url = format!("{api_base}/paypal/proration/return");
    let cancel_url = format!(
        "{dashboard}/billing?proration=cancelled&invoice={}",
        enc(&invoice_number)
    );
    let order = match ctx
        .paypal
        .create_order(
            plan.total_price,
            &format!("Timemoz Proration — {} add-on user(s)", plan.new_user_count),
            &return_url,
            &cancel_url,
        )
        .await
    {
        Ok(o) => o,
        Err(err) => {
            tracing::error!(
                "Failed to create PayPal order for annual proration of subscription {}: {err} — nothing written, retried next run",
                sub.id
            );
            return Ok(ProrationOutcome::PaypalFailed);
        }
    };
    let order_id = order
        .get("id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let approval_url = order
        .get("links")
        .and_then(Value::as_array)
        .and_then(|l| {
            l.iter()
                .find(|x| x.get("rel").and_then(Value::as_str) == Some("approve"))
        })
        .and_then(|x| x.get("href"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let (Some(order_id), Some(approval_url)) = (order_id, approval_url) else {
        tracing::error!(
            "PayPal order for annual proration of subscription {} has no id/approve link — nothing written, retried next run",
            sub.id
        );
        return Ok(ProrationOutcome::PaypalFailed);
    };

    let next = next_invoice_date(now, end_date);
    let expired_at = add_days(now, 15);
    let new_last_billed =
        i32::try_from(i64::from(last_billed) + i64::try_from(plan.new_user_count).unwrap_or(0))
            .unwrap_or(i32::MAX);

    let mut tx = ctx.state.db.begin().await?;
    sqlx::query(r#"SELECT id FROM subscriptions WHERE id = $1 FOR UPDATE"#)
        .bind(sub.id)
        .execute(&mut *tx)
        .await?;
    let current: Option<(i32, String)> = sqlx::query_as(
        r#"SELECT "lastBilledQuantity", "billingSource"::text FROM subscriptions WHERE id = $1"#,
    )
    .bind(sub.id)
    .fetch_optional(&mut *tx)
    .await?;
    let still_valid = match &current {
        // specs/007 R-10: baris yang dialihkan ke MANUAL tidak boleh mendapat invoice proration.
        Some((lb, src)) => src != "MANUAL" && *lb == last_billed,
        None => false,
    };
    let fresh = if still_valid {
        build_proration_plan(&mut tx, sub.company_id, last_billed, price, end_date).await?
    } else {
        None
    };
    if fresh.as_ref() != Some(&plan) {
        tx.rollback().await?;
        tracing::warn!(
            "Annual proration for subscription {} aborted — lastBilledQuantity, active users or billing source changed while the PayPal order {order_id} was created; retried next run",
            sub.id
        );
        return Ok(ProrationOutcome::StateChanged);
    }
    insert_invoice(
        &mut tx,
        &NewInvoice {
            subscription_id: sub.id,
            invoice_number: &invoice_number,
            payment_url: approval_url,
            paypal_payment_id: Some(order_id),
            sub_price: plan.total_price,
            total_price: plan.total_price,
            invoice_type: "PRORATION",
            status: "UNPAID",
            expired_at,
            created_at: now,
        },
        &plan.items,
    )
    .await?;
    sqlx::query(
        r#"UPDATE subscriptions SET quantity = $2, "lastBilledQuantity" = $2, "nextInvoiceDate" = $3
            WHERE id = $1"#,
    )
    .bind(sub.id)
    .bind(new_last_billed)
    .bind(next)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    if let Some(email) = &sub.owner_email {
        let end_str = ymd(end_date);
        // Cron expiry (04:00) menangkap invoice begitu createdAt + GRACE terlewati, jadi hari
        // terakhir yang aman untuk membayar adalah hari ke-(GRACE - 1).
        let due = add_days(now, PRORATION_GRACE_DAYS - 1);
        ctx.mail.fire(
            &ctx.state,
            invoice_confirmation(&InvoiceConfirmation {
                email,
                name: sub.owner_name.as_deref().unwrap_or_default(),
                tier_name: &format!("Add-on: {} user(s) (prorated)", plan.new_user_count),
                invoice_number: &invoice_number,
                payment_url: approval_url,
                period: &format!("{} – {end_str}", ymd(now)),
                total_price: plan.total_price,
                expired_at: &end_str,
                payment_due_date: &ymd(due),
            }),
        );
    }
    tracing::info!(
        "Annual proration invoice {invoice_number} created — {} new user(s) in {} group(s), total: ${}, paymentUrl: {approval_url}, next check: {}",
        plan.new_user_count,
        plan.items.len(),
        to_fixed(plan.total_price, 2),
        crate::jsdate::js_to_string(next)
    );
    Ok(ProrationOutcome::Created)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dt(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").unwrap()
    }

    #[test]
    fn plan_groups_by_join_date_after_last_billed() {
        let users = [
            dt("2026-01-01 08:00:00"),
            dt("2026-01-02 08:00:00"),
            dt("2026-06-10 09:00:00"),
            dt("2026-06-10 23:00:00"),
            dt("2026-07-15 01:00:00"),
        ];
        let end = dt("2027-01-01 00:00:00");
        let p = proration_plan(&users, 2, 5.0, end).unwrap();
        assert_eq!(p.new_user_count, 3);
        assert_eq!(p.items.len(), 2);
        // 2026-06-10 → 2027-01-01 = 205 hari → ceil(205/30) = 7 bulan.
        assert_eq!(
            p.items[0].description,
            "Add-on: 2 user(s) × $5/user × 7 month(s) (2026-06-10 – 2027-01-01)"
        );
        assert_eq!(p.items[0].unit_price, 35.0);
        assert_eq!(p.items[0].total_price, 70.0);
        // 2026-07-15 → 170 hari → 6 bulan.
        assert_eq!(p.items[1].total_price, 30.0);
        assert_eq!(p.total_price, 100.0);
        assert!(proration_plan(&users, 5, 5.0, end).is_none());
        // slice(-1) = user terakhir saja.
        assert_eq!(
            proration_plan(&users, -1, 5.0, end).unwrap().new_user_count,
            1
        );
    }

    #[test]
    fn price_fallback_like_js_or() {
        let mut s = DueSub {
            id: 1,
            company_id: 1,
            paypal_subscription_id: None,
            price_per_user: 0.0,
            billing_interval: String::new(),
            end_date: dt("2026-01-01 00:00:00"),
            last_billed_quantity: 0,
            tier_price: Some(7.5),
            dont_charge_invite: false,
            owner_email: None,
            owner_name: None,
        };
        assert_eq!(s.price(), 7.5);
        s.price_per_user = 4.0;
        assert_eq!(s.price(), 4.0);
        s.price_per_user = 0.0;
        s.tier_price = None;
        assert_eq!(s.price(), 0.0);
        assert_eq!(s.paypal_id(), "null");
    }
}
