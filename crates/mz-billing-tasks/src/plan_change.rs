//! `handlePendingPlanChangeForce` (tasks.service.ts:918) → `runPendingPlanChangeForce` (:928).
//! Hanya dipanggil route operator `POST /admin/plan-change/force` (tidak ada `@Cron`).
//!
//! Meniru webhook PayPal `PAYMENT.SALE.COMPLETED` untuk perubahan paket yang tertunda: tukar tier,
//! tanggal baru, salin batas tier ke company (ber-audit), pasang ulang override, lalu tandai invoice
//! UNPAID terakhir PAID (atau buat invoice PAID pengganti). Tidak transaksional, sama dengan Node.

use chrono::NaiveDateTime;
use mz_admin_company::plan_limits::reapply_plan_override_after_tier_sync;
use mz_audit::{Lookup, Model};
use mz_billing::generate_invoice_number;
use mz_core::jsfmt::num_string;
use mz_core::{ApiError, ApiResult};
use serde_json::{Value, json};
use sqlx::FromRow;

use crate::Ctx;
use crate::jsdate::{add_days, add_one_year, js_to_string};
use crate::scheduler::{Item, NewInvoice, insert_invoice};

/// `actorName` Node (dipakai hanya bila tidak ada konteks request).
pub const ACTOR: &str = "pending-plan-change";

#[derive(FromRow)]
struct PendingSub {
    id: i32,
    #[sqlx(rename = "companyId")]
    company_id: i32,
    #[sqlx(rename = "pendingPricingTierId")]
    pending_pricing_tier_id: Option<i32>,
    #[sqlx(rename = "pendingPaypalSubscriptionId")]
    pending_paypal_subscription_id: Option<String>,
    #[sqlx(rename = "pendingPaypalPlanId")]
    pending_paypal_plan_id: Option<String>,
    quantity: i32,
}

#[derive(FromRow)]
struct Tier {
    id: i32,
    name: String,
    #[sqlx(rename = "pricePerUserUsd")]
    price_per_user_usd: f64,
    #[sqlx(rename = "billingInterval")]
    billing_interval: String,
    #[sqlx(rename = "maxUsers")]
    max_users: i32,
    #[sqlx(rename = "maxProject")]
    max_project: i32,
    #[sqlx(rename = "useIdleDetection")]
    use_idle_detection: Option<bool>,
    #[sqlx(rename = "useCheatDetection")]
    use_cheat_detection: Option<bool>,
    #[sqlx(rename = "useReports")]
    use_reports: Option<bool>,
    #[sqlx(rename = "useTimesheetApproval")]
    use_timesheet_approval: Option<bool>,
    #[sqlx(rename = "useActivityLevel")]
    use_activity_level: Option<bool>,
    #[sqlx(rename = "useIntegrations")]
    use_integrations: Option<bool>,
}

fn detail(sub: &PendingSub, status: &str, message: &str) -> Value {
    json!({
        "subscriptionId": sub.id,
        "companyId": sub.company_id,
        "status": status,
        "message": message,
    })
}

/// `runPendingPlanChangeForce(companyId?)`.
pub async fn run(ctx: &Ctx, company_id: Option<i32>) -> ApiResult<Value> {
    tracing::info!(
        "[FORCE-PLAN-CHANGE] Running{}...",
        company_id
            .map(|c| format!(" for companyId={c}"))
            .unwrap_or_else(|| " (all companies)".to_owned())
    );
    let subs: Vec<PendingSub> = sqlx::query_as(
        r#"SELECT id, "companyId", "pendingPricingTierId", "pendingPaypalSubscriptionId",
                  "pendingPaypalPlanId", quantity
             FROM subscriptions
            WHERE "billingSource" = 'PAYPAL'::"SubscriptionBillingSource"
              AND status = 'ACTIVE'::"SubscriptionStatus"
              AND "pendingPaypalSubscriptionId" IS NOT NULL AND "pendingPricingTierId" IS NOT NULL
              AND ($1::int IS NULL OR "companyId" = $1)
            ORDER BY id"#,
    )
    .bind(company_id)
    .fetch_all(&ctx.state.db)
    .await?;
    tracing::info!(
        "[FORCE-PLAN-CHANGE] Found {} subscription(s) with pending plan change",
        subs.len()
    );

    let mut details = Vec::new();
    let (mut processed, mut skipped) = (0, 0);
    for sub in &subs {
        match one(ctx, sub).await {
            Ok(Some(msg)) => {
                tracing::info!("[FORCE-PLAN-CHANGE] Subscription {}: {msg}", sub.id);
                details.push(detail(sub, "processed", &msg));
                processed += 1;
            }
            Ok(None) => {
                let msg = format!(
                    "Pending tier {} not found — skipped",
                    sub.pending_pricing_tier_id
                        .map_or_else(|| "null".to_owned(), |t| t.to_string())
                );
                tracing::warn!("[FORCE-PLAN-CHANGE] Subscription {}: {msg}", sub.id);
                details.push(detail(sub, "skipped", &msg));
                skipped += 1;
            }
            Err(err) => {
                let msg = format!("Error: {err}");
                tracing::error!("[FORCE-PLAN-CHANGE] Subscription {}: {msg}", sub.id);
                details.push(detail(sub, "error", &msg));
            }
        }
    }
    tracing::info!("[FORCE-PLAN-CHANGE] Done — processed: {processed}, skipped: {skipped}");
    Ok(json!({
        "success": true,
        "processed": processed,
        "skipped": skipped,
        "details": details,
    }))
}

