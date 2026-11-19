//! Email pengingat harian (tasks.service.ts): `handleDownloadReminder` (:574),
//! `handleTrialEndingReminder` (:662), `handleTrialEndedReminder` (:711),
//! `handleManualRenewalReminder` (:772).
//!
//! Idempotensi per run: download/trial memakai jendela waktu yang tak tumpang-tindih antar hari
//! (sama dengan Node, tanpa kolom penanda); manual renewal mengklaim
//! `manualReminderForEndDate` lewat `UPDATE` bersyarat sebelum mengirim.

use chrono::NaiveDateTime;
use mz_core::ApiResult;
use sqlx::FromRow;

use crate::jsdate::{add_days, calendar_day_range, ymd};
use crate::{Ctx, OWNER_LATERAL, mail};

/// `handleDownloadReminder`: user yang register 24–48 jam lalu dan belum pernah tracking.
pub async fn download_reminder(ctx: &Ctx) {
    tracing::info!("Running download reminder check...");
    let one_day_ago = add_days(ctx.now, -1);
    let two_days_ago = add_days(ctx.now, -2);
    let users: Result<Vec<(String, String)>, _> = sqlx::query_as(
        r#"SELECT u.username, u.name FROM users u
            WHERE u."createdAt" >= $1 AND u."createdAt" <= $2
              AND NOT EXISTS (SELECT 1 FROM tracking t WHERE t."userId" = u.id)
            ORDER BY u.id"#,
    )
    .bind(two_days_ago)
    .bind(one_day_ago)
    .fetch_all(&ctx.state.db)
    .await;
    let users = match users {
        Ok(u) => u,
        Err(err) => {
            tracing::error!(error = %err, "Download reminder check failed");
            return;
        }
    };
    tracing::info!("Found {} user(s) to send download reminder", users.len());
    for (email, name) in users {
        let r = ctx
            .mail
            .send(
                &ctx.state,
                mail::download_reminder(&ctx.state, &email, &name),
            )
            .await;
        if r.success {
            tracing::info!("Download reminder sent to {email}");
        } else {
            tracing::error!("Failed to send download reminder to {email}: {}", r.message);
        }
    }
}

#[derive(FromRow)]
struct TrialRow {
    id: i32,
    product_type: Option<String>,
    owner_email: Option<String>,
    owner_name: Option<String>,
}

async fn trial_rows(
    ctx: &Ctx,
    where_: &str,
    range: (NaiveDateTime, NaiveDateTime),
) -> ApiResult<Vec<TrialRow>> {
    let sql = format!(
        r#"SELECT s.id, pt."productType"::text AS product_type, o.owner_email, o.owner_name
             FROM subscriptions s
             JOIN company c ON c.id = s."companyId" AND c.status = 1
             LEFT JOIN "pricingTiers" pt ON pt.id = s."pricingTierId"
             {OWNER_LATERAL}
            WHERE s."endDate" >= $1 AND s."endDate" <= $2 AND {where_}
            ORDER BY s.id"#
    );
    Ok(sqlx::query_as(&sql)
        .bind(range.0)
        .bind(range.1)
        .fetch_all(&ctx.state.db)
        .await?)
}

/// `handleTrialEndingReminder`: subscription TRIAL yang `endDate`-nya jatuh di hari kalender ke-4.
pub async fn trial_ending_reminder(ctx: &Ctx) {
    tracing::info!("Running trial ending reminder check...");
    let rows = match trial_rows(
        ctx,
        r#"s.status = 'TRIAL'::"SubscriptionStatus""#,
        calendar_day_range(ctx.now, 4),
    )
    .await
    {
        Ok(r) => r,
        Err(err) => {
            tracing::error!(error = %err, "Trial ending reminder check failed");
            return;
        }
    };
    tracing::info!(
        "Found {} trial subscription(s) for ending reminder",
        rows.len()
    );
    for sub in rows {
        let Some(email) = sub.owner_email.filter(|e| !e.is_empty()) else {
            continue;
        };
        let name = sub.owner_name.unwrap_or_default();
        let product = sub.product_type.as_deref().unwrap_or("TIME");
        let r = ctx
            .mail
            .send(
                &ctx.state,
                mail::trial_ending_reminder(&email, &name, product),
            )
            .await;
        if r.success {
            tracing::info!("Trial ending reminder sent to {email}");
        } else {
            tracing::error!(
                "Failed to send trial ending reminder to {email} (subscription {}): {}",
                sub.id,
                r.message
            );
        }
    }
}

