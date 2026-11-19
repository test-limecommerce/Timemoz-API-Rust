//! `UsersService.checkSubscription` (:2296) — `GET /users/check/subscription`.
//!
//! Dipakai aplikasi desktop sebagai BATAS PAKAI: `endDate` yang dikirim berhenti dipakai begitu
//! `today > endDate`. Untuk langganan yang dijaga kunci paket Free, `endDate` = tanggal kunci
//! (`endDate` + masa tenggang) supaya desktop berhenti bersamaan dengan API & dashboard.

use chrono::NaiveDateTime;
use mz_core::company::find_company_id;
use mz_core::config::Config;
use mz_core::enums::{SubscriptionBillingSource, SubscriptionStatus};
use mz_core::subscription_lock::{LockableSubscription, free_lock_at_for};
use mz_core::time::{JsDate, now_naive};
use mz_core::{ApiError, ApiResult, AppState};
use serde_json::{Value, json};

use crate::model::js_num;

#[derive(sqlx::FromRow)]
struct CompanyRow {
    #[sqlx(rename = "isTrial")]
    is_trial: bool,
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
    #[sqlx(rename = "dontChargeInvite")]
    dont_charge_invite: bool,
}

#[derive(sqlx::FromRow)]
struct SubscriptionRow {
    #[sqlx(rename = "autoRenew")]
    auto_renew: bool,
    uuid: String,
    #[sqlx(rename = "startDate")]
    start_date: JsDate,
    #[sqlx(rename = "endDate")]
    end_date: JsDate,
    #[sqlx(rename = "pricePerUser")]
    price_per_user: f64,
    #[sqlx(rename = "maxUsers")]
    max_users: i32,
    #[sqlx(rename = "extendUsers")]
    extend_users: i32,
    #[sqlx(rename = "pendingPricingTierId")]
    pending_pricing_tier_id: Option<i32>,
    status: String,
    #[sqlx(rename = "billingSource")]
    billing_source: String,
    tier_name: String,
    tier_uuid: String,
    #[sqlx(rename = "basePrice")]
    base_price: f64,
    #[sqlx(rename = "extraUserPrice")]
    extra_user_price: f64,
    #[sqlx(rename = "maxAppUrlPerUser")]
    max_app_url_per_user: i32,
}

/// `maxUsers = subscription.maxUsers + subscription.extendUsers; if (maxUsers === 0) maxUsers = 1`.
pub fn effective_max_users(max_users: i32, extend_users: i32) -> i32 {
    match max_users.saturating_add(extend_users) {
        0 => 1,
        n => n,
    }
}

/// `usableUntil`: tanggal kunci Free bila gerbang Free hidup DAN langganan ini dijaga gerbang itu
/// (`freeSubscriptionDays() !== null && freeSubscriptionLockAtFor(...)`), kalau tidak `endDate`.
pub fn usable_until(cfg: &Config, sub: &LockableSubscription, now: NaiveDateTime) -> JsDate {
    if cfg.free_subscription_days.is_some()
        && let Some(lock_at) = free_lock_at_for(cfg, sub, now)
    {
        return JsDate(lock_at);
    }
    sub.end_date
}

fn failure(message: &str) -> Value {
    json!({ "success": false, "message": message, "data": null })
}

/// `GET /users/check/subscription`. Semua galat (termasuk yang di Node berupa `TypeError`)
/// menjadi `200 { success: false, message: 'Cannot check subscription' }`.
pub async fn check_subscription(state: &AppState, user_id: i32) -> ApiResult<Value> {
    match check_inner(state, user_id).await {
        Ok(v) => Ok(v),
        Err(err) => {
            tracing::error!(user_id, error = %err, "checkSubscription gagal");
            Ok(json!({ "success": false, "message": "Cannot check subscription" }))
        }
    }
}

