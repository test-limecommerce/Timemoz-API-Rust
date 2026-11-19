//! Kalkulasi murni (tanpa DB): batas periode gaji, detik kerja, jumlah bayar,
//! pembulatan identik JS. Semua tanggal "hari ini" = UTC (proses Node produksi
//! berjalan dengan `TZ=UTC`).

use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime};

use crate::jsfmt;

/// `Math.round` JS: setengah dibulatkan KE ATAS (juga untuk negatif).
pub fn js_round(x: f64) -> i64 {
    (x + 0.5).floor() as i64
}

/// `parseFloat((seconds / 3600).toFixed(digits))`.
pub fn hours(total_seconds: i64, digits: usize) -> f64 {
    jsfmt::fixed(total_seconds as f64 / 3600.0, digits)
}

/// `totalHours` pada respons: dua desimal.
pub fn hours2(total_seconds: i64) -> f64 {
    hours(total_seconds, 2)
}

/// `calculateTotalAmount`: HOURLY = jam × tarif (dua desimal), selain itu tarif tetap.
pub fn total_amount(total_seconds: i64, pay_rate: Option<f64>, rate_type: &str) -> Option<f64> {
    let rate = pay_rate?;
    if rate_type == "HOURLY" {
        Some(jsfmt::fixed(total_seconds as f64 / 3600.0 * rate, 2))
    } else {
        Some(rate)
    }
}

/// Detik satu entri timeline: `max(0, floor((endAt - startAt) / 1000))`.
pub fn duration_seconds(start: NaiveDateTime, end: NaiveDateTime) -> i64 {
    let ms = end.and_utc().timestamp_millis() - start.and_utc().timestamp_millis();
    ms.div_euclid(1000).max(0)
}

/// Senin pada pekan `d` (Minggu dihitung bagian dari pekan yang berakhir di hari itu).
fn monday_of(d: NaiveDate) -> NaiveDate {
    let dow = i64::from(d.weekday().num_days_from_sunday());
    let offset = if dow == 0 { -6 } else { 1 - dow };
    d + Duration::days(offset)
}

fn month_bounds(d: NaiveDate) -> (NaiveDate, NaiveDate) {
    let first = d.with_day(1).unwrap_or(d);
    let next_first = if d.month() == 12 {
        NaiveDate::from_ymd_opt(d.year() + 1, 1, 1)
    } else {
        NaiveDate::from_ymd_opt(d.year(), d.month() + 1, 1)
    }
    .unwrap_or(first);
    (first, next_first - Duration::days(1))
}

/// `getPeriodBounds(payPeriodType, today)` — (periodStart, periodEnd) sebagai tanggal.
/// WEEKLY: Senin..Minggu pekan ini. BIWEEKLY: 14 hari berakhir Minggu pekan ini.
/// MONTHLY (dan nilai tak dikenal): tanggal 1..akhir bulan.
pub fn period_bounds(pay_period_type: &str, today: NaiveDate) -> (NaiveDate, NaiveDate) {
    match pay_period_type {
        "WEEKLY" => {
            let start = monday_of(today);
            (start, start + Duration::days(6))
        }
        "BIWEEKLY" => {
            let sunday = monday_of(today) + Duration::days(6);
            (sunday - Duration::days(13), sunday)
        }
        _ => month_bounds(today),
    }
}

/// `toHms` di ekspor CSV.
pub fn to_hms(s: i64) -> String {
    format!(
        "{:02}:{:02}:{:02}",
        s.div_euclid(3600),
        (s % 3600).div_euclid(60),
        s % 60
    )
}

/// `formatDuration` di PDF timesheet.
pub fn format_duration(seconds: i64) -> String {
    let h = seconds.div_euclid(3600);
    let m = (seconds % 3600).div_euclid(60);
    if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m")
    }
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

fn month_name(m: u32) -> &'static str {
    MONTHS
        .get((m as usize).wrapping_sub(1))
        .copied()
        .unwrap_or("")
}

/// `dd Mon yyyy` (PDF timesheet).
pub fn format_date_short(d: NaiveDate) -> String {
    format!("{:02} {} {}", d.day(), month_name(d.month()), d.year())
}

/// `toLocaleDateString('en-GB', {day:'2-digit', month:'short', year:'numeric'})`
/// pada Node 20 (ICU 73): September ditulis `Sept`.
pub fn format_date_gb(d: NaiveDate) -> String {
    let month = if d.month() == 9 {
        "Sept"
    } else {
        month_name(d.month())
    };
    format!("{:02} {} {}", d.day(), month, d.year())
}

