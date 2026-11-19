//! `handleUnpaidProratedUsersExpiration` (tasks.service.ts:2027),
//! `handleUnpaidProratedUsersExpirationForce` (:2044) → `runProratedUsersExpiration` (:2072).
//!
//! Invoice PRORATION yang UNPAID > 7 hari (force: tanpa tunggu) di-expire per invoice dalam SATU
//! transaksi: klaim CAS `UNPAID → EXPIRED`, kunci baris subscription (`FOR UPDATE`), nonaktifkan
//! user tambahan (posisi `[prev .. lastBilled-1]` urut `createdAt, id`), keluarkan dari divisi,
//! catat id `userCompany` di invoice, turunkan `quantity`/`lastBilledQuantity`.

use mz_account::division_membership::remove_users_from_divisions_in_tx;
use mz_audit::{Lookup, Model, Row};
use mz_core::jsfmt::num_string;
use mz_core::{ApiError, ApiResult};
use serde_json::{Value, json};
use sqlx::FromRow;

use crate::Ctx;
use crate::jsdate::add_days;
use crate::scheduler::PRORATION_GRACE_DAYS;

/// `actorName` audit.
pub const ACTOR: &str = "unpaid-prorated-users-expiration";

#[derive(FromRow)]
struct Inv {
    id: i32,
    #[sqlx(rename = "invoiceNumber")]
    invoice_number: String,
    #[sqlx(rename = "subscriptionId")]
    subscription_id: i32,
    #[sqlx(rename = "companyId")]
    company_id: i32,
    prorated: f64,
}

struct Expired {
    company_id: i32,
    deactivated: usize,
    previous_last_billed: f64,
    user_ids: Vec<i32>,
}

/// Cron 04:00 WIB (galat dicatat, tidak dilempar).
pub async fn handle_cron(ctx: &Ctx) {
    tracing::info!("Running unpaid proration expiration check...");
    if let Err(err) = run(ctx, None, false).await {
        tracing::error!(error = %err, "Unpaid proration expiration check failed");
    }
}

fn detail(number: &str, company_id: i32, deactivated: usize, message: &str) -> Value {
    json!({
        "invoiceNumber": number,
        "companyId": company_id,
        "deactivated": deactivated,
        "message": message,
    })
}

/// `runProratedUsersExpiration(companyId?, force)`.
pub async fn run(ctx: &Ctx, company_id: Option<i32>, force: bool) -> ApiResult<Value> {
    if force {
        tracing::info!(
            "[FORCE-PRORATION-EXPIRY] Running{}...",
            company_id
                .map(|c| format!(" for companyId={c}"))
                .unwrap_or_else(|| " (all companies)".to_owned())
        );
    }
    let seven_days_ago = add_days(ctx.now, -PRORATION_GRACE_DAYS);
    // specs/007 R-10: invoice PRORATION pada baris yang sudah MANUAL tidak di-expire.
    let invoices: Vec<Inv> = sqlx::query_as(
        r#"SELECT i.id, i."invoiceNumber", i."subscriptionId", s."companyId",
                  COALESCE((SELECT SUM(it.quantity) FROM "invoiceItems" it WHERE it."invoiceId" = i.id), 0)::float8
                    AS prorated
             FROM invoices i JOIN subscriptions s ON s.id = i."subscriptionId"
            WHERE i."invoiceType" = 'PRORATION'::"InvoiceType" AND i.status = 'UNPAID'::"InvoiceStatus"
              AND s."billingSource" = 'PAYPAL'::"SubscriptionBillingSource"
              AND ($1::int IS NULL OR s."companyId" = $1)
              AND ($2 OR i."createdAt" <= $3)
            ORDER BY i."createdAt" ASC, i.id ASC"#,
    )
    .bind(company_id)
    .bind(force)
    .bind(seven_days_ago)
    .fetch_all(&ctx.state.db)
    .await?;
    tracing::info!(
        "[PRORATION-EXPIRY] Found {} unpaid proration invoice(s) to expire",
        invoices.len()
    );

    let mut details = Vec::new();
    let mut expired = 0;
    for inv in &invoices {
        match expire_one(ctx, inv).await {
            Ok(None) => {
                let msg = "Skipped — invoice no longer UNPAID";
                tracing::info!("[PRORATION-EXPIRY] Invoice {}: {msg}", inv.invoice_number);
                details.push(detail(&inv.invoice_number, inv.company_id, 0, msg));
            }
            Ok(Some(r)) => {
                let msg = format!(
                    "{} user(s) deactivated, lastBilledQuantity reset to {}",
                    r.deactivated,
                    num_string(r.previous_last_billed)
                );
                tracing::info!("[PRORATION-EXPIRY] Invoice {}: {msg}", inv.invoice_number);
                details.push(detail(
                    &inv.invoice_number,
                    r.company_id,
                    r.deactivated,
                    &msg,
                ));
                expired += 1;
                // Cache in-process Rust untuk user yang keanggotaannya baru dinonaktifkan.
                for uid in r.user_ids {
                    ctx.state.invalidate_user_access(uid);
                    mz_core::invalidation::forget_user_me(&ctx.state, uid).await;
                }
            }
            Err(err) => {
                let msg = format!("Error: {err}");
                tracing::error!("[PRORATION-EXPIRY] Invoice {}: {msg}", inv.invoice_number);
                details.push(detail(&inv.invoice_number, inv.company_id, 0, &msg));
            }
        }
    }
    tracing::info!("[PRORATION-EXPIRY] Done — expired: {expired}");
    Ok(json!({ "success": true, "expired": expired, "details": details }))
}

