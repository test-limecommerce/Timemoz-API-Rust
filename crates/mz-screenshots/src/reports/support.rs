//! Helper murni laporan: `report-time.ts`, `report-history.ts`, `share-report-scope.ts`, util JS.

use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, Timelike, Utc};
use mz_core::ApiError;
use mz_core::time::{JsDate, parse_js_date};

/// Rentang report terpanjang; cukup untuk preset "This/Last Year" di dashboard.
pub const MAX_REPORT_RANGE_DAYS: i64 = 366;

/// `setMonth(getMonth() + delta)` JS (zona UTC): hari yang melebihi panjang bulan meluap ke bulan berikutnya.
pub fn shift_months_js(dt: NaiveDateTime, delta: i32) -> NaiveDateTime {
    let total = i64::from(dt.year()) * 12 + i64::from(dt.month0()) + i64::from(delta);
    let year = i32::try_from(total.div_euclid(12)).unwrap_or(dt.year());
    let month0 = u32::try_from(total.rem_euclid(12)).unwrap_or(0);
    let first = NaiveDate::from_ymd_opt(year, month0 + 1, 1).unwrap_or_else(|| dt.date());
    let date = first
        .checked_add_signed(Duration::days(i64::from(dt.day()) - 1))
        .unwrap_or(first);
    date.and_time(dt.time())
}

pub const DEFAULT_DATA_HISTORY_MONTHS: i32 = 3;

/// `historyMinAllowedDate`: tanggal tertua yang boleh tampil, `None` bila tanpa batas.
pub fn history_min_allowed_date(
    data_history_months: Option<i32>,
    now: NaiveDateTime,
) -> Option<NaiveDateTime> {
    let months = data_history_months
        .filter(|m| *m != 0)
        .unwrap_or(DEFAULT_DATA_HISTORY_MONTHS);
    if months < 0 {
        return None;
    }
    Some(shift_months_js(now, -months))
}

/// `clampToHistory`: `startDate` dijepit ke batas riwayat.
pub fn clamp_to_history(
    start: NaiveDateTime,
    data_history_months: Option<i32>,
    now: NaiveDateTime,
) -> NaiveDateTime {
    match history_min_allowed_date(data_history_months, now) {
        Some(min) if start < min => min,
        _ => start,
    }
}

/// `toISOString().split('T')[0]`.
pub fn date_part(dt: NaiveDateTime) -> String {
    dt.format("%Y-%m-%d").to_string()
}

/// `assertReportRange`: 400 bila tanggal tidak valid, terbalik, atau > 366 hari.
pub fn assert_report_range(start: Option<&str>, end: Option<&str>) -> Result<(), ApiError> {
    let from = start.and_then(parse_js_date);
    let to = end.and_then(parse_js_date);
    let (Some(from), Some(to)) = (from, to) else {
        return Err(ApiError::bad_request(
            "startAt and endAt must be valid dates",
        ));
    };
    if to < from {
        return Err(ApiError::bad_request("endAt must be on or after startAt"));
    }
    let days = (to - from)
        .num_milliseconds()
        .div_euclid(24 * 60 * 60 * 1000);
    if days > MAX_REPORT_RANGE_DAYS {
        return Err(ApiError::bad_request(format!(
            "Date range cannot exceed {MAX_REPORT_RANGE_DAYS} days"
        )));
    }
    Ok(())
}

/// `toIntIds`: buang elemen yang bukan bilangan bulat positif, unik, urutan kemunculan.
pub fn to_int_ids(values: &[f64]) -> Vec<i32> {
    let mut out: Vec<i32> = Vec::new();
    for v in values {
        if v.fract() == 0.0 && *v > 0.0 && *v <= f64::from(i32::MAX) {
            #[allow(clippy::cast_possible_truncation)]
            let id = *v as i32;
            if !out.contains(&id) {
                out.push(id);
            }
        }
    }
    out
}

/// `parseIdList`: daftar id dipisah spasi dari frontend.
pub fn parse_id_list(value: Option<&str>) -> Vec<i32> {
    let nums: Vec<f64> = value
        .map(|v| {
            v.split_whitespace()
                .map(|p| p.parse::<f64>().unwrap_or(f64::NAN))
                .collect()
        })
        .unwrap_or_default();
    to_int_ids(&nums)
}

