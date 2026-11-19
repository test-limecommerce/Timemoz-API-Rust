//! Aritmetika `Date` JS yang dipakai cron billing Node, dalam UTC (zona proses produksi Node = UTC).
//! Semua nilai `NaiveDateTime` = kolom `timestamp(3)` Prisma (UTC).

use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime};

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// `new Date(d.getTime() + ms)`.
pub fn add_ms(d: NaiveDateTime, ms: i64) -> NaiveDateTime {
    d.checked_add_signed(Duration::milliseconds(ms))
        .unwrap_or(d)
}

/// `d.setDate(d.getDate() + n)` (UTC: tanpa DST, = tambah n × 24 jam).
pub fn add_days(d: NaiveDateTime, n: i64) -> NaiveDateTime {
    add_ms(d, n * DAY_MS)
}

fn days_in_month(year: i32, month: u32) -> u32 {
    let (ny, nm) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    NaiveDate::from_ymd_opt(ny, nm, 1)
        .and_then(|d| d.pred_opt())
        .map_or(28, |d| d.day())
}

fn first_of(year: i32, month0: i64) -> NaiveDateTime {
    // month0: bulan berbasis 0 yang boleh melewati 11 (dinormalisasi seperti JS).
    let y = i64::from(year) + month0.div_euclid(12);
    let m = month0.rem_euclid(12) + 1;
    let date = i32::try_from(y)
        .ok()
        .and_then(|y| NaiveDate::from_ymd_opt(y, u32::try_from(m).unwrap_or(1), 1))
        .unwrap_or_default();
    date.and_time(NaiveTime::MIN)
}

/// `const n = new Date(d); n.setMonth(n.getMonth() + 1); n.setDate(1); n.setHours(0, 0, 0, 0);`
///
/// `setMonth` mempertahankan tanggal dan meluap (31 Jan → 3 Mar), baru kemudian `setDate(1)`,
/// jadi dari tanggal 31 Januari hasilnya 1 Maret, bukan 1 Februari — sama dengan Node.
pub fn first_of_next_month(d: NaiveDateTime) -> NaiveDateTime {
    let month0 = i64::from(d.month0());
    let (ny, nm) = if d.month() == 12 {
        (d.year() + 1, 1)
    } else {
        (d.year(), d.month() + 1)
    };
    if d.day() <= days_in_month(ny, nm) {
        first_of(d.year(), month0 + 1)
    } else {
        first_of(d.year(), month0 + 2)
    }
}

/// `d.setFullYear(d.getFullYear() + 1)` (29 Feb → 1 Mar).
pub fn add_one_year(d: NaiveDateTime) -> NaiveDateTime {
    let y = d.year() + 1;
    let date = NaiveDate::from_ymd_opt(y, d.month(), d.day())
        .or_else(|| NaiveDate::from_ymd_opt(y, 3, 1))
        .unwrap_or(d.date());
    date.and_time(d.time())
}

/// `formatDateStr` (tasks.service.ts:853): `YYYY-MM-DD`.
pub fn ymd(d: NaiveDateTime) -> String {
    d.format("%Y-%m-%d").to_string()
}

/// `DD-MM-YYYY` (tanggal jatuh tempo email `duedate`).
pub fn dmy(d: NaiveDateTime) -> String {
    d.format("%d-%m-%Y").to_string()
}

/// `` `${date}` `` = `Date.prototype.toString()` di proses ber-TZ UTC.
pub fn js_to_string(d: NaiveDateTime) -> String {
    d.format("%a %b %d %Y %H:%M:%S GMT+0000 (Coordinated Universal Time)")
        .to_string()
}

/// `calendarDayRange(daysAhead)` (tasks.service.ts:632): `moment().add(n,'day')` lalu
/// `startOf('day')` .. `endOf('day')` (…23:59:59.999).
pub fn calendar_day_range(now: NaiveDateTime, days_ahead: i64) -> (NaiveDateTime, NaiveDateTime) {
    let day = add_days(now, days_ahead).date();
    let start = day.and_time(NaiveTime::MIN);
    (start, add_ms(start, DAY_MS - 1))
}

/// `remainingMonthsFrom` (tasks.service.ts:847): `ceil(selisih / 30 hari)`, 0 bila ≤ 0.
pub fn remaining_months_from(from: NaiveDateTime, end: NaiveDateTime) -> i64 {
    let diff = (end - from).num_milliseconds();
    if diff <= 0 {
        return 0;
    }
    let month = 30 * DAY_MS;
    (diff + month - 1) / month
}

