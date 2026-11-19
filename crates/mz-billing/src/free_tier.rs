//! Port `activateFreeTier`, `resolveFreeTierFor`, `autoDowngradeToFree`
//! (`src/invoices/invoices.service.ts`) + helper masa Free (`src/subscription-lock/free-subscription.ts`).
//!
//! PayPal tidak dipanggil langsung: pemanggil memberi [`PaypalSubscriptionCanceller`] (di Node
//! `PaypalService.cancelSubscription(id, reason)`); klien `mz-paypal` cukup dibungkus adaptor.

use std::future::Future;

use chrono::{Duration, NaiveDate, NaiveDateTime};
use mz_admin_company::common::{CompanyChange, after_company_change};
use mz_admin_company::plan_limits::{Bind, reapply_plan_override_after_tier_sync, update_company};
use mz_audit::AuditHandle;
use mz_core::config::Config;
use mz_core::enums::SubscriptionStatus;
use mz_core::time::now_naive;
use mz_core::{ApiError, ApiResult, AppState};
use serde_json::{Value, json};
use sqlx::FromRow;

/// Node: free-subscription.ts:73 (`FREE_FOREVER_END_DATE` = `new Date('2099-12-31')`).
pub fn free_forever_end_date() -> NaiveDateTime {
    NaiveDate::from_ymd_opt(2099, 12, 31)
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .unwrap_or(NaiveDateTime::MAX)
}

/// Node: free-subscription.ts:76 (`freeSubscriptionEndDate`): `from + FREE_SUBSCRIPTION_DAYS`,
/// atau `fallback` bila fitur itu mati (env kosong / bukan angka / ≤ 0).
pub fn free_subscription_end_date(
    cfg: &Config,
    from: NaiveDateTime,
    fallback: NaiveDateTime,
) -> NaiveDateTime {
    match cfg.free_subscription_days {
        Some(days) if days > 0 => from + Duration::milliseconds(days * 24 * 60 * 60 * 1000),
        _ => fallback,
    }
}

/// Node: free-subscription.ts:89 (`isZeroPricedTier`): kedua kolom harga nol.
pub fn is_zero_priced_tier(base_price: f64, price_per_user_usd: f64) -> bool {
    !positive(base_price) && !positive(price_per_user_usd)
}

/// JS `x > 0` (NaN → false).
fn positive(x: f64) -> bool {
    x > 0.0
}

/// Pembatal langganan PayPal (`PaypalService.cancelSubscription(id, reason)`). Galat dikembalikan
/// sebagai teks pesan; `activate_free_tier` hanya mencatatnya (`warn`) dan lanjut, seperti Node.
pub trait PaypalSubscriptionCanceller: Send + Sync {
    fn cancel_subscription(
        &self,
        paypal_subscription_id: &str,
        reason: &str,
    ) -> impl Future<Output = Result<(), String>> + Send;
}

/// Implementasi produksi: klien `mz-paypal` (galat = `err.message`). Dulu dibungkus terpisah di
/// `mz-invoices` dan `mz-billing-tasks`.
impl PaypalSubscriptionCanceller for mz_paypal::PaypalClient {
    fn cancel_subscription(
        &self,
        paypal_subscription_id: &str,
        reason: &str,
    ) -> impl Future<Output = Result<(), String>> + Send {
        let (client, id, reason) = (
            self.clone(),
            paypal_subscription_id.to_owned(),
            reason.to_owned(),
        );
        async move {
            client
                .cancel_subscription(&id, Some(&reason))
                .await
                .map_err(|e| e.message)
        }
    }
}