/// Billable: nilai timeline bila terisi, jika tidak mengikuti project.
pub fn timeline_is_billable(billable: Option<bool>, project_billable: bool) -> bool {
    billable.unwrap_or(project_billable)
}

/// Fragmen SQL `billableFilterWhere` (alias `tt` timeline, `p` project); `None` = tanpa filter.
pub fn billable_filter_sql(filter: Option<&str>) -> Option<&'static str> {
    match filter {
        Some("billable") => {
            Some("(tt.billable = TRUE OR (tt.billable IS NULL AND p.billable = TRUE))")
        }
        Some("non-billable") => {
            Some("(tt.billable = FALSE OR (tt.billable IS NULL AND p.billable = FALSE))")
        }
        _ => None,
    }
}

/// `shareReportStoredRange`: awal & akhir hari UTC.
pub fn share_report_stored_range(start: &str, end: &str) -> Option<(NaiveDateTime, NaiveDateTime)> {
    Some((
        parse_js_date(&format!("{start}T00:00:00.000Z"))?,
        parse_js_date(&format!("{end}T23:59:59.999Z"))?,
    ))
}

/// `encodeURIComponent` untuk path share.
fn enc(s: &str) -> String {
    mz_files::encode::encode_uri_component(s)
}

/// `shareReportScreenshotPath`.
pub fn share_report_screenshot_path(share_uuid: &str, screenshot_uuid: &str) -> String {
    format!(
        "/reports/share/{}/screenshots/{}",
        enc(share_uuid),
        enc(screenshot_uuid)
    )
}

/// `Date.getTime()` dari `NaiveDateTime` UTC.
pub fn millis(dt: NaiveDateTime) -> i64 {
    dt.and_utc().timestamp_millis()
}

/// `toISOString()`.
pub fn iso(dt: NaiveDateTime) -> String {
    JsDate(dt).to_js_string()
}

/// Jam:menit UTC `HH:mm` dari instan.
pub fn hh_mm_utc(dt: DateTime<Utc>) -> String {
    format!("{:02}:{:02}", dt.hour(), dt.minute())
}

/// Peta berurutan-kemunculan (`Map` JS).
pub struct OrderedMap<K, V> {
    keys: Vec<K>,
    index: std::collections::HashMap<K, usize>,
    values: Vec<V>,
}

impl<K: std::hash::Hash + Eq + Clone, V> OrderedMap<K, V> {
    pub fn new() -> Self {
        Self {
            keys: Vec::new(),
            index: std::collections::HashMap::new(),
            values: Vec::new(),
        }
    }

    pub fn entry_or_insert_with(&mut self, key: K, make: impl FnOnce() -> V) -> &mut V {
        let idx = match self.index.get(&key) {
            Some(i) => *i,
            None => {
                self.keys.push(key.clone());
                self.values.push(make());
                let i = self.values.len() - 1;
                self.index.insert(key, i);
                i
            }
        };
        // `idx` selalu valid: dibuat atau ditemukan di atas.
        #[allow(clippy::indexing_slicing)]
        &mut self.values[idx]
    }

