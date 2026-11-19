//! Pembacaan bersama method `InvoiceService`: padanan
//! `users.findFirst({ include: { userCompany: { include: { company: { include: { subscriptions:
//! { include: { pricingTier: true } } } } } } } })`.
//!
//! - `user.userCompany[0]` (Node: tanpa urutan/filter) = aturan company bersama
//!   [`mz_core::company::find_company_id`] (keanggotaan aktif, default dulu, lalu tertua).
//! - `company.subscriptions[0]` (Node: tanpa urutan/filter status) = langganan dengan `id`
//!   terkecil (preseden `mz-users`, `plan/deviations/mz-users.md` #1).

use chrono::NaiveDateTime;
use mz_core::AppState;
use sqlx::FromRow;

/// Kolom `users` yang dipakai (`username` = email, `name`).
#[derive(Debug, Clone, FromRow)]
pub struct UserRow {
    pub id: i32,
    pub username: String,
    pub name: String,
}

#[derive(Debug, Clone, FromRow)]
pub struct CompanyRow {
    pub id: i32,
    pub name: String,
    pub telp: Option<String>,
    #[sqlx(rename = "isTrial")]
    pub is_trial: bool,
}

/// Kolom skalar `pricingTiers` yang dipakai alur billing.
#[derive(Debug, Clone, FromRow)]
pub struct TierRow {
    pub id: i32,
    pub name: String,
    #[sqlx(rename = "minUsers")]
    pub min_users: i32,
    #[sqlx(rename = "maxUsers")]
    pub max_users: i32,
    #[sqlx(rename = "basePrice")]
    pub base_price: f64,
    #[sqlx(rename = "pricePerUserUsd")]
    pub price_per_user_usd: f64,
    #[sqlx(rename = "billingInterval")]
    pub billing_interval: String,
    #[sqlx(rename = "paypalPlanId")]
    pub paypal_plan_id: Option<String>,
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

impl TierRow {
    /// `tier.billingInterval || 'MONTH'`.
    pub fn interval(&self) -> &str {
        if self.billing_interval.is_empty() {
            "MONTH"
        } else {
            &self.billing_interval
        }
    }

    /// `tier.paypalPlanId` truthy.
    pub fn plan_id(&self) -> Option<&str> {
        self.paypal_plan_id.as_deref().filter(|s| !s.is_empty())
    }
}

const TIER_COLS: &str = r#"t.id, t.name, t."minUsers", t."maxUsers", t."basePrice", t."pricePerUserUsd",
    t."billingInterval", t."paypalPlanId", t."maxProject", t."screenshotResolution",
    t."screenshotRetentionDays", t."reportExportsPerMonth", t."dataHistoryMonths",
    t."budgetAlertDeliveryChannel"::text AS "budgetAlertDeliveryChannel", t."clientProofExportAllowed""#;

/// Kolom `subscriptions` yang dipakai.
#[derive(Debug, Clone, FromRow)]
pub struct SubRow {
    pub id: i32,
    #[sqlx(rename = "companyId")]
    pub company_id: i32,
    #[sqlx(rename = "pricingTierId")]
    pub pricing_tier_id: i32,
    #[sqlx(rename = "startDate")]
    pub start_date: NaiveDateTime,
    #[sqlx(rename = "endDate")]
    pub end_date: NaiveDateTime,
    pub status: String,
    pub quantity: i32,
    #[sqlx(rename = "pricePerUser")]
    pub price_per_user: f64,
    #[sqlx(rename = "paypalSubscriptionId")]
    pub paypal_subscription_id: Option<String>,
    #[sqlx(rename = "paypalPlanId")]
    pub paypal_plan_id: Option<String>,
    #[sqlx(rename = "autoRenew")]
    pub auto_renew: bool,
    #[sqlx(rename = "pendingPaypalSubscriptionId")]
    pub pending_paypal_subscription_id: Option<String>,
}

impl SubRow {
    /// `subscription.paypalSubscriptionId` truthy.
    pub fn paypal_id(&self) -> Option<&str> {
        self.paypal_subscription_id
            .as_deref()
            .filter(|s| !s.is_empty())
    }