/// `handleTrialEndedReminder`: paket TIME berbayar PayPal (TRIAL/INACTIVE) yang `endDate`-nya hari
/// ini dan belum pernah punya invoice PAID.
pub async fn trial_ended_reminder(ctx: &Ctx) {
    tracing::info!("Running trial ended reminder check...");
    let rows = match trial_rows(
        ctx,
        r#"s."billingSource" = 'PAYPAL'::"SubscriptionBillingSource"
           AND s.status IN ('TRIAL'::"SubscriptionStatus", 'INACTIVE'::"SubscriptionStatus")
           AND s."pricePerUser" > 0
           AND pt."productType" = 'TIME'::"PricingProductType"
           AND NOT EXISTS (SELECT 1 FROM invoices i
                            WHERE i."subscriptionId" = s.id AND i.status = 'PAID'::"InvoiceStatus")"#,
        calendar_day_range(ctx.now, 0),
    )
    .await
    {
        Ok(r) => r,
        Err(err) => {
            tracing::error!(error = %err, "Trial ended reminder check failed");
            return;
        }
    };
    tracing::info!(
        "Found {} trial subscription(s) for ended reminder",
        rows.len()
    );
    for sub in rows {
        let Some(email) = sub.owner_email.filter(|e| !e.is_empty()) else {
            continue;
        };
        let name = sub.owner_name.unwrap_or_default();
        let r = ctx
            .mail
            .send(&ctx.state, mail::trial_ended_reminder(&email, &name))
            .await;
        if r.success {
            tracing::info!("Trial ended reminder sent to {email}");
        } else {
            tracing::error!(
                "Failed to send trial ended reminder to {email} (subscription {}): {}",
                sub.id,
                r.message
            );
        }
    }
}

#[derive(FromRow)]
struct ManualRow {
    id: i32,
    #[sqlx(rename = "endDate")]
    end_date: NaiveDateTime,
    plan_name: Option<String>,
    company_uuid: Option<String>,
    company_name: Option<String>,
}

/// `handleManualRenewalReminder` (specs/007 R-20): email ke Finance
/// (`MANUAL_BILLING_NOTIFY_EMAIL`) untuk langganan MANUAL ACTIVE yang berakhir di hari ke-7.
/// Tepat sekali per (subscription, endDate) lewat klaim `manualReminderForEndDate`.
pub async fn manual_renewal_reminder(ctx: &Ctx) {
    let Some(recipient) = ctx
        .state
        .config
        .var("MANUAL_BILLING_NOTIFY_EMAIL")
        .map(|v| mz_core::jsfmt::trim(&v).to_owned())
        .filter(|v| !v.is_empty())
    else {
        return;
    };
    tracing::info!("Running manual renewal reminder check...");
    let range = calendar_day_range(ctx.now, 7);
    let rows: Result<Vec<ManualRow>, _> = sqlx::query_as(
        r#"SELECT s.id, s."endDate", pt.name AS plan_name, c.uuid::text AS company_uuid,
                  c.name AS company_name
             FROM subscriptions s
             JOIN company c ON c.id = s."companyId" AND c.status = 1
             LEFT JOIN "pricingTiers" pt ON pt.id = s."pricingTierId"
            WHERE s."billingSource" = 'MANUAL'::"SubscriptionBillingSource"
              AND s.status = 'ACTIVE'::"SubscriptionStatus"
              AND s."endDate" >= $1 AND s."endDate" <= $2
            ORDER BY s.id"#,
    )
    .bind(range.0)
    .bind(range.1)
    .fetch_all(&ctx.state.db)
    .await;
    let rows = match rows {
        Ok(r) => r,
        Err(err) => {
            tracing::error!(error = %err, "Manual renewal reminder check failed");
            return;
        }
    };
    tracing::info!(
        "Found {} manual subscription(s) ending in 7 days",
        rows.len()
    );
    for sub in rows {
        if let Err(err) = manual_one(ctx, &recipient, range, &sub).await {
            tracing::error!(
                "Manual renewal reminder failed for subscription {}: {err}",
                sub.id
            );
        }
    }
}

async fn manual_one(
    ctx: &Ctx,
    recipient: &str,
    range: (NaiveDateTime, NaiveDateTime),
    sub: &ManualRow,
) -> ApiResult<()> {
    let n = sqlx::query(
        r#"UPDATE subscriptions SET "manualReminderForEndDate" = $2
            WHERE id = $1 AND "billingSource" = 'MANUAL'::"SubscriptionBillingSource"
              AND status = 'ACTIVE'::"SubscriptionStatus"
              AND "endDate" >= $3 AND "endDate" <= $4
              AND ("manualReminderForEndDate" IS NULL OR "manualReminderForEndDate" <> $2)"#,
    )
    .bind(sub.id)
    .bind(sub.end_date)
    .bind(range.0)
    .bind(range.1)
    .execute(&ctx.state.db)
    .await?
    .rows_affected();
    if n != 1 {
        return Ok(());
    }
    let r = ctx
        .mail
        .send(
            &ctx.state,
            mail::manual_renewal_reminder(
                &ctx.state,
                recipient,
                sub.company_name.as_deref().unwrap_or("-"),
                sub.company_uuid.as_deref().unwrap_or("-"),
                sub.plan_name.as_deref().unwrap_or("-"),
                &ymd(sub.end_date),
            ),
        )
        .await;
    if r.success {
        tracing::info!("Manual renewal reminder sent for subscription {}", sub.id);
    } else {
        tracing::error!(
            "Failed to send manual renewal reminder for subscription {}: {}",
            sub.id,
            r.message
        );
    }
    Ok(())
}
