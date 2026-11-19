//! Port `src/store-visits/store-visit-plans.util.ts` — SATU-SATUNYA tempat tanggal jadwal
//! dikonversi (§1.4, §3.1). `storeVisitPlan.visitDate` (`DATE`) diperlakukan sebagai tanggal
//! UTC polos; tidak ada `company.timeZone` di jalur jadwal.
//!
//! Fungsi yang menerima string `YYYY-MM-DD` mengandaikan string itu sudah lolos
//! [`is_plan_date`] (seperti Node, yang memanggil `dayjs.utc(.., true)` pada nilai tervalidasi);
//! untuk string tidak sah hasilnya `None`/kosong, bukan panic.

use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, Utc};

/// `PLAN_DATE_FORMAT`.
pub const PLAN_DATE_FORMAT: &str = "%Y-%m-%d";

/// `PLAN_DATE_PATTERN` (`/^\d{4}-\d{2}-\d{2}$/`).
pub fn matches_plan_date_pattern(value: &str) -> bool {
    let b = value.as_bytes();
    b.len() == 10
        && b.iter().enumerate().all(|(i, c)| match i {
            4 | 7 => *c == b'-',
            _ => c.is_ascii_digit(),
        })
}

/// Parse ketat `YYYY-MM-DD` (`dayjs.utc(value, FORMAT, true)`).
pub fn parse_plan_date(value: &str) -> Option<NaiveDate> {
    if !matches_plan_date_pattern(value) {
        return None;
    }
    let y = value.get(0..4)?.parse::<i32>().ok()?;
    let m = value.get(5..7)?.parse::<u32>().ok()?;
    let d = value.get(8..10)?.parse::<u32>().ok()?;
    NaiveDate::from_ymd_opt(y, m, d)
}

/// `isPlanDate`: `2026-02-31` ditolak, bukan digeser.
pub fn is_plan_date(value: &str) -> bool {
    parse_plan_date(value).is_some()
}

/// `toPlanDate`: string → nilai kolom `DATE`.
pub fn to_plan_date(value: &str) -> Option<NaiveDate> {
    parse_plan_date(value)
}

/// `fromPlanDate`: nilai kolom `DATE` → `YYYY-MM-DD`.
pub fn from_plan_date(value: NaiveDate) -> String {
    value.format(PLAN_DATE_FORMAT).to_string()
}

/// `fromPlanDate(new Date(ts))` untuk momen UTC (mis. `JsDate`/`timestamp(3)`).
pub fn plan_date_of(moment: NaiveDateTime) -> String {
    from_plan_date(moment.date())
}

/// §3.1 `todayPlanDate`: hari UTC.
pub fn today_plan_date() -> String {
    from_plan_date(Utc::now().date_naive())
}

/// `planDateDiffDays(start, end)` = `end - start` (hari kalender, boleh negatif).
pub fn plan_date_diff_days(start: &str, end: &str) -> Option<i64> {
    Some((parse_plan_date(end)? - parse_plan_date(start)?).num_days())
}

/// `addPlanDays(start, days)`.
pub fn add_plan_days(start: &str, days: i64) -> Option<String> {
    Some(from_plan_date(
        parse_plan_date(start)?.checked_add_signed(Duration::days(days))?,
    ))
}

/// `planWeekday`: 0 = Minggu … 6 = Sabtu.
pub fn plan_weekday(value: &str) -> Option<u32> {
    Some(parse_plan_date(value)?.weekday().num_days_from_sunday())
}

/// §0.2 `expandPlanDateRange(start, end, weekdays?)`: satu tanggal per hari; `weekdays`
/// kosong/`None` = semua hari. Rentang terbalik → kosong.
pub fn expand_plan_date_range(start: &str, end: &str, weekdays: Option<&[i64]>) -> Vec<String> {
    let (Some(s), Some(e)) = (parse_plan_date(start), parse_plan_date(end)) else {
        return Vec::new();
    };
    let span = (e - s).num_days();
    if span < 0 {
        return Vec::new();
    }
    let allowed = weekdays.filter(|w| !w.is_empty());
    let mut out = Vec::new();
    for offset in 0..=span {
        let Some(date) = s.checked_add_signed(Duration::days(offset)) else {
            break;
        };
        let wd = i64::from(date.weekday().num_days_from_sunday());
        if allowed.is_some_and(|w| !w.contains(&wd)) {
            continue;
        }
        out.push(from_plan_date(date));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_strictness() {
        let d = to_plan_date("2026-08-20").unwrap();
        assert_eq!(
            d.and_hms_opt(0, 0, 0).unwrap().to_string(),
            "2026-08-20 00:00:00"
        );
        assert_eq!(from_plan_date(d), "2026-08-20");
        for date in ["2026-12-31", "2027-01-01", "2028-02-29"] {
            assert_eq!(from_plan_date(to_plan_date(date).unwrap()), date);
        }
        assert!(!is_plan_date("2026-02-31"));
        assert!(!is_plan_date("2026-13-01"));
        assert!(!is_plan_date("2026-02-29"));
        assert!(is_plan_date("2028-02-29"));
        assert!(is_plan_date("2026-08-20"));
        assert!(!is_plan_date("2026-8-20"));
    }

    #[test]
    fn diff_add_weekday() {
        assert_eq!(plan_date_diff_days("2026-08-01", "2026-08-31"), Some(30));
        assert_eq!(plan_date_diff_days("2026-08-20", "2026-08-20"), Some(0));
        assert_eq!(plan_date_diff_days("2026-08-21", "2026-08-20"), Some(-1));
        assert_eq!(plan_date_diff_days("2026-12-31", "2027-01-01"), Some(1));
        assert_eq!(
            add_plan_days("2026-08-31", 1).as_deref(),
            Some("2026-09-01")
        );
        assert_eq!(
            add_plan_days("2026-03-28", 1).as_deref(),
            Some("2026-03-29")
        );
        assert_eq!(plan_weekday("2026-08-16"), Some(0));
        assert_eq!(plan_weekday("2026-08-17"), Some(1));
        assert_eq!(plan_weekday("2026-08-22"), Some(6));
    }

    #[test]
    fn expand() {
        assert_eq!(
            expand_plan_date_range("2026-08-20", "2026-08-23", None),
            vec!["2026-08-20", "2026-08-21", "2026-08-22", "2026-08-23"]
        );
        assert_eq!(
            expand_plan_date_range("2026-08-20", "2026-08-20", Some(&[4])),
            vec!["2026-08-20"]
        );
        assert!(expand_plan_date_range("2026-08-20", "2026-08-21", Some(&[0])).is_empty());
        assert!(expand_plan_date_range("2026-08-21", "2026-08-20", None).is_empty());
        let sept = expand_plan_date_range("2026-09-01", "2026-09-30", Some(&[1, 2, 3, 4, 5]));
        assert_eq!(sept.len(), 22);
        assert_eq!(sept.first().map(String::as_str), Some("2026-09-01"));
        assert_eq!(sept.last().map(String::as_str), Some("2026-09-30"));
        assert_eq!(
            expand_plan_date_range("2026-08-20", "2026-08-21", Some(&[])),
            vec!["2026-08-20", "2026-08-21"]
        );
    }
}
