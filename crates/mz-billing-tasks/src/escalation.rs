//! `handlePaymentFailureEscalation` (tasks.service.ts:402) → `runPaymentFailureEscalation` (:414).
//!
//! - D+1: `paypalStatus = PAYMENT_FAILED` ≥ 1 hari → klaim CAS ke `PAYMENT_FAILED_D1`, email ke owner.
//! - D+7: `PAYMENT_FAILED_D1` ≥ 6 hari → satu transaksi: klaim CAS ke `ACCESS_BLOCKED` + suspend
//!   company (`status = 0`, ber-audit), lalu email goodbye.
//!
//! Idempoten: klaim bersyarat; baris yang sudah diproses / dialihkan ke MANUAL tidak cocok lagi.

use mz_admin_company::plan_limits::{Bind, update_company};
use mz_core::ApiResult;
use sqlx::FromRow;

use crate::jsdate::add_days;
use crate::{Ctx, OWNER_LATERAL, mail};

/// `actorName` audit cron ini.
pub const ACTOR: &str = "payment-failure-escalation";

#[derive(FromRow)]
struct Row {
    id: i32,
    #[sqlx(rename = "companyId")]
    company_id: i32,
    owner_email: Option<String>,
    owner_name: Option<String>,
}

async fn find(
    ctx: &Ctx,
    paypal_status: &str,
    before: chrono::NaiveDateTime,
) -> ApiResult<Vec<Row>> {
    let sql = format!(
        r#"SELECT s.id, s."companyId", o.owner_email, o.owner_name
             FROM subscriptions s {OWNER_LATERAL}
            WHERE s."billingSource" = 'PAYPAL'::"SubscriptionBillingSource"
              AND s."paypalStatus" = $1 AND s."updatedAt" <= $2
            ORDER BY s.id"#
    );
    Ok(sqlx::query_as(&sql)
        .bind(paypal_status)
        .bind(before)
        .fetch_all(&ctx.state.db)
        .await?)
}

/// Satu run (galat dicatat, tidak dilempar — seperti `try/catch` Node).
pub async fn run(ctx: &Ctx) {
    tracing::info!("Running payment failure escalation check...");
    if let Err(err) = run_inner(ctx).await {
        tracing::error!(error = %err, "Payment failure escalation check failed");
    }
}

async fn run_inner(ctx: &Ctx) -> ApiResult<()> {
    let one_day_ago = add_days(ctx.now, -1);
    let six_days_ago = add_days(ctx.now, -6);

    // --- D+1 ---
    let d1 = find(ctx, "PAYMENT_FAILED", one_day_ago).await?;
    tracing::info!("Found {} subscription(s) for D+1 reminder", d1.len());
    for sub in d1 {
        if let Err(err) = d1_one(ctx, &sub).await {
            tracing::error!("D+1 escalation failed for subscription {}: {err}", sub.id);
        }
    }

    // --- D+7 ---
    let d7 = find(ctx, "PAYMENT_FAILED_D1", six_days_ago).await?;
    tracing::info!("Found {} subscription(s) for D+7 access block", d7.len());
    for sub in d7 {
        if let Err(err) = d7_one(ctx, &sub).await {
            tracing::error!("D+7 escalation failed for subscription {}: {err}", sub.id);
        }
    }
    Ok(())
}

async fn d1_one(ctx: &Ctx, sub: &Row) -> ApiResult<()> {
    // Klaim dulu (compare-and-set), email hanya bila klaim menang.
    let n = sqlx::query(
        r#"UPDATE subscriptions SET "paypalStatus" = 'PAYMENT_FAILED_D1', "updatedAt" = $2
            WHERE id = $1 AND "billingSource" = 'PAYPAL'::"SubscriptionBillingSource"
              AND "paypalStatus" = 'PAYMENT_FAILED'"#,
    )
    .bind(sub.id)
    .bind(ctx.now)
    .execute(&ctx.state.db)
    .await?
    .rows_affected();
    if n != 1 {
        tracing::info!(
            "D+1 escalation skipped for subscription {} — changed before update",
            sub.id
        );
        return Ok(());
    }
    if let Some(email) = &sub.owner_email {
        let name = sub.owner_name.as_deref().unwrap_or_default();
        ctx.mail
            .send(&ctx.state, mail::d1_payment_failed(&ctx.state, email, name))
            .await;
        tracing::info!("D+1 payment reminder sent to {email}");
    }
    Ok(())
}

async fn d7_one(ctx: &Ctx, sub: &Row) -> ApiResult<()> {
    let mut tx = ctx.state.db.begin().await?;
    let n = sqlx::query(
        r#"UPDATE subscriptions SET "paypalStatus" = 'ACCESS_BLOCKED', "updatedAt" = $2
            WHERE id = $1 AND "billingSource" = 'PAYPAL'::"SubscriptionBillingSource"
              AND "paypalStatus" = 'PAYMENT_FAILED_D1'"#,
    )
    .bind(sub.id)
    .bind(ctx.now)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if n != 1 {
        tx.rollback().await?;
        tracing::info!(
            "D+7 escalation skipped for subscription {} — changed before update",
            sub.id
        );
        return Ok(());
    }
    // Suspend company (`tx.company.update`, ber-audit).
    update_company(
        &mut tx,
        &ctx.audit,
        sub.company_id,
        &[("status".to_owned(), Bind::I32(0))],
    )
    .await?;
    tx.commit().await?;

    if let Some(email) = &sub.owner_email {
        let name = sub.owner_name.as_deref().unwrap_or_default();
        ctx.mail
            .send(
                &ctx.state,
                mail::goodbye_access_blocked(&ctx.state, email, name),
            )
            .await;
        tracing::info!("Access blocked email sent to {email}");
    }
    tracing::info!(
        "Company {} suspended after 7 days of failed payment",
        sub.company_id
    );
    Ok(())
}