/// `toLocaleString('en-US', {minimumFractionDigits: 2, maximumFractionDigits: 2})`.
pub fn money2(x: f64) -> String {
    let s = jsfmt::to_fixed(x.abs(), 2);
    let (int_part, frac) = s.split_once('.').unwrap_or((s.as_str(), "00"));
    let n = int_part.len();
    let mut grouped = String::new();
    for (i, ch) in int_part.chars().enumerate() {
        if i > 0 && (n - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    let neg = x < 0.0 && (int_part != "0" || frac != "00");
    format!("{}{grouped}.{frac}", if neg { "-" } else { "" })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap_or_default()
    }

    #[test]
    fn monthly_bounds_handle_year_end_and_leap() {
        assert_eq!(
            period_bounds("MONTHLY", d(2026, 12, 15)),
            (d(2026, 12, 1), d(2026, 12, 31))
        );
        assert_eq!(
            period_bounds("MONTHLY", d(2028, 2, 10)),
            (d(2028, 2, 1), d(2028, 2, 29))
        );
        assert_eq!(
            period_bounds("UNKNOWN", d(2026, 4, 30)),
            (d(2026, 4, 1), d(2026, 4, 30))
        );
    }

    #[test]
    fn weekly_runs_monday_to_sunday() {
        // 2026-11-16 adalah Senin.
        assert_eq!(
            period_bounds("WEEKLY", d(2026, 11, 16)),
            (d(2026, 11, 16), d(2026, 11, 22))
        );
        assert_eq!(
            period_bounds("WEEKLY", d(2026, 11, 19)),
            (d(2026, 11, 16), d(2026, 11, 22))
        );
        // Minggu masih bagian pekan yang dimulai Senin sebelumnya.
        assert_eq!(
            period_bounds("WEEKLY", d(2026, 11, 22)),
            (d(2026, 11, 16), d(2026, 11, 22))
        );
        assert_eq!(
            period_bounds("WEEKLY", d(2026, 11, 23)),
            (d(2026, 11, 23), d(2026, 11, 29))
        );
    }

    #[test]
    fn biweekly_ends_on_this_sunday() {
        assert_eq!(
            period_bounds("BIWEEKLY", d(2026, 11, 18)),
            (d(2026, 11, 9), d(2026, 11, 22))
        );
        assert_eq!(
            period_bounds("BIWEEKLY", d(2026, 1, 1)),
            (d(2025, 12, 22), d(2026, 1, 4))
        );
    }

    #[test]
    fn amounts_follow_js_rounding() {
        assert_eq!(total_amount(5400, Some(100.0), "HOURLY"), Some(150.0));
        assert_eq!(total_amount(1, Some(100.0), "HOURLY"), Some(0.03));
        assert_eq!(total_amount(99999, Some(15000.0), "FIXED"), Some(15000.0));
        assert_eq!(total_amount(3600, None, "HOURLY"), None);
        assert_eq!(hours2(3599), 1.0);
        assert_eq!(hours2(1800), 0.5);
        assert_eq!(hours(3601, 4), 1.0003);
    }

    #[test]
    fn durations_never_negative() {
        let a = d(2026, 1, 1)
            .and_hms_milli_opt(8, 0, 0, 0)
            .unwrap_or_default();
        let b = d(2026, 1, 1)
            .and_hms_milli_opt(9, 0, 0, 999)
            .unwrap_or_default();
        assert_eq!(duration_seconds(a, b), 3600);
        assert_eq!(duration_seconds(b, a), 0);
    }

    #[test]
    fn formatting() {
        assert_eq!(to_hms(3725), "01:02:05");
        assert_eq!(format_duration(3725), "1h 2m");
        assert_eq!(format_duration(125), "2m");
        assert_eq!(format_date_short(d(2026, 3, 1)), "01 Mar 2026");
        assert_eq!(format_date_gb(d(2026, 9, 5)), "05 Sept 2026");
        assert_eq!(money2(3200.0), "3,200.00");
        assert_eq!(money2(1234567.891), "1,234,567.89");
        assert_eq!(money2(0.0), "0.00");
        assert_eq!(js_round(2.5), 3);
        assert_eq!(js_round(66.666), 67);
    }
}