/// Satu invoice dalam satu transaksi. `Ok(None)` = klaim kalah (sudah bukan UNPAID).
async fn expire_one(ctx: &Ctx, inv: &Inv) -> ApiResult<Option<Expired>> {
    let mut tx = ctx.state.db.begin().await?;
    let claimed = sqlx::query(
        r#"UPDATE invoices SET status = 'EXPIRED'::"InvoiceStatus"
            WHERE id = $1 AND status = 'UNPAID'::"InvoiceStatus""#,
    )
    .bind(inv.id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if claimed != 1 {
        tx.rollback().await?;
        return Ok(None);
    }

    sqlx::query(r#"SELECT id FROM subscriptions WHERE id = $1 FOR UPDATE"#)
        .bind(inv.subscription_id)
        .execute(&mut *tx)
        .await?;
    let sub: Option<(i32, i32, i32)> = sqlx::query_as(
        r#"SELECT id, "companyId", "lastBilledQuantity" FROM subscriptions WHERE id = $1"#,
    )
    .bind(inv.subscription_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((sub_id, company_id, last_billed)) = sub else {
        return Err(ApiError::internal(anyhow::anyhow!(
            "Subscription {} not found",
            inv.subscription_id
        )));
    };

    let current = f64::from(last_billed);
    let previous = (current - inv.prorated).max(0.0);
    let take = current - previous;
    if previous.fract() != 0.0 || take.fract() != 0.0 {
        // Prisma menolak `skip`/`take` pecahan (quantity item Float).
        return Err(ApiError::internal(anyhow::anyhow!(
            "Argument skip: Invalid value provided. Expected Int, provided Float."
        )));
    }
    #[allow(clippy::cast_possible_truncation)]
    let (skip, take_n, previous_i) = (previous as i64, take as i64, previous as i32);

    let users: Vec<(i32, i32)> = if take_n > 0 {
        sqlx::query_as(
            r#"SELECT id, "userId" FROM "userCompany" WHERE "companyId" = $1 AND status = 1
                ORDER BY "createdAt" ASC, id ASC OFFSET $2 LIMIT $3"#,
        )
        .bind(company_id)
        .bind(skip)
        .bind(take_n)
        .fetch_all(&mut *tx)
        .await?
    } else {
        Vec::new()
    };

    let uc_ids: Vec<i32> = users.iter().map(|u| u.0).collect();
    let user_ids: Vec<i32> = users.iter().map(|u| u.1).collect();
    if !users.is_empty() {
        // `tx.userCompany.updateMany({ where: { id: { in }, status: 1 }, data: { status: 0 } })` ber-audit.
        let lookup = {
            let ids = uc_ids.clone();
            Lookup::filter(move |qb| {
                qb.push("id = ANY(").push_bind(ids.clone());
                qb.push(") AND status = 1");
            })
        };
        let pre = ctx.audit.pre_read_many(Model::UserCompany, lookup).await;
        let affected =
            sqlx::query(r#"UPDATE "userCompany" SET status = 0 WHERE id = ANY($1) AND status = 1"#)
                .bind(&uc_ids)
                .execute(&mut *tx)
                .await?
                .rows_affected();
        ctx.audit
            .record_update_many(
                Model::UserCompany,
                pre,
                affected,
                &Row::new().with("status", 0),
                json!({ "id": { "in": uc_ids }, "status": 1 }),
            )
            .await;

        // specs/006 R11: keluar dari divisi company ini (id USER, bukan id userCompany).
        remove_users_from_divisions_in_tx(&mut tx, &ctx.audit, &user_ids, Some(company_id)).await?;

        sqlx::query(
            r#"UPDATE invoices SET "prorationDeactivatedUserCompanyIds" = $2 WHERE id = $1"#,
        )
        .bind(inv.id)
        .bind(&uc_ids)
        .execute(&mut *tx)
        .await?;
    }

    sqlx::query(
        r#"UPDATE subscriptions SET quantity = $2, "lastBilledQuantity" = $2 WHERE id = $1"#,
    )
    .bind(sub_id)
    .bind(previous_i)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    Ok(Some(Expired {
        company_id,
        deactivated: users.len(),
        previous_last_billed: previous,
        user_ids,
    }))
}
