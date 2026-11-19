//! Port `src/subscription-lock/*` — 402 `SUBSCRIPTION_EXPIRED` untuk company
//! yang masa Free / langganan MANUAL-nya habis dan lewat masa tenggang.
//!
//! Hanya keadaan TERBUKA yang di-cache, paling lama sampai pergantian tahap
//! berikutnya (`endDate` / `lockAt`). Keadaan terkunci selalu dibaca ulang.

use std::time::Duration;

use axum::http::StatusCode;
use chrono::NaiveDateTime;
use serde::Serialize;
use serde_json::json;

use crate::cache::{EpochCache, Keep, shared_err};
use crate::config::Config;
use crate::enums::{PricingProductType, SubscriptionBillingSource, SubscriptionStatus};
use crate::time::{JsDate, now_naive};
use crate::{ApiError, ApiResult, AppState};

const UNLOCKED_CACHE_TTL: Duration = Duration::from_secs(60);
const DAY_MS: i64 = 24 * 60 * 60 * 1000;

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscriptionLockState {
    pub locked: bool,
    pub free_ends_at: Option<JsDate>,
    pub lock_at: Option<JsDate>,
    pub in_grace_period: bool,
    pub product_type: Option<PricingProductType>,
    pub billing_source: Option<SubscriptionBillingSource>,
    pub manual_ends_at: Option<JsDate>,
}

pub type SubscriptionLockCache = EpochCache<i32, (SubscriptionLockState, Duration)>;