/// Tier harga yang dipakai `activateFreeTier` (kolom skalar `pricingTiers` yang disalin ke company).
#[derive(Debug, Clone, FromRow, PartialEq)]
pub struct FreeTier {
    pub id: i32,
    pub name: String,
    #[sqlx(rename = "productType")]
    pub product_type: String,
    #[sqlx(rename = "maxUsers")]
    pub max_users: i32,
    #[sqlx(rename = "maxProject")]
    pub max_project: i32,
    #[sqlx(rename = "screenshotResolution")]
    pub screenshot_resolution: String,
    #[sqlx(rename = "screenshotRetentionDays")]
    pub screenshot_retention_days: i32,
    #[sqlx(rename = "reportExportsPerMonth")]
    pub report_exports_per_month: i32,
    #[sqlx(rename = "dataHistoryMonths")]
    pub data_history_months: i32,
    #[sqlx(rename = "budgetAlertDeliveryChannel")]
    pub budget_alert_delivery_channel: String,
    #[sqlx(rename = "clientProofExportAllowed")]
    pub client_proof_export_allowed: bool,
}

const TIER_COLUMNS: &str = r#"id, name, "productType"::text AS "productType", "maxUsers", "maxProject",
    "screenshotResolution", "screenshotRetentionDays", "reportExportsPerMonth", "dataHistoryMonths",
    "budgetAlertDeliveryChannel"::text AS "budgetAlertDeliveryChannel", "clientProofExportAllowed""#;

impl FreeTier {
    /// `prisma.pricingTiers.findUnique({ where: { id } })` (kolom yang dipakai saja).
    pub async fn by_id<'e>(exec: impl sqlx::PgExecutor<'e>, id: i32) -> sqlx::Result<Option<Self>> {
        sqlx::query_as(&format!(
            r#"SELECT {TIER_COLUMNS} FROM "pricingTiers" WHERE id = $1"#
        ))
        .bind(id)
        .fetch_optional(exec)
        .await
    }

    /// `prisma.pricingTiers.findFirst({ where: { uuid } })`.
    pub async fn by_uuid<'e>(
        exec: impl sqlx::PgExecutor<'e>,
        uuid: &uuid::Uuid,
    ) -> sqlx::Result<Option<Self>> {
        sqlx::query_as(&format!(
            r#"SELECT {TIER_COLUMNS} FROM "pricingTiers" WHERE uuid = $1"#
        ))
        .bind(uuid)
        .fetch_optional(exec)
        .await
    }
}

/// Node: invoices.service.ts:2140 (`resolveFreeTierFor`). Tier Free yang cocok dengan `productType`
/// paket yang sedang dipakai (bukan sembarang tier bernama "Free"), supaya company SALES-only tidak
/// naik ke Free FULL saat turun paket. `product_type` = `SALES` | `TIME` | `FULL`.
pub async fn resolve_free_tier_for<'e>(
    exec: impl sqlx::PgExecutor<'e>,
    product_type: &str,
) -> sqlx::Result<Option<FreeTier>> {
    sqlx::query_as(&format!(
        r#"SELECT {TIER_COLUMNS} FROM "pricingTiers"
            WHERE "productType" = $1::"PricingProductType" AND status = 1 AND lower(name) = 'free'
            ORDER BY id LIMIT 1"#
    ))
    .bind(product_type)
    .fetch_optional(exec)
    .await
}

/// Baris langganan yang dibaca `activateFreeTier`.
#[derive(Debug, Clone, FromRow)]
pub struct FreeTierSubscription {
    pub id: i32,
    #[sqlx(rename = "paypalSubscriptionId")]
    pub paypal_subscription_id: Option<String>,
    #[sqlx(rename = "pendingPaypalSubscriptionId")]
    pub pending_paypal_subscription_id: Option<String>,
    pub status: String,
    #[sqlx(rename = "pricePerUser")]
    pub price_per_user: f64,
    #[sqlx(rename = "endDate")]
    pub end_date: Option<NaiveDateTime>,
}

/// Syarat tambahan pada baris subscription saat ditulis (`guard` Node; di sini yang dipakai
/// `autoDowngradeToFree`: `{ billingSource, status }`).
#[derive(Debug, Clone, Copy)]
pub struct FreeTierGuard {
    pub billing_source: &'static str,
    pub status: SubscriptionStatus,
}