/// `Ok(None)` = tier tertunda tidak ada (dilewati).
async fn one(ctx: &Ctx, sub: &PendingSub) -> ApiResult<Option<String>> {
    let tier: Option<Tier> = sqlx::query_as(
        r#"SELECT id, name, "pricePerUserUsd", "billingInterval", "maxUsers", "maxProject",
                  "useIdleDetection", "useCheatDetection", "useReports", "useTimesheetApproval",
                  "useActivityLevel", "useIntegrations"
             FROM "pricingTiers" WHERE id = $1"#,
    )
    .bind(sub.pending_pricing_tier_id)
    .fetch_optional(&ctx.state.db)
    .await?;
    let Some(tier) = tier else {
        return Ok(None);
    };

    let now = ctx.now;
    let billing_interval = if tier.billing_interval.is_empty() {
        "MONTH".to_owned()
    } else {
        tier.billing_interval.clone()
    };
    let is_yearly = billing_interval == "YEAR";
    let new_end = if is_yearly {
        add_one_year(now)
    } else {
        add_days(now, 30)
    };
    let next_invoice: NaiveDateTime = if is_yearly {
        crate::jsdate::next_invoice_date(now, new_end)
    } else {
        add_days(new_end, -3)
    };
    // `subscription.quantity || 1`.
    let qty = if sub.quantity == 0 { 1 } else { sub.quantity };

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
    .bind(&billing_interval)
    .bind(now)
    .bind(new_end)
    .bind(next_invoice)
    .bind(qty)
    .execute(&ctx.state.db)
    .await?;

    // `prisma.company.update` (ber-audit) — batas & fitur tier baru.
    let pre = ctx
        .audit
        .pre_read(Model::Company, Lookup::Id(sub.company_id))
        .await;
    let n = sqlx::query(
        r#"UPDATE company SET "maxUsers" = $2, "maxProject" = $3, "useIdleDetection" = $4,
                  "useCheatDetection" = $5, "useReports" = $6, "useTimesheetApproval" = $7,
                  "useActivityLevel" = $8, "useIntegrations" = $9
            WHERE id = $1"#,
    )
    .bind(sub.company_id)
    .bind(tier.max_users)
    .bind(tier.max_project)
    .bind(tier.use_idle_detection)
    .bind(tier.use_cheat_detection)
    .bind(tier.use_reports)
    .bind(tier.use_timesheet_approval)
    .bind(tier.use_activity_level)
    .bind(tier.use_integrations)
    .execute(&ctx.state.db)
    .await?
    .rows_affected();
    if n == 0 {
        return Err(ApiError::internal(anyhow::anyhow!(
            "An operation failed because it depends on one or more records that were required but not found. Record to update not found."
        )));
    }
    if let Some(row) = ctx
        .audit
        .result_row(&ctx.state.db, Model::Company, Lookup::Id(sub.company_id))
        .await
    {
        ctx.audit.record_update(Model::Company, pre, &row).await;
    }
    // Override paket per company (spec 007) harus menang atas batas tier yang baru disalin.
    reapply_plan_override_after_tier_sync(&ctx.state, &ctx.audit, sub.company_id).await;

    let pending: Option<(i32, String)> = sqlx::query_as(
        r#"SELECT id, "invoiceNumber" FROM invoices
            WHERE "subscriptionId" = $1 AND status = 'UNPAID'::"InvoiceStatus"
            ORDER BY "createdAt" DESC LIMIT 1"#,
    )
    .bind(sub.id)
    .fetch_optional(&ctx.state.db)
    .await?;
    let force_payment_id = format!("FORCE-ADMIN-{}", now.and_utc().timestamp_millis());
    if let Some((invoice_id, number)) = pending {
        sqlx::query(
            r#"UPDATE invoices SET status = 'PAID'::"InvoiceStatus", "paypalPaymentId" = $2,
                      "expiredAt" = $3
                WHERE id = $1"#,
        )
        .bind(invoice_id)
        .bind(&force_payment_id)
        .bind(new_end)
        .execute(&ctx.state.db)
        .await?;
        tracing::info!("[FORCE-PLAN-CHANGE] Invoice {number} marked PAID");
    } else {
        let multiplier = if is_yearly { 12.0 } else { 1.0 };
        let qty_f = f64::from(qty);
        let sub_price = qty_f * tier.price_per_user_usd * multiplier;
        let invoice_number = generate_invoice_number(ctx.now_utc());
        let description = format!(
            "{} Subscription (plan change to {}) - {qty} user(s) @ ${}/user{}",
            if is_yearly { "Yearly" } else { "Monthly" },
            tier.name,
            num_string(tier.price_per_user_usd),
            if is_yearly {
                "/month (billed yearly)"
            } else {
                ""
            }
        );
        let mut tx = ctx.state.db.begin().await?;
        insert_invoice(
            &mut tx,
            &NewInvoice {
                subscription_id: sub.id,
                invoice_number: &invoice_number,
                payment_url: "",
                paypal_payment_id: Some(&force_payment_id),
                sub_price,
                total_price: sub_price,
                invoice_type: "SUBSCRIPTION",
                status: "PAID",
                expired_at: new_end,
                created_at: now,
            },
            &[Item {
                description,
                quantity: qty_f,
                unit_price: tier.price_per_user_usd * multiplier,
                total_price: sub_price,
            }],
        )
        .await?;
        tx.commit().await?;
        tracing::info!("[FORCE-PLAN-CHANGE] Fallback invoice {invoice_number} created PAID");
    }

    Ok(Some(format!(
        "Plan change applied — new tier: {}, endDate: {}",
        tier.name,
        js_to_string(new_end)
    )))
}