pub fn new_cache() -> SubscriptionLockCache {
    EpochCache::new(100_000)
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct LockableSubscription {
    pub status: SubscriptionStatus,
    #[sqlx(rename = "endDate")]
    pub end_date: JsDate,
    #[sqlx(rename = "pricePerUser")]
    pub price_per_user: f64,
    #[sqlx(rename = "billingSource")]
    pub billing_source: SubscriptionBillingSource,
    #[sqlx(rename = "isTrial")]
    pub company_is_trial: Option<bool>,
    #[sqlx(rename = "productType")]
    pub product_type: Option<PricingProductType>,
}

fn add_days(d: NaiveDateTime, days: i64) -> NaiveDateTime {
    d + chrono::Duration::milliseconds(days * DAY_MS)
}

pub fn free_lock_at(cfg: &Config, end: NaiveDateTime) -> NaiveDateTime {
    add_days(end, cfg.free_subscription_grace_days)
}

pub fn manual_lock_at(cfg: &Config, end: NaiveDateTime) -> NaiveDateTime {
    add_days(end, cfg.manual_subscription_grace_days)
}

/// `isFreeSubscriptionExpired`.
pub fn is_free_expired(sub: &LockableSubscription, now: NaiveDateTime) -> bool {
    if sub.end_date.0 > now {
        return false;
    }
    if sub.price_per_user <= 0.0 {
        return true;
    }
    if sub.status == SubscriptionStatus::INACTIVE {
        return true;
    }
    sub.status == SubscriptionStatus::PENDING && !sub.company_is_trial.unwrap_or(false)
}

pub fn is_free_locked(cfg: &Config, sub: &LockableSubscription, now: NaiveDateTime) -> bool {
    is_free_expired(sub, add_days(now, -cfg.free_subscription_grace_days))
}

/// `freeSubscriptionLockAtFor`.
pub fn free_lock_at_for(
    cfg: &Config,
    sub: &LockableSubscription,
    now: NaiveDateTime,
) -> Option<NaiveDateTime> {
    if sub.price_per_user > 0.0 && !is_free_expired(sub, now) {
        return None;
    }
    Some(free_lock_at(cfg, sub.end_date.0))
}

pub fn is_manual_expired(sub: &LockableSubscription, now: NaiveDateTime) -> bool {
    if sub.billing_source != SubscriptionBillingSource::MANUAL || sub.end_date.0 > now {
        return false;
    }
    sub.status != SubscriptionStatus::ACTIVE && sub.status != SubscriptionStatus::TRIAL
}

pub fn is_manual_locked(cfg: &Config, sub: &LockableSubscription, now: NaiveDateTime) -> bool {
    is_manual_expired(sub, add_days(now, -cfg.manual_subscription_grace_days))
}

/// `isManualRunning` (`src/subscriptions/current-subscription.ts`).
pub fn is_manual_running(sub: &LockableSubscription, now: NaiveDateTime) -> bool {
    sub.billing_source == SubscriptionBillingSource::MANUAL
        && sub.status == SubscriptionStatus::ACTIVE
        && sub.end_date.0 > now
}

/// Langganan terkini company (`PLAN_SUBSCRIPTION_ORDER`: startDate desc, id desc).
pub async fn current_subscription(
    db: &sqlx::PgPool,
    company_id: i32,
) -> sqlx::Result<Option<LockableSubscription>> {
    sqlx::query_as::<_, LockableSubscription>(
        r#"SELECT s.status::text AS status, s."endDate", s."pricePerUser",
                  s."billingSource"::text AS "billingSource",
                  c."isTrial", pt."productType"::text AS "productType"
           FROM subscriptions s
           LEFT JOIN company c ON c.id = s."companyId"
           LEFT JOIN "pricingTiers" pt ON pt.id = s."pricingTierId"
           WHERE s."companyId" = $1
           ORDER BY s."startDate" DESC, s.id DESC
           LIMIT 1"#,
    )
    .bind(company_id)
    .fetch_optional(db)
    .await
}

fn until(now: NaiveDateTime, candidates: [NaiveDateTime; 2]) -> Duration {
    candidates
        .into_iter()
        .find(|d| *d > now)
        .and_then(|d| (d - now).to_std().ok())
        .map_or(UNLOCKED_CACHE_TTL, |d| d.min(UNLOCKED_CACHE_TTL))
}

pub fn compute(
    cfg: &Config,
    sub: Option<&LockableSubscription>,
    now: NaiveDateTime,
) -> (SubscriptionLockState, Duration) {
    let Some(sub) = sub else {
        return (SubscriptionLockState::default(), UNLOCKED_CACHE_TTL);
    };
    let product_type = sub.product_type;

    if sub.billing_source == SubscriptionBillingSource::MANUAL {
        let expired = is_manual_expired(sub, now);
        let locked = is_manual_locked(cfg, sub, now);
        let lock_at = manual_lock_at(cfg, sub.end_date.0);
        let running = is_manual_running(sub, now);
        return (
            SubscriptionLockState {
                locked,
                free_ends_at: None,
                lock_at: expired.then_some(JsDate(lock_at)),
                in_grace_period: expired && !locked,
                product_type,
                billing_source: Some(SubscriptionBillingSource::MANUAL),
                manual_ends_at: running.then_some(sub.end_date),
            },
            until(now, [sub.end_date.0, lock_at]),
        );
    }

    if cfg.free_subscription_days.is_none() {
        return (
            SubscriptionLockState {
                billing_source: Some(sub.billing_source),
                ..Default::default()
            },
            UNLOCKED_CACHE_TTL,
        );
    }

    let expired = is_free_expired(sub, now);
    let locked = is_free_locked(cfg, sub, now);
    (
        SubscriptionLockState {
            locked,
            free_ends_at: (sub.price_per_user <= 0.0).then_some(sub.end_date),
            lock_at: free_lock_at_for(cfg, sub, now).map(JsDate),
            in_grace_period: expired && !locked,
            product_type,
            billing_source: Some(sub.billing_source),
            manual_ends_at: None,
        },
        until(now, [sub.end_date.0, free_lock_at(cfg, sub.end_date.0)]),
    )
}

async fn fetch(state: AppState, user_id: i32) -> anyhow::Result<(SubscriptionLockState, Duration)> {
    let Some(company_id) = crate::company::find_company_id(&state.db, user_id).await? else {
        return Ok((SubscriptionLockState::default(), UNLOCKED_CACHE_TTL));
    };
    let sub = current_subscription(&state.db, company_id).await?;
    Ok(compute(&state.config, sub.as_ref(), now_naive()))
}

/// `SubscriptionLockService.getState`.
pub async fn get_state(state: &AppState, user_id: i32) -> ApiResult<SubscriptionLockState> {
    let st = state.clone();
    let (s, _) = state
        .caches
        .subscription_lock
        .get_or_load(
            user_id,
            |(s, ttl)| (!s.locked).then_some(Keep::ttl(*ttl)),
            move || async move { fetch(st, user_id).await.map_err(shared_err) },
        )
        .await
        .map_err(|e| ApiError::internal(anyhow::anyhow!("{e}")))?;
    Ok(s)
}

/// Dipanggil sesudah user berganti company default.
/// Lokal + publish `subscription-lock`.
pub fn invalidate_user(state: &AppState, user_id: i32) {
    invalidate_user_local(&state.caches, user_id);
    crate::invalidation::publish(
        state,
        crate::invalidation::Message::SubscriptionLock { user_id },
    );
}

/// Hanya lokal (dipakai penerima pesan invalidasi).
pub fn invalidate_user_local(caches: &crate::state::Caches, user_id: i32) {
    caches
        .subscription_lock
        .invalidate_where(|u, _| *u == user_id);
}

pub fn locked_error(s: &SubscriptionLockState) -> ApiError {
    let manual = s.billing_source == Some(SubscriptionBillingSource::MANUAL);
    ApiError::body(
        StatusCode::PAYMENT_REQUIRED,
        json!({
            "code": "SUBSCRIPTION_EXPIRED",
            "billingSource": s.billing_source,
            "message": if manual {
                "Your subscription has ended. Contact the Timemoz team to renew."
            } else {
                "Your free plan has ended. Please upgrade your subscription to continue."
            },
            "freeEndsAt": s.free_ends_at,
            "lockAt": s.lock_at,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::parse_js_date;

    fn sub(
        end: &str,
        price: f64,
        status: SubscriptionStatus,
        src: SubscriptionBillingSource,
    ) -> LockableSubscription {
        LockableSubscription {
            status,
            end_date: JsDate(parse_js_date(end).unwrap_or_default()),
            price_per_user: price,
            billing_source: src,
            company_is_trial: Some(false),
            product_type: None,
        }
    }

    fn cfg() -> Config {
        let mut c = Config::for_test();
        c.free_subscription_days = Some(14);
        c
    }

    #[test]
    fn free_plan_grace_then_lock() {
        let s = sub(
            "2026-11-01T00:00:00Z",
            0.0,
            SubscriptionStatus::ACTIVE,
            SubscriptionBillingSource::PAYPAL,
        );
        let in_grace = parse_js_date("2026-11-02T00:00:00Z").unwrap_or_default();
        let (st, _) = compute(&cfg(), Some(&s), in_grace);
        assert!(!st.locked && st.in_grace_period);
        let after = parse_js_date("2026-11-05T00:00:00Z").unwrap_or_default();
        let (st, _) = compute(&cfg(), Some(&s), after);
        assert!(st.locked);
    }

    #[test]
    fn free_gate_off_never_locks_paypal() {
        let s = sub(
            "2020-01-01T00:00:00Z",
            0.0,
            SubscriptionStatus::ACTIVE,
            SubscriptionBillingSource::PAYPAL,
        );
        let (st, _) = compute(&Config::for_test(), Some(&s), now_naive());
        assert!(!st.locked);
    }

    #[test]
    fn manual_active_past_end_is_not_expired() {
        let s = sub(
            "2020-01-01T00:00:00Z",
            10.0,
            SubscriptionStatus::ACTIVE,
            SubscriptionBillingSource::MANUAL,
        );
        let (st, _) = compute(&Config::for_test(), Some(&s), now_naive());
        assert!(!st.locked);
        let s = sub(
            "2020-01-01T00:00:00Z",
            10.0,
            SubscriptionStatus::INACTIVE,
            SubscriptionBillingSource::MANUAL,
        );
        let (st, _) = compute(&Config::for_test(), Some(&s), now_naive());
        assert!(st.locked);
    }
}
