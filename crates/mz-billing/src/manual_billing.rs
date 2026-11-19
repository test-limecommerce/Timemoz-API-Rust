//! Port `src/subscriptions/current-subscription.ts` → `assertNotManualBilling` (Mudah-API).

use axum::http::StatusCode;
use chrono::NaiveDateTime;
use mz_core::time::now_naive;
use mz_core::{ApiError, ApiResult};

/// Node: subscriptions/current-subscription.ts:59 (`assertNotManualBilling`). Menolak (409
/// `{code:"MANUAL_BILLING"}`) aksi billing self-serve selama langganan MANUAL berjalan.
/// Baris "terkini" = `startDate desc, id desc` TANPA filter status (`PLAN_SUBSCRIPTION_ORDER`).
///
/// Menerima pool maupun transaksi (`&PgPool` atau `&mut *tx`), seperti `SubscriptionReader` Node.
pub async fn assert_not_manual_billing<'e, E>(exec: E, company_id: i32) -> ApiResult<()>
where
    E: sqlx::PgExecutor<'e>,
{
    assert_not_manual_billing_at(exec, company_id, now_naive()).await
}

/// [`assert_not_manual_billing`] dengan `now` eksplisit (untuk uji).
pub async fn assert_not_manual_billing_at<'e, E>(
    exec: E,
    company_id: i32,
    now: NaiveDateTime,
) -> ApiResult<()>
where
    E: sqlx::PgExecutor<'e>,
{
    let row: Option<(String, String, NaiveDateTime)> = sqlx::query_as(
        r#"SELECT "billingSource"::text, status::text, "endDate"
           FROM subscriptions WHERE "companyId" = $1
           ORDER BY "startDate" DESC, id DESC LIMIT 1"#,
    )
    .bind(company_id)
    .fetch_optional(exec)
    .await?;
    if row.is_some_and(|(source, status, end)| is_manual_running(&source, &status, end, now)) {
        return Err(manual_billing_error());
    }
    Ok(())
}

/// Node: current-subscription.ts:46 (`isManualRunning`): MANUAL + ACTIVE + belum lewat `endDate`.
pub fn is_manual_running(
    billing_source: &str,
    status: &str,
    end_date: NaiveDateTime,
    now: NaiveDateTime,
) -> bool {
    billing_source == "MANUAL" && status == "ACTIVE" && end_date > now
}

/// `ConflictException({ code: 'MANUAL_BILLING', message })`.
pub fn manual_billing_error() -> ApiError {
    ApiError::coded(
        StatusCode::CONFLICT,
        "MANUAL_BILLING",
        "Your subscription is managed by the Timemoz team.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(day: u32) -> NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(2026, 11, day)
            .and_then(|d| d.and_hms_opt(0, 0, 0))
            .unwrap_or_default()
    }

    #[test]
    fn manual_running_rules() {
        assert!(is_manual_running("MANUAL", "ACTIVE", t(20), t(10)));
        assert!(!is_manual_running("MANUAL", "ACTIVE", t(10), t(10)));
        assert!(!is_manual_running("MANUAL", "INACTIVE", t(20), t(10)));
        assert!(!is_manual_running("PAYPAL", "ACTIVE", t(20), t(10)));
        assert!(!is_manual_running("MANUAL", "TRIAL", t(20), t(10)));
    }
}