/// Hasil [`activate_free_tier`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FreeTierOutcome {
    Activated {
        new_plan: String,
    },
    /// `guard` tidak cocok lagi (TOCTOU): subscription, invoice, company tidak disentuh.
    Skipped,
}

impl FreeTierOutcome {
    /// Bentuk objek Node: `{ success, message, data: { newPlan } }` / `{ success:false, skipped:true, message }`.
    pub fn to_json(&self) -> Value {
        match self {
            Self::Activated { new_plan } => json!({
                "success": true,
                "message": "Free plan activated successfully",
                "data": { "newPlan": new_plan },
            }),
            Self::Skipped => json!({
                "success": false,
                "skipped": true,
                "message": "Subscription changed before the free plan switch",
            }),
        }
    }
}

/// Masa berakhir paket Free yang baru. Masa Free tidak boleh diperpanjang dengan "pindah ke Free
/// lagi": kalau langganannya belum benar-benar berbayar, tanggal habisnya dipertahankan (paling
/// lama masa Free penuh dari sekarang). Yang turun dari paket berbayar mendapat masa Free penuh.
/// `PENDING` dihitung belum berbayar walau `pricePerUser` sudah terisi.
pub fn free_tier_end_date(
    cfg: &Config,
    now: NaiveDateTime,
    price_per_user: f64,
    status: &str,
    current_end: Option<NaiveDateTime>,
) -> NaiveDateTime {
    let fresh = free_subscription_end_date(cfg, now, free_forever_end_date());
    let was_paid = price_per_user > 0.0 && status != SubscriptionStatus::PENDING.as_str();
    match current_end {
        Some(end) if !was_paid => end.min(fresh),
        _ => fresh,
    }
}

/// Node: invoices.service.ts:2008 (`activateFreeTier`).
///
/// Langkah (urutan Node): (1) batalkan langganan PayPal aktif & pending (galat hanya di-`warn`,
/// dilakukan SEBELUM guard dicek seperti Node); (2) satu transaksi: subscription → tier Free,
/// invoice `UNPAID` → `CANCELLED`, limit company disalin dari tier (tulisan company ber-audit
/// lewat `audit`); dengan `guard`, subscription ditulis dulu bersyarat dan baris yang sudah tak
/// cocok dilewati; (3) `reapplyPlanOverrideAfterTierSync`.
///
/// Deviasi: sesudah commit cache in-process (modul company, user-me) diinvalidasi; Node membiarkannya
/// kedaluwarsa sendiri (lihat `plan/deviations/mz-billing.md`).
pub async fn activate_free_tier<C: PaypalSubscriptionCanceller>(
    state: &AppState,
    audit: &AuditHandle,
    paypal: &C,
    subscription: &FreeTierSubscription,
    new_tier: &FreeTier,
    company_id: i32,
    guard: Option<FreeTierGuard>,
) -> ApiResult<FreeTierOutcome> {
    for sub_id in [
        subscription.paypal_subscription_id.as_deref(),
        subscription.pending_paypal_subscription_id.as_deref(),
    ]
    .into_iter()
    .flatten()
    .filter(|s| !s.is_empty())
    {
        if let Err(err) = paypal
            .cancel_subscription(sub_id, "Switched to free plan")
            .await
        {
            tracing::warn!("Failed to cancel PayPal sub {sub_id} on free tier switch: {err}");
        }
    }

    let now = now_naive();
    let end_date = free_tier_end_date(
        &state.config,
        now,
        subscription.price_per_user,
        &subscription.status,
        subscription.end_date,
    );

    let mut tx = state.db.begin().await?;
    if let Some(g) = guard {
        // Subscription ditulis lebih dulu: klaim bersyarat itu yang memutuskan.
        let n = write_subscription(
            &mut tx,
            subscription.id,
            new_tier.id,
            now,
            end_date,
            Some(g),
        )
        .await?;
        if n != 1 {
            tx.rollback().await?;
            return Ok(FreeTierOutcome::Skipped);
        }
        cancel_unpaid_invoices(&mut tx, subscription.id).await?;
        write_company(&mut tx, audit, company_id, new_tier, now).await?;
    } else {
        cancel_unpaid_invoices(&mut tx, subscription.id).await?;
        let n =
            write_subscription(&mut tx, subscription.id, new_tier.id, now, end_date, None).await?;
        if n != 1 {
            // `prisma.subscriptions.update` melempar bila baris tak ada.
            tx.rollback().await?;
            return Err(ApiError::internal(anyhow::anyhow!(
                "subscription {} not found",
                subscription.id
            )));
        }
        write_company(&mut tx, audit, company_id, new_tier, now).await?;
    }
    tx.commit().await?;

    // Override paket per company (spec 007) harus menang atas batas tier yang baru disalin.
    reapply_plan_override_after_tier_sync(state, audit, company_id).await;
    after_company_change(
        state,
        company_id,
        CompanyChange {
            modules: true,
            plan: true,
            user_me: true,
            divisions: false,
        },
    )
    .await;

    Ok(FreeTierOutcome::Activated {
        new_plan: new_tier.name.clone(),
    })
}

