//! Pembantu bersama crate: galat ber-`code` (dipakai ulang dari `mz_products::common`), tanggal
//! polos `@db.Date`, cek kode galat, dan pembacaan `company`.

use axum::http::StatusCode;
use chrono::{NaiveDate, Utc};
use mz_core::{ApiError, ApiResult};
use serde_json::{Value, json};
use sqlx::PgExecutor;

pub use mz_products::common::{
    bad_request, coded, conflict, internal, like_pattern, ms, ms_opt, not_found, offset, page_meta,
    row_uuid, unprocessable,
};

/// `unprocessable(code, message, details)` (`details` hanya bila ada).
pub fn unprocessable_details(code: &str, message: impl Into<String>, details: Value) -> ApiError {
    ApiError::body(
        StatusCode::UNPROCESSABLE_ENTITY,
        json!({ "code": code, "message": message.into(), "details": details }),
    )
}

/// `ORDER_NOT_FOUND`.
pub fn order_not_found() -> ApiError {
    not_found("ORDER_NOT_FOUND", "Order not found")
}

/// `409 ORDER_IMMUTABLE` (§0.7).
pub fn order_immutable() -> ApiError {
    conflict("ORDER_IMMUTABLE", "Only a draft order can be edited (§0.7)")
}

/// `409 ORDER_STATE_CONFLICT` (§6.2).
pub fn state_conflict() -> ApiError {
    conflict(
        "ORDER_STATE_CONFLICT",
        "Order status has changed — reload before trying again (§6.2)",
    )
}

/// Galat `{ code }` dengan kode ini (pengganti `instanceof ConflictException && code === ..`).
pub fn has_code(e: &ApiError, code: &str) -> bool {
    matches!(e, ApiError::Body { body, .. } if body.get("code").and_then(Value::as_str) == Some(code))
}

/// `moment.utc(value, 'YYYY-MM-DD', true).isValid()`.
pub fn parse_calendar_date(value: &str) -> Option<NaiveDate> {
    if value.len() != 10 {
        return None;
    }
    NaiveDate::parse_from_str(value, "%Y-%m-%d").ok()
}

/// `assertValidCalendarDate(value, field)` → 422 `INVALID_DATE`.
pub fn assert_valid_calendar_date(value: &str, field: &str) -> ApiResult<NaiveDate> {
    parse_calendar_date(value).ok_or_else(|| {
        unprocessable(
            "INVALID_DATE",
            format!("{field} is not a valid calendar date"),
        )
    })
}

/// `YYYY-MM-DD` kolom `@db.Date`.
pub fn date_str(d: NaiveDate) -> String {
    d.format("%Y-%m-%d").to_string()
}

/// Hari ini (dan kemarin) menurut `timeZone` (`moment.tz(timeZone)`).
pub fn today_in(time_zone: &str) -> NaiveDate {
    Utc::now()
        .with_timezone(&mz_core::tz::zone_or_utc(Some(time_zone)))
        .date_naive()
}

/// §5.6/§18 `assertOrderDateInRange` — `[today-1, today]` di `company.timeZone`.
pub fn assert_order_date_in_range(order_date: &str, time_zone: &str) -> ApiResult<()> {
    let today = today_in(time_zone);
    let yesterday = today.pred_opt().unwrap_or(today);
    let (t, y) = (date_str(today), date_str(yesterday));
    if order_date != t && order_date != y {
        return Err(unprocessable(
            "ORDER_DATE_OUT_OF_RANGE",
            format!("orderDate must be {y} or {t} (company.timeZone)"),
        ));
    }
    Ok(())
}

/// `company.findUniqueOrThrow({ select: { timeZone, currency } })`.
pub async fn company_tz_currency<'e>(
    exec: impl PgExecutor<'e>,
    company_id: i32,
) -> ApiResult<(String, String)> {
    let row: Option<(String, String)> =
        sqlx::query_as(r#"SELECT "timeZone", currency FROM company WHERE id = $1"#)
            .bind(company_id)
            .fetch_optional(exec)
            .await?;
    row.ok_or_else(|| internal("No company found"))
}

/// `company.timeZone` (`findUniqueOrThrow`).
pub async fn company_time_zone<'e>(
    exec: impl PgExecutor<'e>,
    company_id: i32,
) -> ApiResult<String> {
    let row: Option<String> = sqlx::query_scalar(r#"SELECT "timeZone" FROM company WHERE id = $1"#)
        .bind(company_id)
        .fetch_optional(exec)
        .await?;
    row.ok_or_else(|| internal("No company found"))
}

/// Konstraint unik yang dilanggar (`None` = bukan unique violation).
pub fn unique_constraint(err: &sqlx::Error) -> Option<String> {
    mz_products::common::unique_constraint(err)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calendar_dates() {
        assert!(parse_calendar_date("2026-02-28").is_some());
        assert!(parse_calendar_date("2026-02-30").is_none());
        assert!(parse_calendar_date("2026-2-3").is_none());
    }

    #[test]
    fn code_check() {
        let e = conflict("ORDER_STATE_CONFLICT", "x");
        assert!(has_code(&e, "ORDER_STATE_CONFLICT"));
        assert!(!has_code(&e, "ORDER_IMMUTABLE"));
    }
}
