//! Port `src/products/price-list-active.util.ts` — "aktif hari ini" menurut `company.timeZone`.

use chrono::{DateTime, NaiveDate, Utc};

/// Node: src/products/price-list-active.util.ts:15 `isPriceListActiveToday`.
/// `start_date`/`end_date` = kolom `@db.Date` (tanggal kalender UTC).
pub fn is_price_list_active_today(
    is_active: bool,
    start_date: Option<NaiveDate>,
    end_date: Option<NaiveDate>,
    time_zone: &str,
    now: DateTime<Utc>,
) -> bool {
    if !is_active {
        return false;
    }
    let today = now.with_timezone(&mz_core::tz::zone_or_utc(Some(time_zone))).date_naive();
    if start_date.is_some_and(|s| today < s) {
        return false;
    }
    if end_date.is_some_and(|e| today > e) {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    const JAKARTA: &str = "Asia/Jakarta";

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }
    fn d(s: &str) -> Option<NaiveDate> {
        Some(NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap())
    }

    // Node: src/products/price-list-active.util.spec.ts
    #[test]
    fn inactive_is_never_active() {
        assert!(!is_price_list_active_today(false, None, None, JAKARTA, at("2026-03-10T00:00:00Z")));
    }

    #[test]
    fn no_dates_always_active() {
        assert!(is_price_list_active_today(true, None, None, JAKARTA, at("2026-03-10T00:00:00Z")));
    }

    #[test]
    fn end_date_rolled_over_in_jakarta() {
        let now = at("2026-03-10T20:00:00Z");
        assert!(!is_price_list_active_today(true, None, d("2026-03-10"), JAKARTA, now));
        assert!(is_price_list_active_today(true, None, d("2026-03-10"), "UTC", now));
    }

    #[test]
    fn start_date_already_today_in_jakarta() {
        let now = at("2026-03-10T20:00:00Z");
        assert!(is_price_list_active_today(true, d("2026-03-11"), None, JAKARTA, now));
        assert!(!is_price_list_active_today(true, d("2026-03-11"), None, "UTC", now));
    }

    #[test]
    fn end_date_in_past_and_within_range() {
        let now = at("2026-03-15T12:00:00Z");
        assert!(!is_price_list_active_today(true, None, d("2026-03-10"), JAKARTA, now));
        assert!(is_price_list_active_today(true, d("2026-03-01"), d("2026-03-31"), JAKARTA, now));
    }
}
