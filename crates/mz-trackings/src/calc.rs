//! Logika murni (tanpa DB): kalkulasi waktu, paginasi tanggal, dan pembulatan
//! angka yang meniru JavaScript/moment di `trackings.service.ts`.
//!
//! Semua kalkulasi "waktu lokal" Node (`moment()`, `getHours()`, `setHours()`)
//! berjalan pada `TZ=UTC` di produksi, jadi di sini semuanya UTC.

use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, Utc};
use serde_json::{Number, Value};

/// Zona waktu data lama (timeline tanpa `timezone` disimpan sebagai waktu lokal).
pub const LEGACY_TZ: &str = "Asia/Jakarta";
const MS_PER_DAY: i64 = 86_400_000;

/// `parseInt(s, 10)`: awalan digit (dengan tanda opsional); selain itu `NaN` (`None`).
pub fn parse_int_js(s: &str) -> Option<i64> {
    let t = s.trim_start();
    let end = t
        .char_indices()
        .find(|(i, c)| !(c.is_ascii_digit() || (*i == 0 && (*c == '-' || *c == '+'))))
        .map_or(t.len(), |(i, _)| i);
    t.get(..end).and_then(|p| p.parse::<i64>().ok())
}

/// Angka JS: bilangan bulat dicetak tanpa `.0` (`100`, bukan `100.0`).
pub fn js_number(f: f64) -> Value {
    if !f.is_finite() {
        return Value::Null;
    }
    if f.fract() == 0.0 && f.abs() < 9.0e15 {
        #[allow(clippy::cast_possible_truncation)]
        return Value::from(f as i64);
    }
    Number::from_f64(f).map_or(Value::Null, Value::Number)
}

/// `parseFloat(x.toFixed(2))`. `toFixed` membulatkan nilai desimal EKSAK double
/// ke atas pada seri (half-up), berbeda dengan `{:.2}` Rust (half-even).
pub fn to_fixed2(x: f64) -> f64 {
    if !x.is_finite() {
        return x;
    }
    let neg = x < 0.0;
    let exact = format!("{:.25}", x.abs());
    let (int_part, frac) = exact.split_once('.').unwrap_or((&exact, ""));
    let digit = |i: usize| -> u64 {
        frac.as_bytes()
            .get(i)
            .map_or(0, |b| u64::from(b.saturating_sub(b'0')))
    };
    let int_val: u64 = int_part.parse().unwrap_or(0);
    let mut n = int_val * 100 + digit(0) * 10 + digit(1);
    if digit(2) >= 5 {
        n += 1;
    }
    #[allow(clippy::cast_precision_loss)]
    let v = n as f64 / 100.0;
    if neg { -v } else { v }
}

/// `Math.floor((end - start) / 1000)` untuk milidetik bulat.
pub fn floor_secs(start_ms: i64, end_ms: i64) -> i64 {
    (end_ms - start_ms).div_euclid(1000)
}

/// Jumlah durasi positif dalam detik (durasi negatif/nol dihitung 0).
pub fn total_seconds(ranges: &[(i64, i64)]) -> i64 {
    ranges.iter().map(|(s, e)| floor_secs(*s, *e).max(0)).sum()
}

pub fn naive_ms(dt: NaiveDateTime) -> i64 {
    dt.and_utc().timestamp_millis()
}

/// `moment.tz.zone(name)` (tanpa peka huruf, `/` setara `_`).
pub use mz_core::tz::find_zone as find_tz;

/// `toUtcMs` di `findAll`: `startAt` bertimezone = sudah UTC; timezone null =
/// jam dinding Asia/Jakarta yang disimpan apa adanya.
pub fn legacy_aware_utc_ms(start_at: NaiveDateTime, timezone: Option<&str>) -> i64 {
    if timezone.is_some_and(|t| !t.is_empty()) {
        return naive_ms(start_at);
    }
    use chrono::TimeZone;
    find_tz(LEGACY_TZ)
        .and_then(|tz| tz.from_local_datetime(&start_at).earliest())
        .map_or_else(|| naive_ms(start_at), |d| d.timestamp_millis())
}

