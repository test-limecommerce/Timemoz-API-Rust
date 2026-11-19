//! Pembantu kecil bersama: semantik `Date` JavaScript (server Node ber-`TZ=UTC`), format
//! tanggal laporan, pembulatan, dan penjaga UUID kolom `@db.Uuid`.

use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, Timelike};
use mz_core::{ApiError, ApiResult};

/// `round(n) = Math.round(n * 100) / 100` (`LeavesService.round`).
pub fn round(n: f64) -> f64 {
    mz_leave::js::round2(n)
}

/// `new Date(year, monthIndex, day)` di zona proses (produksi `TZ=UTC`): bulan/hari di luar
/// rentang meluap seperti JS, tahun 0..=99 dipetakan ke 1900+tahun. `None` = di luar
/// jangkauan `chrono` (Node: Invalid Date → galat Prisma 500).
pub fn js_local_date(year: i64, month0: i64, day: i64) -> Option<NaiveDate> {
    let year = if (0..=99).contains(&year) {
        1900 + year
    } else {
        year
    };
    let y = year.checked_add(month0.div_euclid(12))?;
    let m = u32::try_from(month0.rem_euclid(12) + 1).ok()?;
    let first = NaiveDate::from_ymd_opt(i32::try_from(y).ok()?, m, 1)?;
    first.checked_add_signed(Duration::try_days(day.checked_sub(1)?)?)
}

/// `new Date(\`${year}-01-01\`)` / `-12-31` (string ISO = UTC). `None` bila tahun tidak empat
/// digit (V8: bentuk lain dibaca parser lama atau Invalid Date → Node 500).
pub fn iso_year_bounds(year: i64) -> Option<(NaiveDate, NaiveDate)> {
    if !(0..=9999).contains(&year) {
        return None;
    }
    let y = i32::try_from(year).ok()?;
    Some((
        NaiveDate::from_ymd_opt(y, 1, 1)?,
        NaiveDate::from_ymd_opt(y, 12, 31)?,
    ))
}

/// Galat 500 untuk tanggal yang tidak bisa dibuat (`Invalid Date` diteruskan ke Prisma).
pub fn invalid_date() -> ApiError {
    ApiError::internal(anyhow::anyhow!("Invalid Date"))
}

/// `pad(n)` = `String(n).padStart(2, '0')`.
fn pad(n: u32) -> String {
    format!("{n:02}")
}

/// `fmtDate(d)`: `YYYY-MM-DD` UTC.
pub fn fmt_date(d: NaiveDate) -> String {
    format!("{}-{}-{}", d.year(), pad(d.month()), pad(d.day()))
}

/// `fmtDateTime(d)`: `YYYY-MM-DD HH:mm` UTC; `null` → `-`.
pub fn fmt_date_time(d: Option<NaiveDateTime>) -> String {
    match d {
        None => "-".to_owned(),
        Some(d) => format!(
            "{} {}:{}",
            fmt_date(d.date()),
            pad(d.hour()),
            pad(d.minute())
        ),
    }
}

/// Label bulan (`MONTHS_ID`, sebenarnya singkatan bahasa Inggris).
pub const MONTHS_ID: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// `MONTHS_ID[i]` (`undefined` di luar jangkauan → `None`).
pub fn month_label(index0: i64) -> Option<&'static str> {
    usize::try_from(index0)
        .ok()
        .and_then(|i| MONTHS_ID.get(i).copied())
}

/// Prisma melempar P2023 (500) untuk string non-UUID pada filter kolom `@db.Uuid`.
pub fn require_db_uuid(raw: &str) -> ApiResult<uuid::Uuid> {
    mz_core::prisma::parse_uuid(raw).map_err(|msg| ApiError::internal(anyhow::anyhow!(msg)))
}

/// Kolom `@db.Date` sebagai `Date` JS (tengah malam UTC).
pub fn date_js(d: NaiveDate) -> mz_core::time::JsDate {
    mz_core::time::JsDate(d.and_time(chrono::NaiveTime::MIN))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Option<NaiveDate> {
        mz_leave::day_count::parse_date_key(s)
    }

    #[test]
    fn js_local_date_overflow() {
        assert_eq!(js_local_date(2026, 0, 1), d("2026-01-01"));
        // new Date(2026, 2, 0) = hari terakhir Februari.
        assert_eq!(js_local_date(2026, 2, 0), d("2026-02-28"));
        assert_eq!(js_local_date(2024, 2, 0), d("2024-02-29"));
        assert_eq!(js_local_date(2026, 12, 0), d("2026-12-31"));
        assert_eq!(js_local_date(2026, -1, 1), d("2025-12-01"));
        assert_eq!(js_local_date(2026, 13, 1), d("2027-02-01"));
        assert_eq!(js_local_date(26, 0, 1), d("1926-01-01"));
    }

    #[test]
    fn year_bounds_and_formats() {
        assert_eq!(
            iso_year_bounds(2026),
            Some((
                d("2026-01-01").unwrap_or_default(),
                d("2026-12-31").unwrap_or_default()
            ))
        );
        assert_eq!(iso_year_bounds(12345), None);
        let at = mz_core::time::parse_js_date("2026-03-04T05:06:59.999Z").unwrap_or_default();
        assert_eq!(fmt_date_time(Some(at)), "2026-03-04 05:06");
        assert_eq!(fmt_date_time(None), "-");
        assert_eq!(month_label(11), Some("Dec"));
        assert_eq!(month_label(12), None);
    }

    #[test]
    fn uuid_guard() {
        assert!(require_db_uuid("9c9dd5b0-645e-499d-9c3e-d6cfc2f4ac55").is_ok());
        assert!(require_db_uuid("balance").is_err());
    }
}
