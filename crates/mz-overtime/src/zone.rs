//! Semantik `moment-timezone` yang dipakai lembur: lookup nama zona, tanggal lokal,
//! awal hari lokal, dan `timelineInstant` (`src/tracking-timelines/timeline-instant.ts`).

use chrono::{DateTime, Duration, LocalResult, NaiveDate, NaiveDateTime, TimeZone, Timelike, Utc};
use chrono_tz::Tz;

/// `LEGACY_TZ` (`timeline-instant.ts`): baris tanpa `timezone` = jam dinding Jakarta.
pub const LEGACY_TZ: Tz = chrono_tz::Asia::Jakarta;

/// `moment.tz.zone(name)` / `tz && moment.tz.zone(tz) ? tz : 'UTC'` (`companyTimeZoneOrUtc`,
/// `localDateKey`, `resolveZone`, `localDateKeysCovered`).
pub use mz_core::tz::{find_zone, zone_or_utc};

pub fn ms_of(dt: NaiveDateTime) -> i64 {
    dt.and_utc().timestamp_millis()
}

pub fn ndt(ms: i64) -> NaiveDateTime {
    DateTime::<Utc>::from_timestamp_millis(ms)
        .unwrap_or_default()
        .naive_utc()
}

/// `moment.tz(ms, zone).format('YYYY-MM-DD')`.
pub fn local_date(ms: i64, tz: Tz) -> NaiveDate {
    tz.from_utc_datetime(&ndt(ms)).date_naive()
}

/// `localDateKey(now, timeZone)` (`work-pattern.service.ts`): zona null/tak dikenal → UTC.
pub fn local_date_key(now_ms: i64, time_zone: Option<&str>) -> NaiveDate {
    local_date(now_ms, zone_or_utc(time_zone))
}

/// Jam dinding `wall` di `tz` → epoch ms seperti `moment.tz(str, zone)`: waktu ambigu →
/// offset pertama (instant lebih awal); waktu yang tidak ada (celah DST) → maju ke menit
/// valid pertama.
pub fn wall_to_ms(wall: NaiveDateTime, tz: Tz) -> i64 {
    for minutes in 0..=180 {
        let candidate = wall + Duration::minutes(minutes);
        match tz.from_local_datetime(&candidate) {
            LocalResult::Single(d) => return d.timestamp_millis(),
            LocalResult::Ambiguous(a, b) => return a.timestamp_millis().min(b.timestamp_millis()),
            LocalResult::None => {}
        }
    }
    ms_of(wall)
}

/// Awal hari lokal `date` (`moment.tz(dateKey, zone).startOf('day')`).
pub fn local_day_start_ms(date: NaiveDate, tz: Tz) -> i64 {
    wall_to_ms(date.and_time(chrono::NaiveTime::MIN), tz)
}

/// `timelineInstant(at, utcAt, timezone)`: zona terisi & dikenal → `utcAt ?? at` sudah
/// instant; selain itu jam dinding `at` (detik penuh, `format('YYYY-MM-DDTHH:mm:ss')`)
/// dibaca sebagai Asia/Jakarta.
pub fn timeline_instant(
    at: NaiveDateTime,
    utc_at: Option<NaiveDateTime>,
    timezone: Option<&str>,
) -> i64 {
    if timezone.is_some_and(|t| !t.is_empty() && find_zone(t).is_some()) {
        return ms_of(utc_at.unwrap_or(at));
    }
    let wall = at.with_nanosecond(0).unwrap_or(at);
    wall_to_ms(wall, LEGACY_TZ)
}

/// Kunci `YYYY-MM-DD` ketat (`DATE_KEY_PATTERN` + `moment.utc(k, 'YYYY-MM-DD', true)`).
pub fn parse_date_key(key: &str) -> Option<NaiveDate> {
    if !matches_date_pattern(key) {
        return None;
    }
    NaiveDate::parse_from_str(key, "%Y-%m-%d").ok()
}

/// `/^\d{4}-\d{2}-\d{2}$/` saja (tanpa cek kalender).
pub fn matches_date_pattern(key: &str) -> bool {
    let b = key.as_bytes();
    b.len() == 10
        && b.iter().enumerate().all(|(i, c)| match i {
            4 | 7 => *c == b'-',
            _ => c.is_ascii_digit(),
        })
}

pub fn date_key(d: NaiveDate) -> String {
    d.format("%Y-%m-%d").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> NaiveDateTime {
        mz_core::time::parse_js_date(s).unwrap_or_default()
    }

    #[test]
    fn zone_lookup() {
        assert_eq!(zone_or_utc(Some("asia/jakarta")), chrono_tz::Asia::Jakarta);
        assert_eq!(zone_or_utc(Some("Mars/Olympus")), Tz::UTC);
        assert_eq!(zone_or_utc(None), Tz::UTC);
        assert_eq!(zone_or_utc(Some("")), Tz::UTC);
    }

    #[test]
    fn timeline_instant_legacy_and_utc() {
        let a = at("2026-10-20T09:00:00.500Z");
        let u = at("2026-10-20T02:00:00Z");
        assert_eq!(timeline_instant(a, Some(u), Some("Asia/Jakarta")), ms_of(u));
        assert_eq!(timeline_instant(a, None, Some("Asia/Jakarta")), ms_of(a));
        // Legacy: jam dinding Jakarta, milidetik dibuang.
        assert_eq!(
            timeline_instant(a, Some(u), None),
            ms_of(at("2026-10-20T02:00:00Z"))
        );
        assert_eq!(
            timeline_instant(a, Some(u), Some("Bukan/Zona")),
            ms_of(at("2026-10-20T02:00:00Z"))
        );
    }

    #[test]
    fn date_keys() {
        assert!(parse_date_key("2026-10-22").is_some());
        assert!(parse_date_key("2026-02-30").is_none());
        assert!(matches_date_pattern("2026-02-30"));
        assert!(!matches_date_pattern("2026/10/22"));
        assert!(!matches_date_pattern("2026-1-22"));
        assert!(parse_date_key("+202-10-22").is_none());
    }

    #[test]
    fn day_start_dst() {
        let ny = chrono_tz::America::New_York;
        let d = |s: &str| NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap_or_default();
        let len = |s: &str| {
            let day = d(s);
            local_day_start_ms(day.succ_opt().unwrap_or(day), ny) - local_day_start_ms(day, ny)
        };
        assert_eq!(len("2026-11-01"), 25 * 3_600_000);
        assert_eq!(len("2026-03-08"), 23 * 3_600_000);
    }
}