/// Tanggal "hari ini" menurut zona (`moment.tz(zone).format('YYYY-MM-DD')`).
pub fn today_in_tz(now: DateTime<Utc>, tz_name: &str) -> NaiveDate {
    match find_tz(tz_name) {
        Some(tz) => now.with_timezone(&tz).date_naive(),
        None => now.date_naive(),
    }
}

/// Aturan `takenDate` final di `TrackingService.create`.
pub fn final_taken_date(
    from_given: bool,
    taken: NaiveDateTime,
    tz_name: &str,
    now: DateTime<Utc>,
) -> NaiveDate {
    if from_given {
        return taken.date();
    }
    let today = today_in_tz(now, tz_name);
    // `startFromUser.isAfter(dayjs.utc(todayJakartaStr))`: perbandingan INSTAN
    // terhadap tengah malam UTC hari itu.
    let today_midnight = today.and_hms_opt(0, 0, 0);
    if today_midnight.is_some_and(|m| taken > m) {
        taken.date()
    } else {
        today
    }
}

/// `m.month(n).year(y)` lalu rentang bulan. `None` = tanggal moment tak valid.
/// Hasil: (hari pertama, hari terakhir).
pub fn month_range(
    now: NaiveDate,
    month: Option<&str>,
    year: Option<&str>,
) -> Option<(NaiveDate, NaiveDate)> {
    let mut y = i64::from(now.year());
    let mut m0 = i64::from(now.month0());
    if let Some(raw) = month {
        let n = parse_int_js(raw)?;
        // moment `.month(n)` menggulung bulan di luar 0..11 ke tahun berikutnya.
        let total = y.checked_mul(12)?.checked_add(n)?;
        y = total.div_euclid(12);
        m0 = total.rem_euclid(12);
    }
    if let Some(raw) = year {
        y = parse_int_js(raw)?;
    }
    let y = i32::try_from(y).ok()?;
    let m1 = u32::try_from(m0).ok()?.checked_add(1)?;
    let first = NaiveDate::from_ymd_opt(y, m1, 1)?;
    let (ny, nm) = if m1 == 12 {
        (y.checked_add(1)?, 1)
    } else {
        (y, m1 + 1)
    };
    let last = NaiveDate::from_ymd_opt(ny, nm, 1)?.pred_opt()?;
    Some((first, last))
}

/// Semua tanggal dari `first` sampai `last` (inklusif).
pub fn dates_between(first: NaiveDate, last: NaiveDate) -> Vec<NaiveDate> {
    let mut out = Vec::new();
    let mut cur = first;
    while cur <= last {
        out.push(cur);
        match cur.succ_opt() {
            Some(n) => cur = n,
            None => break,
        }
    }
    out
}

/// Minggu moment locale `en`: Minggu..Sabtu yang memuat `d`.
pub fn week_dates(d: NaiveDate) -> Vec<NaiveDate> {
    let back = i64::from(d.weekday().num_days_from_sunday());
    let start = d - Duration::days(back);
    dates_between(start, start + Duration::days(6))
}

/// `{index, percent, leftPercent}` per timeline hari itu + `{drawPercent}` di akhir.
/// `timelines` = (startAt, endAt) dalam ms, terurut `startAt asc`.
pub fn hour_count(day: NaiveDate, timelines: &[(i64, i64)]) -> Vec<Value> {
    let day_start = day.and_hms_opt(0, 0, 0).map_or(0, naive_ms);
    let day_end = day_start + MS_PER_DAY - 1;
    let filtered: Vec<&(i64, i64)> = timelines
        .iter()
        .filter(|(s, e)| *s >= day_start && *e <= day_end)
        .collect();

    let mut out = Vec::new();
    let last = filtered.len().saturating_sub(1);
    for (i, (start, end)) in filtered.iter().map(|t| **t).enumerate() {
        let default_jam = start.div_euclid(MS_PER_DAY) * MS_PER_DAY;
        let duration = floor_secs(start, end);
        // `startdate.getHours() === 0 ? 0 : ...` (jam 00 selalu 0, termasuk 00:30).
        let hour = (start - default_jam) / 3_600_000;
        let left = if hour == 0 {
            0
        } else {
            floor_secs(default_jam, start)
        };
        #[allow(clippy::cast_precision_loss)]
        let left_percent = (left as f64 / 86_400.0) * 100.0;
        #[allow(clippy::cast_precision_loss)]
        let percent = (duration as f64 / 86_400.0) * 100.0;
        out.push(serde_json::json!({
            "index": i,
            "percent": js_number(percent),
            "leftPercent": js_number(left_percent),
        }));
        if i == last {
            #[allow(clippy::cast_precision_loss)]
            let draw = (end - default_jam) as f64 / 1000.0;
            out.push(serde_json::json!({ "drawPercent": js_number((draw / 86_400.0) * 100.0) }));
        }
    }
    out
}