    pub fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        let i = *self.index.get(key)?;
        self.values.get_mut(i)
    }

    pub fn contains_key(&self, key: &K) -> bool {
        self.index.contains_key(key)
    }

    pub fn values(&self) -> impl Iterator<Item = &V> {
        self.values.iter()
    }

    pub fn into_values(self) -> Vec<V> {
        self.values
    }

    pub fn keys(&self) -> impl Iterator<Item = &K> {
        self.keys.iter()
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

impl<K: std::hash::Hash + Eq + Clone, V> Default for OrderedMap<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mz_core::time::timeline_duration_seconds;

    fn dt(s: &str) -> NaiveDateTime {
        parse_js_date(s).unwrap_or_default()
    }

    #[test]
    fn month_shift_overflows_like_js() {
        // 31 Mar - 1 bulan = "31 Feb" = 3 Mar
        assert_eq!(
            shift_months_js(dt("2026-03-31T10:00:00Z"), -1),
            dt("2026-03-03T10:00:00Z")
        );
        assert_eq!(
            shift_months_js(dt("2026-11-16T08:00:00Z"), -3),
            dt("2026-08-16T08:00:00Z")
        );
        assert_eq!(
            shift_months_js(dt("2026-01-15T00:00:00Z"), -2),
            dt("2025-11-15T00:00:00Z")
        );
        assert_eq!(
            shift_months_js(dt("2026-12-10T00:00:00Z"), 1),
            dt("2027-01-10T00:00:00Z")
        );
    }

    #[test]
    fn history_rules() {
        let now = dt("2026-11-16T08:00:00Z");
        assert_eq!(history_min_allowed_date(Some(-1), now), None);
        assert_eq!(
            history_min_allowed_date(Some(0), now),
            Some(dt("2026-08-16T08:00:00Z"))
        );
        assert_eq!(
            history_min_allowed_date(None, now),
            Some(dt("2026-08-16T08:00:00Z"))
        );
        assert_eq!(
            history_min_allowed_date(Some(12), now),
            Some(dt("2025-11-16T08:00:00Z"))
        );
        assert_eq!(
            clamp_to_history(dt("2026-01-01T00:00:00Z"), Some(3), now),
            dt("2026-08-16T08:00:00Z")
        );
        assert_eq!(
            clamp_to_history(dt("2026-10-01T00:00:00Z"), Some(3), now),
            dt("2026-10-01T00:00:00Z")
        );
        assert_eq!(
            clamp_to_history(dt("2020-01-01T00:00:00Z"), Some(-1), now),
            dt("2020-01-01T00:00:00Z")
        );
    }

    #[test]
    fn range_assertions() {
        assert!(assert_report_range(Some("2026-01-01"), Some("2026-12-31")).is_ok());
        assert!(assert_report_range(Some("2026-01-01"), Some("2027-01-03")).is_err());
        assert!(assert_report_range(Some("2026-01-01"), Some("2027-01-02")).is_ok());
        assert!(assert_report_range(Some("2026-01-01"), Some("2027-01-01")).is_ok());
        assert!(assert_report_range(Some("2026-02-01"), Some("2026-01-01")).is_err());
        assert!(assert_report_range(Some("x"), Some("2026-01-01")).is_err());
        assert!(assert_report_range(None, None).is_err());
    }

    #[test]
    fn id_lists() {
        assert_eq!(parse_id_list(Some("3  1 3 x -2 0 1.5 7")), vec![3, 1, 7]);
        assert_eq!(parse_id_list(None), Vec::<i32>::new());
        assert_eq!(to_int_ids(&[2.0, f64::NAN, 2.0, 0.0]), vec![2]);
    }

    #[test]
    fn durations_and_billable() {
        let a = JsDate(dt("2026-01-01T00:00:00Z"));
        let b = JsDate(dt("2026-01-01T01:00:00Z"));
        assert_eq!(timeline_duration_seconds(a, b, None, None), 3600);
        assert_eq!(timeline_duration_seconds(b, a, None, None), 0);
        assert_eq!(timeline_duration_seconds(a, a, Some(a), Some(b)), 3600);
        assert!(timeline_is_billable(None, true));
        assert!(!timeline_is_billable(Some(false), true));
        assert!(billable_filter_sql(Some("all")).is_none());
        assert!(billable_filter_sql(Some("billable")).is_some());
    }

    #[test]
    fn share_paths_and_ranges() {
        assert_eq!(
            share_report_screenshot_path("a", "b"),
            "/reports/share/a/screenshots/b"
        );
        let (s, e) = share_report_stored_range("2026-07-01", "2026-07-31").unwrap_or_default();
        assert_eq!(iso(s), "2026-07-01T00:00:00.000Z");
        assert_eq!(iso(e), "2026-07-31T23:59:59.999Z");
    }

    #[test]
    fn ordered_map_keeps_insertion_order() {
        let mut m: OrderedMap<&str, i32> = OrderedMap::new();
        *m.entry_or_insert_with("b", || 0) += 1;
        *m.entry_or_insert_with("a", || 0) += 2;
        *m.entry_or_insert_with("b", || 0) += 3;
        assert_eq!(m.values().copied().collect::<Vec<_>>(), vec![4, 2]);
        assert_eq!(m.keys().copied().collect::<Vec<_>>(), vec!["b", "a"]);
        assert!(m.contains_key(&"a"));
        assert_eq!(m.len(), 2);
    }
}
