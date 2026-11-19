//! `handleSubscriptionExpiration` (tasks.service.ts:2283), `handleSubscriptionExpirationForce`
//! (:2297) → `runExpirationCheck` (:2328).
//!
//! Tahap (urutan Node): D-1 (autoRenew=false, berakhir ≤ 24 jam) dan D-0 (sudah lewat) khusus
//! PayPal → batalkan langganan PayPal (galat di-`warn`) lalu `UPDATE` bersyarat ke INACTIVE;
//! MANUAL D-0; TRIAL yang lewat (company `isTrial=false`, ber-audit); paket Free yang lewat masa
//! tenggang (bila `FREE_SUBSCRIPTION_DAYS`); sapu ulang INACTIVE PayPal yang belum di tier Free →
//! `mz_billing::auto_downgrade_to_free`. Semua tulisan bersyarat → aman dijalankan ulang.

use chrono::NaiveDateTime;
use mz_admin_company::plan_limits::{Bind, update_company};
use mz_billing::auto_downgrade_to_free;
use mz_core::ApiResult;
use serde_json::{Value, json};
use sqlx::{FromRow, Postgres, QueryBuilder};

use crate::Ctx;
use crate::jsdate::{add_days, js_to_string};

/// `actorName` audit (Node memakainya di jalur force; lihat deviasi untuk cron).
pub const ACTOR: &str = "subscription-expiration";

#[derive(FromRow)]
struct Sub {
    id: i32,
    #[sqlx(rename = "companyId")]
    company_id: i32,
    #[sqlx(rename = "paypalSubscriptionId")]
    paypal_subscription_id: Option<String>,
    #[sqlx(rename = "paypalStatus")]
    paypal_status: Option<String>,
    #[sqlx(rename = "endDate")]
    end_date: NaiveDateTime,
    #[sqlx(rename = "autoRenew")]
    auto_renew: bool,
}

const SUB_COLS: &str = r#"SELECT id, "companyId", "paypalSubscriptionId", "paypalStatus", "endDate", "autoRenew" FROM subscriptions"#;

/// `baseWhere`: PayPal + ACTIVE (+ company).
fn push_base(qb: &mut QueryBuilder<'_, Postgres>, company_id: Option<i32>) {
    qb.push(
        r#" "billingSource" = 'PAYPAL'::"SubscriptionBillingSource" AND status = 'ACTIVE'::"SubscriptionStatus""#,
    );
    if let Some(c) = company_id {
        qb.push(r#" AND "companyId" = "#).push_bind(c);
    }
}

fn detail(id: i32, company_id: i32, stage: &str, message: &str) -> Value {
    json!({
        "subscriptionId": id,
        "companyId": company_id,
        "stage": stage,
        "message": message,
    })
}

/// Cron 03:00 WIB (galat dicatat, tidak dilempar).
pub async fn handle_cron(ctx: &Ctx) {
    tracing::info!("Running subscription expiration check...");
    if let Err(err) = run(ctx, None).await {
        tracing::error!(error = %err, "Subscription expiration check failed");
    }
}

#[derive(Default)]
struct Acc {
    details: Vec<Value>,
}

/// D-1 / D-0: batalkan PayPal bila masih bisa dibatalkan, lalu INACTIVE bersyarat.
async fn expire_paypal(
    ctx: &Ctx,
    company_id: Option<i32>,
    sub: &Sub,
    stage: &str,
    reason: &str,
    success_msg: String,
) -> ApiResult<bool> {
    if let Some(pid) = sub
        .paypal_subscription_id
        .as_deref()
        .filter(|s| !s.is_empty())
        && matches!(sub.paypal_status.as_deref(), Some("ACTIVE" | "SUSPENDED"))
        && let Err(err) = ctx.paypal.cancel_subscription(pid, Some(reason)).await
    {
        tracing::warn!("Failed to cancel PayPal sub {pid} on {stage}: {err}");
    }
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
        r#"UPDATE subscriptions SET status = 'INACTIVE'::"SubscriptionStatus", "paypalStatus" = 'CANCELLED' WHERE id = "#,
    );
    qb.push_bind(sub.id).push(" AND");
    push_base(&mut qb, company_id);
    let count = qb.build().execute(&ctx.state.db).await?.rows_affected();
    let msg = if count > 0 {
        success_msg
    } else {
        "Skipped — subscription changed before update".to_owned()
    };
    tracing::info!("Subscription {}: {msg}", sub.id);
    Ok(count > 0)
}