/// Persentase harian bulanan: `parseFloat(((s / 28800) * 100).toFixed(2))`.
pub fn day_percentage(total_second: i64) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    to_fixed2((total_second as f64 / 28_800.0) * 100.0)
}

/// Hasil `getWorkLimitData` bagian hitung (setelah baris batas & detik hari lain dibaca).
#[derive(Debug, PartialEq, Eq)]
pub struct WorkLimitCalc {
    pub daily_limit_sec: Option<i32>,
    pub daily_worked_sec: i64,
    pub daily_overtime_sec: i64,
    pub weekly_limit_sec: Option<i32>,
    pub weekly_worked_sec: i64,
    pub weekly_overtime_sec: i64,
}

/// Senin pada pekan yang memuat `d` (`daysFromMonday`).
pub fn week_start_monday(d: NaiveDate) -> NaiveDate {
    d - Duration::days(i64::from(d.weekday().num_days_from_monday()))
}

pub fn work_limit_calc(
    daily_limit: Option<i32>,
    weekly_limit: Option<i32>,
    daily_worked: i64,
    other_days_sec: i64,
) -> Option<WorkLimitCalc> {
    if daily_limit.is_none() && weekly_limit.is_none() {
        return None;
    }
    let daily_overtime = daily_limit.map_or(0, |l| (daily_worked - i64::from(l)).max(0));
    let weekly_worked = other_days_sec + daily_worked;
    let weekly_overtime = weekly_limit.map_or(0, |l| (weekly_worked - i64::from(l)).max(0));
    Some(WorkLimitCalc {
        daily_limit_sec: daily_limit,
        daily_worked_sec: daily_worked,
        daily_overtime_sec: daily_overtime,
        weekly_limit_sec: weekly_limit,
        weekly_worked_sec: weekly_worked,
        weekly_overtime_sec: weekly_overtime,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap_or_default()
    }

    fn ms(y: i32, m: u32, day: u32, h: u32, mi: u32, s: u32) -> i64 {
        d(y, m, day).and_hms_opt(h, mi, s).map_or(0, naive_ms)
    }

    #[test]
    fn parse_int_like_js() {
        assert_eq!(parse_int_js("12abc"), Some(12));
        assert_eq!(parse_int_js("-3"), Some(-3));
        assert_eq!(parse_int_js(""), None);
        assert_eq!(parse_int_js("abc"), None);
    }

    #[test]
    fn to_fixed2_rounds_half_up_on_exact_ties() {
        assert!((to_fixed2(0.125) - 0.13).abs() < 1e-12);
        assert!((to_fixed2(12.344) - 12.34).abs() < 1e-12);
        assert!((to_fixed2(100.0) - 100.0).abs() < 1e-12);
        assert_eq!(js_number(to_fixed2(100.0)), Value::from(100));
        assert_eq!(js_number(0.5), serde_json::json!(0.5));
    }

    #[test]
    fn day_percentage_of_full_day() {
        assert_eq!(js_number(day_percentage(28_800)), Value::from(100));
        assert_eq!(js_number(day_percentage(3_600)), serde_json::json!(12.5));
    }

    #[test]
    fn durations_clamp_negative() {
        assert_eq!(total_seconds(&[(0, 5_999), (10_000, 5_000)]), 5);
        assert_eq!(floor_secs(0, 1_999), 1);
    }

    #[test]
    fn legacy_timeline_is_shifted_from_jakarta() {
        let start = d(2026, 11, 16).and_hms_opt(8, 0, 0).unwrap_or_default();
        assert_eq!(legacy_aware_utc_ms(start, Some("UTC")), naive_ms(start));
        assert_eq!(
            legacy_aware_utc_ms(start, None),
            naive_ms(start) - 7 * 3_600_000
        );
    }

    #[test]
    fn month_range_wraps_like_moment() {
        let now = d(2026, 11, 16);
        assert_eq!(
            month_range(now, None, None),
            Some((d(2026, 11, 1), d(2026, 11, 30)))
        );
        // month index 0-based seperti moment
        assert_eq!(
            month_range(now, Some("1"), Some("2024")),
            Some((d(2024, 2, 1), d(2024, 2, 29)))
        );
        assert_eq!(
            month_range(now, Some("12"), None),
            Some((d(2027, 1, 1), d(2027, 1, 31)))
        );
        assert_eq!(
            month_range(now, Some("-1"), None),
            Some((d(2025, 12, 1), d(2025, 12, 31)))
        );
        assert_eq!(month_range(now, Some("x"), None), None);
    }

    #[test]
    fn week_runs_sunday_to_saturday() {
        let w = week_dates(d(2026, 11, 16)); // Senin
        assert_eq!(w.first(), Some(&d(2026, 11, 15)));
        assert_eq!(w.last(), Some(&d(2026, 11, 21)));
        assert_eq!(w.len(), 7);
        assert_eq!(week_start_monday(d(2026, 11, 15)), d(2026, 11, 9)); // Minggu -> Senin lalu
    }

    #[test]
    fn hour_count_matches_node_quirks() {
        let day = d(2026, 11, 16);
        let t = [
            (ms(2026, 11, 16, 0, 30, 0), ms(2026, 11, 16, 1, 30, 0)),
            (ms(2026, 11, 16, 12, 0, 0), ms(2026, 11, 16, 18, 0, 0)),
            (ms(2026, 11, 15, 23, 0, 0), ms(2026, 11, 16, 1, 0, 0)), // lintas hari: dibuang
        ];
        let out = hour_count(day, &t);
        assert_eq!(out.len(), 3);
        // jam 00 -> left = 0 walau menit 30
        assert_eq!(
            out.first().and_then(|v| v.get("leftPercent")),
            Some(&Value::from(0))
        );
        assert_eq!(
            out.first().and_then(|v| v.get("index")),
            Some(&Value::from(0))
        );
        assert_eq!(
            out.get(1).and_then(|v| v.get("leftPercent")),
            Some(&Value::from(50))
        );
        assert_eq!(
            out.get(2).and_then(|v| v.get("drawPercent")),
            Some(&Value::from(75))
        );
    }

    #[test]
    fn work_limit_math() {
        assert_eq!(work_limit_calc(None, None, 100, 0), None);
        let c = work_limit_calc(Some(28_800), Some(144_000), 30_000, 120_000);
        assert_eq!(
            c,
            Some(WorkLimitCalc {
                daily_limit_sec: Some(28_800),
                daily_worked_sec: 30_000,
                daily_overtime_sec: 1_200,
                weekly_limit_sec: Some(144_000),
                weekly_worked_sec: 150_000,
                weekly_overtime_sec: 6_000,
            })
        );
    }

    #[test]
    fn final_taken_date_rules() {
        let now = DateTime::<Utc>::from_timestamp(1_794_000_000, 0).unwrap_or_default(); // 2026-11-06T..Z
        let today = now.date_naive();
        let past = today
            .pred_opt()
            .unwrap_or(today)
            .and_hms_opt(5, 0, 0)
            .unwrap_or_default();
        let future = (today + Duration::days(3))
            .and_hms_opt(5, 0, 0)
            .unwrap_or_default();
        assert_eq!(final_taken_date(false, past, "UTC", now), today);
        assert_eq!(final_taken_date(false, future, "UTC", now), future.date());
        assert_eq!(final_taken_date(true, past, "UTC", now), past.date());
        // zona jauh di depan UTC bisa menghasilkan "hari ini" yang berbeda
        assert!(final_taken_date(false, past, "Pacific/Kiritimati", now) >= today);
    }
}