    /// Argumen `activateFreeTier(subscription, …)`.
    pub fn free_tier_input(&self) -> mz_billing::FreeTierSubscription {
        mz_billing::FreeTierSubscription {
            id: self.id,
            paypal_subscription_id: self.paypal_subscription_id.clone(),
            pending_paypal_subscription_id: self.pending_paypal_subscription_id.clone(),
            status: self.status.clone(),
            price_per_user: self.price_per_user,
            end_date: Some(self.end_date),
        }
    }
}

const SUB_COLS: &str = r#"s.id, s."companyId", s."pricingTierId", s."startDate", s."endDate",
    s.status::text AS status, s.quantity, s."pricePerUser", s."paypalSubscriptionId", s."paypalPlanId",
    s."autoRenew", s."pendingPaypalSubscriptionId""#;

/// Langganan + `pricingTier` (relasi opsional di skema).
#[derive(Debug, Clone)]
pub struct SubWithTier {
    pub sub: SubRow,
    pub tier: Option<TierRow>,
}

/// User → company → langganan pertama (+ tier). `None` di tiap tingkat = Node `undefined`.
#[derive(Debug, Clone)]
pub struct Context {
    pub user: UserRow,
    pub company: Option<CompanyRow>,
    pub subscription: Option<SubWithTier>,
}

pub async fn user(state: &AppState, user_id: i32) -> sqlx::Result<Option<UserRow>> {
    sqlx::query_as(r#"SELECT id, username, name FROM users WHERE id = $1"#)
        .bind(user_id)
        .fetch_optional(&state.db)
        .await
}

pub async fn company(state: &AppState, company_id: i32) -> sqlx::Result<Option<CompanyRow>> {
    sqlx::query_as(r#"SELECT id, name, telp, "isTrial" FROM company WHERE id = $1"#)
        .bind(company_id)
        .fetch_optional(&state.db)
        .await
}

/// `company.subscriptions[0]` + `pricingTier`.
pub async fn first_subscription(
    state: &AppState,
    company_id: i32,
) -> sqlx::Result<Option<SubWithTier>> {
    let sub: Option<SubRow> = sqlx::query_as(&format!(
        r#"SELECT {SUB_COLS} FROM subscriptions s WHERE s."companyId" = $1 ORDER BY s.id ASC LIMIT 1"#
    ))
    .bind(company_id)
    .fetch_optional(&state.db)
    .await?;
    let Some(sub) = sub else {
        return Ok(None);
    };
    let tier = tier_by_id(state, sub.pricing_tier_id).await?;
    Ok(Some(SubWithTier { sub, tier }))
}

pub async fn tier_by_id(state: &AppState, id: i32) -> sqlx::Result<Option<TierRow>> {
    sqlx::query_as(&format!(
        r#"SELECT {TIER_COLS} FROM "pricingTiers" t WHERE t.id = $1"#
    ))
    .bind(id)
    .fetch_optional(&state.db)
    .await
}

/// `pricingTiers.findFirst({ where: { uuid, status: 1 } })` (`active_only`) atau `{ uuid }`.
pub async fn tier_by_uuid(
    state: &AppState,
    uuid: &uuid::Uuid,
    active_only: bool,
) -> sqlx::Result<Option<TierRow>> {
    let filter = if active_only { " AND t.status = 1" } else { "" };
    sqlx::query_as(&format!(
        r#"SELECT {TIER_COLS} FROM "pricingTiers" t WHERE t.uuid = $1{filter} ORDER BY t.id LIMIT 1"#
    ))
    .bind(uuid)
    .fetch_optional(&state.db)
    .await
}

/// Seluruh rantai `include` Node. `Ok(None)` = user tidak ada (`User not found`).
pub async fn context(state: &AppState, user_id: i32) -> sqlx::Result<Option<Context>> {
    let Some(user) = user(state, user_id).await? else {
        return Ok(None);
    };
    let company = match mz_core::company::find_company_id(&state.db, user_id).await? {
        Some(cid) => company(state, cid).await?,
        None => None,
    };
    let subscription = match &company {
        Some(c) => first_subscription(state, c.id).await?,
        None => None,
    };
    Ok(Some(Context {
        user,
        company,
        subscription,
    }))
}

/// `userCompany.count({ where: { companyId, status: 1 } })`.
pub async fn active_users(state: &AppState, company_id: i32) -> sqlx::Result<i64> {
    sqlx::query_scalar(
        r#"SELECT COUNT(*) FROM "userCompany" WHERE "companyId" = $1 AND status = 1"#,
    )
    .bind(company_id)
    .fetch_one(&state.db)
    .await
}

/// Semua anggota company (status apa pun) — untuk invalidasi cache kunci langganan Rust.
pub async fn member_ids(state: &AppState, company_id: i32) -> sqlx::Result<Vec<i32>> {
    sqlx::query_scalar(r#"SELECT DISTINCT "userId" FROM "userCompany" WHERE "companyId" = $1"#)
        .bind(company_id)
        .fetch_all(&state.db)
        .await
}

/// Sesudah langganan company berubah: cache in-process `subscription-lock` Rust (+ pesan
/// protokol `mz:cache-invalidate`) untuk semua anggota. Node tidak menginvalidasi (cache-nya
/// hanya menyimpan keadaan terbuka ≤ 60 dtk); di Rust perubahan langsung terlihat. Tidak gagal.
pub async fn invalidate_subscription_lock(state: &AppState, company_id: i32) {
    match member_ids(state, company_id).await {
        Ok(ids) => {
            for uid in ids {
                mz_core::subscription_lock::invalidate_user(state, uid);
            }
        }
        Err(e) => tracing::warn!(company_id, error = %e, "invalidasi subscription-lock gagal"),
    }
}