async fn check_inner(state: &AppState, user_id: i32) -> ApiResult<Value> {
    let exists: Option<i32> = sqlx::query_scalar("SELECT id FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_optional(&state.db)
        .await?;
    if exists.is_none() {
        return Ok(failure("Cannot get subscription data"));
    }

    // Company: aturan company bersama (default aktif, kalau tidak yang tertua). Node membaca
    // `userCompany[0]` tanpa urutan; tanpa keanggotaan aktif ia melempar TypeError.
    let Some(company_id) = find_company_id(&state.db, user_id).await? else {
        return Err(ApiError::internal(anyhow::anyhow!("no active company")));
    };
    let company = sqlx::query_as::<_, CompanyRow>(
        r#"SELECT "isTrial", "maxProject", "useIdleDetection", "useCheatDetection", "useReports",
                  "useTimesheetApproval", "useActivityLevel", "useIntegrations", "dontChargeInvite"
           FROM company WHERE id = $1"#,
    )
    .bind(company_id)
    .fetch_one(&state.db)
    .await?;

    // `company.subscriptions[0]` tanpa urutan/filter status di Node (paket TERTUA menurut `id`).
    let subscription = sqlx::query_as::<_, SubscriptionRow>(
        r#"SELECT s."autoRenew", s.uuid::text AS "uuid", s."startDate", s."endDate", s."pricePerUser",
                  s."maxUsers", s."extendUsers", s."pendingPricingTierId", s.status::text AS status,
                  s."billingSource"::text AS "billingSource",
                  pt.name AS tier_name, pt.uuid::text AS tier_uuid, pt."basePrice",
                  pt."extraUserPrice", pt."maxAppUrlPerUser"
           FROM subscriptions s JOIN "pricingTiers" pt ON pt.id = s."pricingTierId"
           WHERE s."companyId" = $1 ORDER BY s.id LIMIT 1"#,
    )
    .bind(company_id)
    .fetch_optional(&state.db)
    .await?;
    let Some(subscription) = subscription else {
        return Ok(failure("No active subscription"));
    };

    let total_users: i64 = sqlx::query_scalar(
        r#"SELECT COUNT(*) FROM users u
           WHERE u.status <> 0
             AND EXISTS (SELECT 1 FROM "userCompany" uc WHERE uc."userId" = u.id AND uc."companyId" = $1)"#,
    )
    .bind(company_id)
    .fetch_one(&state.db)
    .await?;

    let lockable = LockableSubscription {
        status: subscription
            .status
            .parse::<SubscriptionStatus>()
            .map_err(|()| ApiError::internal(anyhow::anyhow!("unknown subscription status")))?,
        end_date: subscription.end_date,
        price_per_user: subscription.price_per_user,
        billing_source: subscription
            .billing_source
            .parse::<SubscriptionBillingSource>()
            .map_err(|()| ApiError::internal(anyhow::anyhow!("unknown billing source")))?,
        company_is_trial: Some(company.is_trial),
        product_type: None,
    };
    let end_date = usable_until(&state.config, &lockable, now_naive());

    let pending_plan = match subscription.pending_pricing_tier_id {
        Some(tier_id) => {
            let name: Option<String> =
                sqlx::query_scalar(r#"SELECT name FROM "pricingTiers" WHERE id = $1"#)
                    .bind(tier_id)
                    .fetch_optional(&state.db)
                    .await?;
            name.map(
                |tier_name| json!({ "tierName": tier_name, "effectiveAt": subscription.end_date }),
            )
        }
        None => None,
    };

    Ok(json!({
        "success": true,
        "message": "Success",
        "data": {
            "autoRenew": subscription.auto_renew,
            "isTrial": company.is_trial,
            "tierName": subscription.tier_name,
            "startDate": subscription.start_date,
            "endDate": end_date,
            "status": subscription.status,
            "currentlyTotalUsers": total_users,
            "maxUsers": effective_max_users(subscription.max_users, subscription.extend_users),
            "subscriptionUuid": subscription.uuid,
            "pricingUuid": subscription.tier_uuid,
            "basePrice": js_num(subscription.base_price),
            "extendUserPrice": js_num(subscription.extra_user_price),
            "pendingPlan": pending_plan,
            "dontChargeInvite": company.dont_charge_invite,
            "config": {
                "maxAppUrlPerUser": subscription.max_app_url_per_user,
                "maxProject": company.max_project,
                "useIdleDetection": company.use_idle_detection,
                "useCheatDetection": company.use_cheat_detection,
                "useReports": company.use_reports,
                "useTimesheetApproval": company.use_timesheet_approval,
                "useActivityLevel": company.use_activity_level,
                "useIntegrations": company.use_integrations,
            },
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mz_core::time::parse_js_date;

    fn naive(s: &str) -> NaiveDateTime {
        parse_js_date(s).unwrap_or_default()
    }

    fn sub(price: f64, status: SubscriptionStatus, end: &str, trial: bool) -> LockableSubscription {
        LockableSubscription {
            status,
            end_date: JsDate(naive(end)),
            price_per_user: price,
            billing_source: SubscriptionBillingSource::PAYPAL,
            company_is_trial: Some(trial),
            product_type: None,
        }
    }

    fn cfg(days: Option<i64>, grace: i64) -> Config {
        let mut c = Config::for_test();
        c.free_subscription_days = days;
        c.free_subscription_grace_days = grace;
        c
    }

    #[test]
    fn max_users_never_zero() {
        assert_eq!(effective_max_users(0, 0), 1);
        assert_eq!(effective_max_users(5, 3), 8);
        assert_eq!(effective_max_users(0, 2), 2);
    }

    #[test]
    fn free_gate_off_returns_end_date() {
        let s = sub(
            0.0,
            SubscriptionStatus::ACTIVE,
            "2026-12-01T00:00:00Z",
            false,
        );
        let got = usable_until(&cfg(None, 3), &s, naive("2026-11-16T00:00:00Z"));
        assert_eq!(got, s.end_date);
    }

    #[test]
    fn free_plan_gets_grace_days_added() {
        let s = sub(
            0.0,
            SubscriptionStatus::ACTIVE,
            "2026-12-01T00:00:00Z",
            false,
        );
        let got = usable_until(&cfg(Some(30), 3), &s, naive("2026-11-16T00:00:00Z"));
        assert_eq!(got.to_js_string(), "2026-12-04T00:00:00.000Z");
    }

    #[test]
    fn running_paid_plan_keeps_end_date() {
        let s = sub(
            10.0,
            SubscriptionStatus::ACTIVE,
            "2026-12-01T00:00:00Z",
            false,
        );
        let got = usable_until(&cfg(Some(30), 3), &s, naive("2026-11-16T00:00:00Z"));
        assert_eq!(got, s.end_date);
    }

    #[test]
    fn expired_paid_plan_is_locked_at_grace_end() {
        // Berbayar yang sudah lewat endDate dan INACTIVE dianggap habis (menunggu downgrade).
        let s = sub(
            10.0,
            SubscriptionStatus::INACTIVE,
            "2026-11-01T00:00:00Z",
            false,
        );
        let got = usable_until(&cfg(Some(30), 3), &s, naive("2026-11-16T00:00:00Z"));
        assert_eq!(got.to_js_string(), "2026-11-04T00:00:00.000Z");
    }

    #[test]
    fn pending_non_trial_counts_as_expired_but_trial_does_not() {
        let non_trial = sub(
            10.0,
            SubscriptionStatus::PENDING,
            "2026-11-01T00:00:00Z",
            false,
        );
        let trial = sub(
            10.0,
            SubscriptionStatus::PENDING,
            "2026-11-01T00:00:00Z",
            true,
        );
        let now = naive("2026-11-16T00:00:00Z");
        assert_eq!(
            usable_until(&cfg(Some(30), 3), &non_trial, now).to_js_string(),
            "2026-11-04T00:00:00.000Z"
        );
        assert_eq!(usable_until(&cfg(Some(30), 3), &trial, now), trial.end_date);
    }

    #[test]
    fn zero_grace_locks_at_end_date() {
        let s = sub(
            0.0,
            SubscriptionStatus::ACTIVE,
            "2026-12-01T00:00:00Z",
            false,
        );
        let got = usable_until(&cfg(Some(30), 0), &s, naive("2026-11-16T00:00:00Z"));
        assert_eq!(got, s.end_date);
    }
}