async fn write_subscription(
    tx: &mut sqlx::PgConnection,
    subscription_id: i32,
    tier_id: i32,
    now: NaiveDateTime,
    end_date: NaiveDateTime,
    guard: Option<FreeTierGuard>,
) -> sqlx::Result<u64> {
    let mut qb =
        sqlx::QueryBuilder::<sqlx::Postgres>::new(r#"UPDATE subscriptions SET "pricingTierId" = "#);
    qb.push_bind(tier_id)
        .push(r#", status = 'ACTIVE'::"SubscriptionStatus", "billingSource" = 'PAYPAL'::"SubscriptionBillingSource""#)
        .push(r#", "paypalSubscriptionId" = NULL, "paypalPlanId" = NULL, "paypalStatus" = NULL"#)
        .push(r#", "pendingPricingTierId" = NULL, "pendingPaypalSubscriptionId" = NULL, "pendingPaypalPlanId" = NULL"#)
        .push(r#", "autoRenew" = false, "nextInvoiceDate" = NULL, "pricePerUser" = 0, "billingInterval" = 'MONTH'"#)
        .push(r#", "startDate" = "#)
        .push_bind(now)
        .push(r#", "endDate" = "#)
        .push_bind(end_date)
        .push(" WHERE id = ")
        .push_bind(subscription_id);
    if let Some(g) = guard {
        qb.push(r#" AND "billingSource" = "#)
            .push_bind(g.billing_source)
            .push(r#"::"SubscriptionBillingSource" AND status = "#)
            .push_bind(g.status.as_str())
            .push(r#"::"SubscriptionStatus""#);
    }
    Ok(qb.build().execute(&mut *tx).await?.rows_affected())
}

async fn cancel_unpaid_invoices(
    tx: &mut sqlx::PgConnection,
    subscription_id: i32,
) -> sqlx::Result<()> {
    sqlx::query(
        r#"UPDATE invoices SET status = 'CANCELLED'::"InvoiceStatus"
            WHERE "subscriptionId" = $1 AND status = 'UNPAID'::"InvoiceStatus""#,
    )
    .bind(subscription_id)
    .execute(&mut *tx)
    .await?;
    Ok(())
}

async fn write_company(
    tx: &mut sqlx::PgConnection,
    audit: &AuditHandle,
    company_id: i32,
    t: &FreeTier,
    now: NaiveDateTime,
) -> ApiResult<()> {
    let sets = vec![
        ("isTrial".to_owned(), Bind::Bool(false)),
        ("maxUsers".to_owned(), Bind::I32(t.max_users)),
        ("maxProject".to_owned(), Bind::I32(t.max_project)),
        (
            "screenshotResolution".to_owned(),
            Bind::Str(t.screenshot_resolution.clone()),
        ),
        (
            "screenshotRetentionDays".to_owned(),
            Bind::I32(t.screenshot_retention_days),
        ),
        (
            "reportExportsPerMonth".to_owned(),
            Bind::I32(t.report_exports_per_month),
        ),
        ("reportExportsUsedThisMonth".to_owned(), Bind::I32(0)),
        ("reportExportsResetAt".to_owned(), Bind::Ts(now)),
        (
            "dataHistoryMonths".to_owned(),
            Bind::I32(t.data_history_months),
        ),
        (
            "budgetAlertDeliveryChannel".to_owned(),
            Bind::Channel(t.budget_alert_delivery_channel.clone()),
        ),
        (
            "clientProofExportAllowed".to_owned(),
            Bind::Bool(t.client_proof_export_allowed),
        ),
    ];
    update_company(tx, audit, company_id, &sets).await
}

#[derive(FromRow)]
struct DowngradeRow {
    id: i32,
    #[sqlx(rename = "companyId")]
    company_id: i32,
    #[sqlx(rename = "paypalSubscriptionId")]
    paypal_subscription_id: Option<String>,
    #[sqlx(rename = "pendingPaypalSubscriptionId")]
    pending_paypal_subscription_id: Option<String>,
    status: String,
    #[sqlx(rename = "billingSource")]
    billing_source: String,
    #[sqlx(rename = "pricePerUser")]
    price_per_user: f64,
    #[sqlx(rename = "endDate")]
    end_date: Option<NaiveDateTime>,
    #[sqlx(rename = "tierName")]
    tier_name: Option<String>,
    #[sqlx(rename = "productType")]
    product_type: Option<String>,
}

/// Node: invoices.service.ts:2161 (`autoDowngradeToFree`). Dipanggil cron ekspirasi untuk
/// subscription yang `INACTIVE`: daripada menempel ke tier berbayar lama (plan-limits fail-open
/// memperlakukannya "tanpa batas"), company diturunkan ke tier Free ber-`productType` sama.
///
/// `Ok(false)` bila dilewati: subscription tak ada; bukan `PAYPAL` + `INACTIVE` (MANUAL yang habis
/// tetap INACTIVE lalu terkunci, specs/007 R-10); sudah di tier Free; tak ada tier Free untuk
/// `productType` itu; atau berubah sebelum ditulis (TOCTOU). Tidak melempar karena satu company
/// bermasalah (di Node pemanggilnya menangkap); galat DB tetap dikembalikan sebagai `Err`.
pub async fn auto_downgrade_to_free<C: PaypalSubscriptionCanceller>(
    state: &AppState,
    audit: &AuditHandle,
    paypal: &C,
    subscription_id: i32,
) -> ApiResult<bool> {
    let row: Option<DowngradeRow> = sqlx::query_as(
        r#"SELECT s.id, s."companyId", s."paypalSubscriptionId", s."pendingPaypalSubscriptionId",
                  s.status::text AS status, s."billingSource"::text AS "billingSource",
                  s."pricePerUser", s."endDate", pt.name AS "tierName",
                  pt."productType"::text AS "productType"
             FROM subscriptions s
             LEFT JOIN "pricingTiers" pt ON pt.id = s."pricingTierId"
            WHERE s.id = $1"#,
    )
    .bind(subscription_id)
    .fetch_optional(&state.db)
    .await?;
    let Some(sub) = row else {
        return Ok(false);
    };

    // specs/007 R-10 (F-6): hanya baris PayPal yang INACTIVE.
    if sub.billing_source != "PAYPAL" || sub.status != SubscriptionStatus::INACTIVE.as_str() {
        tracing::info!(
            "Lewati auto-downgrade subscription {subscription_id}: billingSource={}, status={}.",
            sub.billing_source,
            sub.status
        );
        return Ok(false);
    }

    // Sudah di tier Free (productType manapun) — tidak ada yang perlu diturunkan.
    if sub
        .tier_name
        .as_deref()
        .is_some_and(|n| n.trim().eq_ignore_ascii_case("free"))
    {
        return Ok(false);
    }

    let product_type = sub.product_type.as_deref().unwrap_or("FULL");
    let Some(free_tier) = resolve_free_tier_for(&state.db, product_type).await? else {
        tracing::warn!(
            "Tidak ada tier Free untuk productType={product_type} — lewati auto-downgrade company {} (subscription {subscription_id}).",
            sub.company_id
        );
        return Ok(false);
    };

    // Syarat yang sama diulang saat menulis: baris yang dialihkan ke MANUAL (atau diaktifkan
    // lagi) setelah pembacaan di atas dilewati (TOCTOU).
    let outcome = activate_free_tier(
        state,
        audit,
        paypal,
        &FreeTierSubscription {
            id: sub.id,
            paypal_subscription_id: sub.paypal_subscription_id,
            pending_paypal_subscription_id: sub.pending_paypal_subscription_id,
            status: sub.status,
            price_per_user: sub.price_per_user,
            end_date: sub.end_date,
        },
        &free_tier,
        sub.company_id,
        Some(FreeTierGuard {
            billing_source: "PAYPAL",
            status: SubscriptionStatus::INACTIVE,
        }),
    )
    .await?;
    if outcome == FreeTierOutcome::Skipped {
        tracing::info!(
            "Lewati auto-downgrade subscription {subscription_id}: berubah sebelum ditulis."
        );
        return Ok(false);
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, m, day)
            .and_then(|x| x.and_hms_opt(0, 0, 0))
            .unwrap_or_default()
    }

    fn cfg(days: Option<i64>) -> Config {
        let mut c = Config::for_test();
        c.free_subscription_days = days;
        c
    }

    #[test]
    fn end_date_free_mati_dan_hidup() {
        let now = d(2026, 11, 17);
        assert_eq!(
            free_subscription_end_date(&cfg(None), now, free_forever_end_date()),
            d(2099, 12, 31)
        );
        assert_eq!(
            free_subscription_end_date(&cfg(Some(14)), now, free_forever_end_date()),
            d(2026, 12, 1)
        );
    }

    #[test]
    fn masa_free_tidak_diperpanjang() {
        let c = cfg(Some(14));
        let now = d(2026, 11, 17);
        // Belum berbayar (harga 0): endDate lama yang lebih awal dipertahankan.
        assert_eq!(
            free_tier_end_date(&c, now, 0.0, "ACTIVE", Some(d(2026, 11, 20))),
            d(2026, 11, 20)
        );
        // endDate lama 2099 → dipotong ke masa Free penuh.
        assert_eq!(
            free_tier_end_date(&c, now, 0.0, "ACTIVE", Some(d(2099, 12, 31))),
            d(2026, 12, 1)
        );
        // PENDING dengan pricePerUser terisi = belum berbayar.
        assert_eq!(
            free_tier_end_date(&c, now, 5.0, "PENDING", Some(d(2026, 11, 18))),
            d(2026, 11, 18)
        );
        // Turun dari paket berbayar (INACTIVE, pricePerUser > 0): masa Free penuh.
        assert_eq!(
            free_tier_end_date(&c, now, 5.0, "INACTIVE", Some(d(2026, 11, 1))),
            d(2026, 12, 1)
        );
        // Tanpa endDate: masa Free penuh.
        assert_eq!(
            free_tier_end_date(&c, now, 0.0, "ACTIVE", None),
            d(2026, 12, 1)
        );
        // Fitur mati: 2099.
        assert_eq!(
            free_tier_end_date(&cfg(None), now, 0.0, "ACTIVE", None),
            d(2099, 12, 31)
        );
    }

    #[test]
    fn tier_nol() {
        assert!(is_zero_priced_tier(0.0, 0.0));
        assert!(!is_zero_priced_tier(0.0, 1.5));
        assert!(!is_zero_priced_tier(10.0, 0.0));
    }

    #[test]
    fn outcome_json() {
        assert_eq!(
            FreeTierOutcome::Activated {
                new_plan: "Free".into()
            }
            .to_json(),
            json!({"success": true, "message": "Free plan activated successfully", "data": {"newPlan": "Free"}})
        );
        assert_eq!(FreeTierOutcome::Skipped.to_json()["skipped"], true);
    }
}
