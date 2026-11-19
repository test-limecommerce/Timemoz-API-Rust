//! Port `src/orders/order-number.service.ts` — nomor dokumen `<prefix>-<YYYYMM>-<00001>` (§7),
//! dialokasikan saat SUBMIT di transaksi yang sama (§7.3, `INSERT … ON CONFLICT`).

use chrono::NaiveDate;
use mz_core::ApiResult;
use sqlx::PgConnection;

use crate::common::internal;

/// Node: src/orders/order-number.service.ts:20 `periodOf` — bulan kalender `orderDate`.
pub fn period_of(order_date: NaiveDate) -> String {
    order_date.format("%Y%m").to_string()
}

/// `<prefix>-<period>-<00001>`; padding tumbuh sendiri melewati 99999.
pub fn format_number(prefix: &str, period: &str, last: i32) -> String {
    format!("{prefix}-{period}-{last:05}")
}

/// Node: src/orders/order-number.service.ts:29 `next` — WAJIB di dalam transaksi submit.
pub async fn next(
    tx: &mut PgConnection,
    company_id: i32,
    period: &str,
    prefix: &str,
) -> ApiResult<String> {
    let last: Option<i32> = sqlx::query_scalar(
        r#"INSERT INTO "orderCounter" ("companyId", "period", "prefix", "lastNumber")
           VALUES ($1, $2, $3, 1)
           ON CONFLICT ("companyId", "period", "prefix")
           DO UPDATE SET "lastNumber" = "orderCounter"."lastNumber" + 1
           RETURNING "lastNumber""#,
    )
    .bind(company_id)
    .bind(period)
    .bind(prefix)
    .fetch_optional(&mut *tx)
    .await?;
    let last = last.ok_or_else(|| internal("orderCounter did not return a number (§7.3)"))?;
    Ok(format_number(prefix, period, last))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Node: src/orders/order-number.service.spec.ts
    #[test]
    fn period_and_format() {
        let d = |s| NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap();
        assert_eq!(period_of(d("2026-09-09")), "202609");
        assert_eq!(period_of(d("2026-01-01")), "202601");
        assert_eq!(format_number("ORD", "202609", 1), "ORD-202609-00001");
        assert_eq!(format_number("ORD", "202609", 100_001), "ORD-202609-100001");
    }
}