/// `runExpirationCheck(companyId?)`.
pub async fn run(ctx: &Ctx, company_id: Option<i32>) -> ApiResult<Value> {
    let now = ctx.now;
    let tomorrow = add_days(now, 1);
    let mut acc = Acc::default();
    let (mut d1_expired, mut d0_expired) = (0, 0);

    // --- D-1 ---
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(SUB_COLS);
    qb.push(" WHERE");
    push_base(&mut qb, company_id);
    qb.push(r#" AND "autoRenew" = false AND "pendingPaypalSubscriptionId" IS NULL AND "pricePerUser" > 0 AND "endDate" >= "#)
        .push_bind(now)
        .push(r#" AND "endDate" <= "#)
        .push_bind(tomorrow)
        .push(" ORDER BY id");
    let d1: Vec<Sub> = qb.build_query_as().fetch_all(&ctx.state.db).await?;
    tracing::info!(
        "Found {} subscription(s) expiring within 24 hours (D-1)",
        d1.len()
    );
    for sub in &d1 {
        let ok_msg = format!(
            "Marked INACTIVE at D-1 (endDate: {})",
            js_to_string(sub.end_date)
        );
        match expire_paypal(
            ctx,
            company_id,
            sub,
            "D-1",
            "Subscription expiring — auto-renewal disabled",
            ok_msg.clone(),
        )
        .await
        {
            Ok(true) => {
                d1_expired += 1;
                acc.details
                    .push(detail(sub.id, sub.company_id, "D-1", &ok_msg));
            }
            Ok(false) => acc.details.push(detail(
                sub.id,
                sub.company_id,
                "D-1",
                "Skipped — subscription changed before update",
            )),
            Err(err) => {
                let msg = format!("Error: {err}");
                tracing::error!("Subscription {} D-1 failed: {msg}", sub.id);
                acc.details
                    .push(detail(sub.id, sub.company_id, "D-1", &msg));
            }
        }
    }

    // --- D-0 ---
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(SUB_COLS);
    qb.push(" WHERE");
    push_base(&mut qb, company_id);
    qb.push(
        r#" AND "pendingPaypalSubscriptionId" IS NULL AND "pricePerUser" > 0 AND "endDate" <= "#,
    )
    .push_bind(now)
    .push(" ORDER BY id");
    let d0: Vec<Sub> = qb.build_query_as().fetch_all(&ctx.state.db).await?;
    tracing::info!("Found {} subscription(s) already expired (D-0)", d0.len());
    for sub in &d0 {
        let ok_msg = format!(
            "Marked INACTIVE at D-0 (endDate: {}, autoRenew: {})",
            js_to_string(sub.end_date),
            sub.auto_renew
        );
        match expire_paypal(
            ctx,
            company_id,
            sub,
            "D-0",
            "Subscription expired",
            ok_msg.clone(),
        )
        .await
        {
            Ok(true) => {
                d0_expired += 1;
                acc.details
                    .push(detail(sub.id, sub.company_id, "D-0", &ok_msg));
            }
            Ok(false) => acc.details.push(detail(
                sub.id,
                sub.company_id,
                "D-0",
                "Skipped — subscription changed before update",
            )),
            Err(err) => {
                let msg = format!("Error: {err}");
                tracing::error!("Subscription {} D-0 failed: {msg}", sub.id);
                acc.details
                    .push(detail(sub.id, sub.company_id, "D-0", &msg));
            }
        }
    }

    // --- MANUAL D-0 (specs/007 R-10) ---
    // `planEntitlements.clear()`: Rust membaca gerbang paket per request (`mz_core::plan`), tidak ada cache.
    let manual_expired = match sqlx::query(
        r#"UPDATE subscriptions SET status = 'INACTIVE'::"SubscriptionStatus"
            WHERE "billingSource" = 'MANUAL'::"SubscriptionBillingSource"
              AND status = 'ACTIVE'::"SubscriptionStatus" AND "endDate" <= $1
              AND ($2::int IS NULL OR "companyId" = $2)"#,
    )
    .bind(now)
    .bind(company_id)
    .execute(&ctx.state.db)
    .await
    {
        Ok(r) => {
            tracing::info!(
                "Marked {} manual subscription(s) INACTIVE (MANUAL D-0)",
                r.rows_affected()
            );
            r.rows_affected()
        }
        Err(err) => {
            tracing::error!("MANUAL D-0 expiration failed: {err}");
            0
        }
    };

    // --- TRIAL EXPIRED ---
    let trials: Vec<Sub> = sqlx::query_as(&format!(
        r#"{SUB_COLS} WHERE status = 'TRIAL'::"SubscriptionStatus" AND "endDate" <= $1
             AND ($2::int IS NULL OR "companyId" = $2) ORDER BY id"#
    ))
    .bind(now)
    .bind(company_id)
    .fetch_all(&ctx.state.db)
    .await?;
    tracing::info!("Found {} trial subscription(s) expired", trials.len());
    for sub in &trials {
        match expire_trial(ctx, sub).await {
            Ok(()) => {
                let msg = format!(
                    "Trial expired — marked INACTIVE, isTrial=false (endDate: {})",
                    js_to_string(sub.end_date)
                );
                tracing::info!("Subscription {}: {msg}", sub.id);
                acc.details
                    .push(detail(sub.id, sub.company_id, "TRIAL-EXPIRED", &msg));
            }
            Err(err) => {
                let msg = format!("Error: {err}");
                tracing::error!("Subscription {} trial expiration failed: {msg}", sub.id);
                acc.details
                    .push(detail(sub.id, sub.company_id, "TRIAL-EXPIRED", &msg));
            }
        }
    }

    // --- FREE EXPIRED (hanya bila FREE_SUBSCRIPTION_DAYS diisi) ---
    if ctx.state.config.free_subscription_days.is_some() {
        let cutoff = add_days(now, -ctx.state.config.free_subscription_grace_days);
        let free_where = r#""billingSource" = 'PAYPAL'::"SubscriptionBillingSource"
              AND status = 'ACTIVE'::"SubscriptionStatus" AND "pricePerUser" <= 0 AND "endDate" <= $1
              AND ($2::int IS NULL OR "companyId" = $2)"#;
        let free: Vec<(i32, i32, NaiveDateTime)> = sqlx::query_as(&format!(
            r#"SELECT id, "companyId", "endDate" FROM subscriptions WHERE {free_where} ORDER BY id"#
        ))
        .bind(cutoff)
        .bind(company_id)
        .fetch_all(&ctx.state.db)
        .await?;
        tracing::info!("Found {} free subscription(s) expired", free.len());
        for (id, cid, end) in free {
            let res = sqlx::query(&format!(
                r#"UPDATE subscriptions SET status = 'INACTIVE'::"SubscriptionStatus"
                    WHERE id = $3 AND {free_where}"#
            ))
            .bind(cutoff)
            .bind(company_id)
            .bind(id)
            .execute(&ctx.state.db)
            .await;
            let msg = match res {
                Ok(r) if r.rows_affected() > 0 => format!(
                    "Free plan expired — marked INACTIVE (endDate: {})",
                    js_to_string(end)
                ),
                Ok(_) => "Skipped — subscription changed before update".to_owned(),
                Err(err) => {
                    let msg = format!("Error: {err}");
                    tracing::error!("Subscription {id} free expiration failed: {msg}");
                    acc.details.push(detail(id, cid, "FREE-EXPIRED", &msg));
                    continue;
                }
            };
            tracing::info!("Subscription {id}: {msg}");
            acc.details.push(detail(id, cid, "FREE-EXPIRED", &msg));
        }
    }

    // --- AUTO-DOWNGRADE TO FREE: semua INACTIVE PayPal yang belum di tier Free ---
    let inactive: Vec<(i32, i32)> = sqlx::query_as(
        r#"SELECT s.id, s."companyId" FROM subscriptions s
             JOIN "pricingTiers" pt ON pt.id = s."pricingTierId"
            WHERE s."billingSource" = 'PAYPAL'::"SubscriptionBillingSource"
              AND s.status = 'INACTIVE'::"SubscriptionStatus"
              AND NOT (pt.name ILIKE 'Free')
              AND ($1::int IS NULL OR s."companyId" = $1)
            ORDER BY s.id"#,
    )
    .bind(company_id)
    .fetch_all(&ctx.state.db)
    .await?;
    tracing::info!(
        "Found {} INACTIVE subscription(s) not yet on Free tier",
        inactive.len()
    );
    let mut downgraded = 0;
    for (id, cid) in inactive {
        let msg = match auto_downgrade_to_free(&ctx.state, &ctx.audit, &ctx.paypal, id).await {
            Ok(true) => {
                downgraded += 1;
                "Downgraded to Free tier".to_owned()
            }
            Ok(false) => {
                "Skipped — no matching Free tier for this productType, or already Free".to_owned()
            }
            Err(err) => {
                let msg = format!("Error: {err}");
                tracing::error!("Subscription {id} auto-downgrade failed: {msg}");
                acc.details
                    .push(detail(id, cid, "AUTO-DOWNGRADE-FREE", &msg));
                continue;
            }
        };
        tracing::info!("Subscription {id}: {msg}");
        acc.details
            .push(detail(id, cid, "AUTO-DOWNGRADE-FREE", &msg));
    }

    Ok(json!({
        "success": true,
        "d1Expired": d1_expired,
        "d0Expired": d0_expired,
        "manualExpired": manual_expired,
        "downgradedToFree": downgraded,
        "details": acc.details,
    }))
}

/// `subscriptions.update` INACTIVE + `company.update({ isTrial: false })` (ber-audit), dua tulisan
/// terpisah seperti Node.
async fn expire_trial(ctx: &Ctx, sub: &Sub) -> ApiResult<()> {
    sqlx::query(
        r#"UPDATE subscriptions SET status = 'INACTIVE'::"SubscriptionStatus" WHERE id = $1"#,
    )
    .bind(sub.id)
    .execute(&ctx.state.db)
    .await?;
    let mut conn = ctx.state.db.acquire().await?;
    update_company(
        &mut conn,
        &ctx.audit,
        sub.company_id,
        &[("isTrial".to_owned(), Bind::Bool(false))],
    )
    .await
}