/// `Math.ceil((end - now) / 1 hari)`.
pub fn days_until(now: NaiveDateTime, end: NaiveDateTime) -> i64 {
    let diff = (end - now).num_milliseconds();
    diff.div_euclid(DAY_MS) + i64::from(diff.rem_euclid(DAY_MS) != 0)
}

/// `nextMonth < renewalCheck ? nextMonth : renewalCheck` dengan `renewalCheck = endDate - 3 hari`.
pub fn next_invoice_date(now: NaiveDateTime, end_date: NaiveDateTime) -> NaiveDateTime {
    let next_month = first_of_next_month(now);
    let renewal_check = add_days(end_date, -3);
    if next_month < renewal_check {
        next_month
    } else {
        renewal_check
    }
}

/// `startOf day` lokal (UTC) — `joinDate.setHours(0, 0, 0, 0)`.
pub fn start_of_day(d: NaiveDateTime) -> NaiveDateTime {
    d.date().and_time(NaiveTime::MIN)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dt(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.3f").unwrap_or_default()
    }

    #[test]
    fn next_month_overflow_like_js() {
        assert_eq!(
            first_of_next_month(dt("2026-01-31 10:00:00.000")),
            dt("2026-03-01 00:00:00.000")
        );
        assert_eq!(
            first_of_next_month(dt("2026-01-28 10:00:00.000")),
            dt("2026-02-01 00:00:00.000")
        );
        assert_eq!(
            first_of_next_month(dt("2026-12-15 10:00:00.000")),
            dt("2027-01-01 00:00:00.000")
        );
        assert_eq!(
            first_of_next_month(dt("2026-11-30 10:00:00.000")),
            dt("2026-12-01 00:00:00.000")
        );
        assert_eq!(
            first_of_next_month(dt("2026-12-31 10:00:00.000")),
            dt("2027-01-01 00:00:00.000")
        );
        assert_eq!(
            first_of_next_month(dt("2026-03-31 10:00:00.000")),
            dt("2026-05-01 00:00:00.000")
        );
    }

    #[test]
    fn year_and_days() {
        assert_eq!(
            add_one_year(dt("2028-02-29 08:00:00.000")),
            dt("2029-03-01 08:00:00.000")
        );
        assert_eq!(
            add_days(dt("2026-11-17 08:00:00.000"), 30),
            dt("2026-12-17 08:00:00.000")
        );
        assert_eq!(
            calendar_day_range(dt("2026-11-17 14:45:00.000"), 4),
            (dt("2026-11-21 00:00:00.000"), dt("2026-11-21 23:59:59.999"))
        );
    }

    #[test]
    fn months_and_days_until() {
        let a = dt("2026-01-01 00:00:00.000");
        assert_eq!(remaining_months_from(a, a), 0);
        assert_eq!(remaining_months_from(a, add_days(a, 30)), 1);
        assert_eq!(remaining_months_from(a, add_days(a, 31)), 2);
        assert_eq!(remaining_months_from(a, add_days(a, 365)), 13);
        assert_eq!(days_until(a, add_ms(a, 1)), 1);
        assert_eq!(days_until(a, add_days(a, 3)), 3);
        assert_eq!(days_until(a, add_ms(a, -1)), 0);
        assert_eq!(days_until(a, add_days(a, -2)), -2);
    }

    #[test]
    fn formats() {
        let d = dt("2026-11-03 07:05:09.000");
        assert_eq!(ymd(d), "2026-11-03");
        assert_eq!(dmy(d), "03-11-2026");
        assert_eq!(
            js_to_string(d),
            "Tue Nov 03 2026 07:05:09 GMT+0000 (Coordinated Universal Time)"
        );
    }

    #[test]
    fn next_invoice_picks_earlier() {
        let now = dt("2026-11-17 02:00:00.000");
        assert_eq!(
            next_invoice_date(now, dt("2027-06-01 00:00:00.000")),
            dt("2026-12-01 00:00:00.000")
        );
        assert_eq!(
            next_invoice_date(now, dt("2026-12-02 00:00:00.000")),
            dt("2026-11-29 00:00:00.000")
        );
    }
}
