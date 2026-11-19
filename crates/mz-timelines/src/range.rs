//! Matematika waktu murni (tanpa DB):
//! `timeline-range.ts`, bagian murni `tracking-timeline-tx.ts`,
//! `manual-time-settings/manual-time-decision.ts` + `manual-time-quota.ts` (`periodBounds`),
//! jendela hari/pekan `remainingWorkTimeSeconds`, format durasi email/ClickUp,
//! dan semantik `moment.tz(name)` untuk nama zona.

use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;

/// `LEGACY_TIMEZONE` — baris tanpa `timezone` menyimpan jam dinding Asia/Jakarta.
pub const LEGACY_TIMEZONE: &str = "Asia/Jakarta";
/// `OVERLAP_WINDOW_MARGIN_MS` (1 hari).
pub const OVERLAP_WINDOW_MARGIN_MS: i64 = 24 * 60 * 60 * 1000;
/// `MIN_APPROVED_PIECE_MS` (`approve-manual-time.ts`).
pub const MIN_APPROVED_PIECE_MS: i64 = 60_000;
/// `FUTURE_TOLERANCE_MS`.
pub const FUTURE_TOLERANCE_MS: i64 = 5 * 60_000;
/// `AUTO_APPROVE_MAX_AGE_MS`.
pub const AUTO_APPROVE_MAX_AGE_MS: i64 = 7 * 24 * 3_600_000;
/// `MANUAL_TIME_MAX_DURATION_MS` (14 jam).
pub const MANUAL_TIME_MAX_DURATION_MS: i64 = 14 * 60 * 60 * 1000;
/// Satu hari (ms), pelebaran kasar query kuota.
pub const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// Rentang setengah-terbuka `[start, end)` dalam epoch ms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MsRange {
    pub start: i64,
    pub end: i64,
}

impl MsRange {
    pub const fn new(start: i64, end: i64) -> Self {
        Self { start, end }
    }
}

/// `addedRanges(current, next)`.
pub fn added_ranges(current: MsRange, next: MsRange) -> Vec<MsRange> {
    if next.end <= current.start || next.start >= current.end {
        return vec![next];
    }
    let mut pieces = Vec::new();
    if next.start < current.start {
        pieces.push(MsRange::new(next.start, current.start));
    }
    if next.end > current.end {
        pieces.push(MsRange::new(current.end, next.end));
    }
    pieces
}

/// `overlapsAny(ranges, other)`.
pub fn overlaps_any(ranges: &[MsRange], other: MsRange) -> bool {
    ranges
        .iter()
        .any(|r| r.start < other.end && r.end > other.start)
}

/// `ManualTrim`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManualTrim {
    Delete,
    Shrink(MsRange),
    Split { left: MsRange, right: MsRange },
}

impl ManualTrim {
    /// Bentuk JSON `{ kind, ... }` untuk log event (sama dengan objek `plan` Node).
    pub fn to_json(self) -> serde_json::Value {
        let r = |x: MsRange| serde_json::json!({ "start": x.start, "end": x.end });
        match self {
            Self::Delete => serde_json::json!({ "kind": "delete" }),
            Self::Shrink(range) => serde_json::json!({ "kind": "shrink", "range": r(range) }),
            Self::Split { left, right } => {
                serde_json::json!({ "kind": "split", "left": r(left), "right": r(right) })
            }
        }
    }
}

/// `planManualTrim(manual, live)`; `None` = tidak bertumpuk.
pub fn plan_manual_trim(manual: MsRange, live: MsRange) -> Option<ManualTrim> {
    if manual.end <= live.start || manual.start >= live.end {
        return None;
    }
    let keep_left = manual.start < live.start;
    let keep_right = manual.end > live.end;
    Some(match (keep_left, keep_right) {
        (true, true) => ManualTrim::Split {
            left: MsRange::new(manual.start, live.start),
            right: MsRange::new(live.end, manual.end),
        },
        (true, false) => ManualTrim::Shrink(MsRange::new(manual.start, live.start)),
        (false, true) => ManualTrim::Shrink(MsRange::new(live.end, manual.end)),
        (false, false) => ManualTrim::Delete,
    })
}

/// `overlapMs(a, b)`.
pub fn overlap_ms(a: MsRange, b: MsRange) -> i64 {
    (a.end.min(b.end) - a.start.max(b.start)).max(0)
}

/// `subtractRanges(range, others, minPieceMs)`: potongan yang tak terpotong
/// sama sekali (`p === range` di JS) cukup ≥ 1 ms; potongan hasil belahan
/// harus ≥ `max(1, minPieceMs)`.
pub fn subtract_ranges(range: MsRange, others: &[MsRange], min_piece_ms: i64) -> Vec<MsRange> {
    // (potongan, masih objek asli?)
    let mut pieces: Vec<(MsRange, bool)> = vec![(range, true)];
    for o in others {
        pieces = pieces
            .into_iter()
            .flat_map(|(p, untouched)| {
                if o.end <= p.start || o.start >= p.end {
                    return vec![(p, untouched)];
                }
                let mut out = Vec::new();
                if o.start > p.start {
                    out.push((MsRange::new(p.start, o.start), false));
                }
                if o.end < p.end {
                    out.push((MsRange::new(o.end, p.end), false));
                }
                out
            })
            .collect();
    }
    let mut kept: Vec<MsRange> = pieces
        .into_iter()
        .filter(|(p, untouched)| {
            let min = if *untouched { 1 } else { min_piece_ms.max(1) };
            p.end - p.start >= min
        })
        .map(|(p, _)| p)
        .collect();
    kept.sort_by_key(|p| p.start);
    kept
}

/// Epoch ms → `NaiveDateTime` UTC (kolom `timestamp(3)`).
pub fn ndt(ms: i64) -> NaiveDateTime {
    DateTime::<Utc>::from_timestamp_millis(ms)
        .unwrap_or_default()
        .naive_utc()
}

/// `NaiveDateTime` (UTC) → epoch ms.
pub fn ms_of(dt: NaiveDateTime) -> i64 {
    dt.and_utc().timestamp_millis()
}

/// `Date.prototype.toISOString()`.
pub fn iso(ms: i64) -> String {
    mz_core::time::to_js_iso(&DateTime::<Utc>::from_timestamp_millis(ms).unwrap_or_default())
}

/// Jam dinding (tanpa milidetik, seperti `format('YYYY-MM-DD HH:mm:ss')`) yang
/// dibaca sebagai waktu di `tz` → epoch ms.
fn wall_to_ms(wall: NaiveDateTime, tz: Tz) -> i64 {
    let wall = wall.with_nanosecond_zero();
    match tz.from_local_datetime(&wall) {
        chrono::LocalResult::Single(d) => d.timestamp_millis(),
        chrono::LocalResult::Ambiguous(a, _) => a.timestamp_millis(),
        // Celah DST: moment menggeser maju; cukup pakai offset satu jam kemudian.
        chrono::LocalResult::None => tz
            .from_local_datetime(&(wall + Duration::hours(1)))
            .earliest()
            .map_or_else(|| ms_of(wall), |d| d.timestamp_millis() - 3_600_000),
    }
}

trait ZeroNanos {
    fn with_nanosecond_zero(self) -> Self;
}

impl ZeroNanos for NaiveDateTime {
    fn with_nanosecond_zero(self) -> Self {
        use chrono::Timelike;
        self.with_nanosecond(0).unwrap_or(self)
    }
}

/// `timelineRangeMs(row)`: `timezone` truthy → kolom sudah UTC; selain itu jam
/// dinding Jakarta (milidetik dibuang, sama dengan `format(...ss)`).
pub fn timeline_range_ms(
    start: NaiveDateTime,
    end: NaiveDateTime,
    timezone: Option<&str>,
) -> MsRange {
    if timezone.is_some_and(|t| !t.is_empty()) {
        return MsRange::new(ms_of(start), ms_of(end));
    }
    MsRange::new(
        wall_to_ms(start, chrono_tz::Asia::Jakarta),
        wall_to_ms(end, chrono_tz::Asia::Jakarta),
    )
}

/// `moment.tz.zone(name)` / `tz && moment.tz.zone(tz) ? tz : 'UTC'`.
pub use mz_core::tz::{find_zone, zone_or_utc};

/// `moment.utc(x).tz(name)` / `moment.tz(name)`: nama falsy (tak ada / `""`)
/// mengembalikan `undefined` di JS (→ `None`, pemanggil meniru TypeError-nya);
/// nama tak dikenal tetap UTC.
pub fn moment_tz(name: Option<&str>) -> Option<Tz> {
    match name {
        None | Some("") => None,
        Some(n) => Some(find_zone(n).unwrap_or(Tz::UTC)),
    }
}

/// Tanggal lokal instant `ms` di `tz`.
pub fn local_date(ms: i64, tz: Tz) -> NaiveDate {
    tz.from_utc_datetime(&ndt(ms)).date_naive()
}

/// Tengah malam lokal `date` di `tz` (`moment.tz(dateStr, zone).startOf('day')`).
pub fn local_midnight_ms(date: NaiveDate, tz: Tz) -> i64 {
    wall_to_ms(date.and_hms_opt(0, 0, 0).unwrap_or_default(), tz)
}

/// `moment.utc(a).tz(z).isSame(moment.utc(b).tz(z), 'day')`.
pub fn same_local_day(a_ms: i64, b_ms: i64, tz: Tz) -> bool {
    local_date(a_ms, tz) == local_date(b_ms, tz)
}

/// `moment.diff(other, 'second')` / `'hours'`: dibulatkan ke arah nol.
pub fn diff_trunc(a_ms: i64, b_ms: i64, unit_ms: i64) -> i64 {
    (a_ms - b_ms) / unit_ms
}

/// Hari JS (`Date.getUTCDay`, 0 = Minggu).
pub fn js_weekday(d: NaiveDate) -> i64 {
    i64::from(d.weekday().num_days_from_sunday())
}

/// `periodBounds({ startMs, zone, period, workweekStartDay })` → `[start, end)`.
pub fn period_bounds(start_ms: i64, zone: Tz, week: bool, workweek_start_day: i32) -> MsRange {
    let day = local_date(start_ms, zone);
    if !week {
        return MsRange::new(
            local_midnight_ms(day, zone),
            local_midnight_ms(day + Duration::days(1), zone),
        );
    }
    let back = (js_weekday(day) - i64::from(workweek_start_day) + 7) % 7;
    let week_start = day - Duration::days(back);
    MsRange::new(
        local_midnight_ms(week_start, zone),
        local_midnight_ms(week_start + Duration::days(7), zone),
    )
}

/// Jendela satu sisi batas jam kerja (`remainingWorkTimeSeconds.usedSeconds`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkWindow {
    /// `tracking.takenDate` antara `[from, to]` (inklusif, tanggal).
    pub taken_from: NaiveDate,
    pub taken_to: NaiveDate,
    /// Jendela UTC hari/pekan lokal (hanya bila `zone` diberikan).
    pub local: Option<MsRange>,
}

/// Jendela harian untuk `takenDate` (UTC tengah malam).
pub fn daily_window(taken: NaiveDate, zone: Option<Tz>) -> WorkWindow {
    WorkWindow {
        taken_from: taken,
        taken_to: taken,
        local: zone.map(|z| {
            MsRange::new(
                local_midnight_ms(taken, z),
                local_midnight_ms(taken + Duration::days(1), z),
            )
        }),
    }
}

/// Jendela pekanan (`workweekStartDay`, hari dihitung dari tanggal UTC `takenDate`).
pub fn weekly_window(taken: NaiveDate, workweek_start_day: i32, zone: Option<Tz>) -> WorkWindow {
    let days_back = (js_weekday(taken) - i64::from(workweek_start_day) + 7) % 7;
    let week_start = taken - Duration::days(days_back);
    let week_end = taken + Duration::days(6 - days_back);
    WorkWindow {
        taken_from: week_start,
        taken_to: week_end,
        local: zone.map(|z| {
            MsRange::new(
                local_midnight_ms(week_start, z),
                local_midnight_ms(week_start + Duration::days(7), z),
            )
        }),
    }
}

/// Alasan entri manual time tidak disetujui otomatis.
pub mod reason {
    pub const MODE_ALL: &str = "mode-all";
    pub const NOT_ALLOWED: &str = "not-allowed";
    pub const EXCEPTION: &str = "exception";
    pub const MISSING_TIMEZONE: &str = "missing-timezone";
    pub const TOO_OLD: &str = "too-old";
    pub const BUDGET: &str = "budget";
}

/// `decideEligibility(...)`: `Ok(())` = layak, `Err(alasan)`.
pub fn decide_eligibility(
    mode: &str,
    is_allow_manual_time: bool,
    on_exception_list: bool,
    has_timezone: bool,
    start_ms: i64,
    now_ms: i64,
) -> Result<(), &'static str> {
    if mode == "ALL" {
        return Err(reason::MODE_ALL);
    }
    if !is_allow_manual_time {
        return Err(reason::NOT_ALLOWED);
    }
    if on_exception_list {
        return Err(reason::EXCEPTION);
    }
    if !has_timezone {
        return Err(reason::MISSING_TIMEZONE);
    }
    if start_ms < now_ms - AUTO_APPROVE_MAX_AGE_MS {
        return Err(reason::TOO_OLD);
    }
    Ok(())
}

/// `endsInFuture(endMs, nowMs)`.
pub fn ends_in_future(end_ms: i64, now_ms: i64) -> bool {
    end_ms > now_ms + FUTURE_TOLERANCE_MS
}

/// `formatManualTimeSeconds(totalSeconds)`.
pub fn format_manual_time_seconds(total_seconds: i64) -> String {
    let duration_minutes = total_seconds.div_euclid(60);
    let hours = duration_minutes.div_euclid(60);
    let minutes = duration_minutes % 60;
    let plural = |n: i64| if n > 1 { "s" } else { "" };
    if hours > 0 && minutes > 0 {
        return format!(
            "{hours} hour{} {minutes} minute{}",
            plural(hours),
            plural(minutes)
        );
    }
    if hours > 0 {
        return format!("{hours} hour{}", plural(hours));
    }
    format!("{minutes} minute{}", plural(minutes))
}

/// Total detik terpakai sebuah baris (`(endUtc ?? end) - (startUtc ?? start)`, ≥ 0, pecahan).
pub fn used_seconds_of(
    start: NaiveDateTime,
    end: NaiveDateTime,
    start_utc: Option<NaiveDateTime>,
    end_utc: Option<NaiveDateTime>,
) -> f64 {
    let s = ms_of(start_utc.unwrap_or(start));
    let e = ms_of(end_utc.unwrap_or(end));
    #[allow(clippy::cast_precision_loss)]
    let secs = (e - s) as f64 / 1000.0;
    secs.max(0.0)
}

/// Waktu UTC dari kolom lokal (`splitEntry`/`splitTime` `toUtc`): `utc` lebih
/// dulu, lalu `local - offsetMinutes`, terakhir `local` apa adanya.
pub fn split_to_utc(
    local: NaiveDateTime,
    utc: Option<NaiveDateTime>,
    offset_min: Option<i32>,
) -> i64 {
    if let Some(u) = utc {
        return ms_of(u);
    }
    if let Some(off) = offset_min {
        return ms_of(local) - i64::from(off) * 60_000;
    }
    ms_of(local)
}

/// `moment.utc(x).format('DD MMMM YYYY')`.
pub fn format_day_month_year(ms: i64) -> String {
    ndt(ms).format("%d %B %Y").to_string()
}

/// Tanggal `tracking.takenDate` sebagai `Date` JS (UTC tengah malam).
pub fn date_ms(d: NaiveDate) -> i64 {
    ms_of(d.and_hms_opt(0, 0, 0).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(a: i64, b: i64) -> MsRange {
        MsRange::new(a, b)
    }

    fn ms(s: &str) -> i64 {
        mz_core::time::parse_js_date(s).map_or(0, ms_of)
    }

    #[test]
    fn added_ranges_extension_and_disjoint() {
        assert_eq!(added_ranges(r(0, 10), r(0, 15)), vec![r(10, 15)]);
        assert_eq!(added_ranges(r(5, 10), r(0, 15)), vec![r(0, 5), r(10, 15)]);
        assert_eq!(added_ranges(r(0, 10), r(20, 30)), vec![r(20, 30)]);
        assert!(added_ranges(r(0, 10), r(2, 8)).is_empty());
        assert!(overlaps_any(&[r(10, 15)], r(12, 20)));
        assert!(!overlaps_any(&[r(10, 15)], r(15, 20)));
    }

    #[test]
    fn manual_trim_plans() {
        assert_eq!(plan_manual_trim(r(0, 10), r(10, 20)), None);
        assert_eq!(
            plan_manual_trim(r(0, 10), r(0, 10)),
            Some(ManualTrim::Delete)
        );
        assert_eq!(
            plan_manual_trim(r(0, 10), r(5, 20)),
            Some(ManualTrim::Shrink(r(0, 5)))
        );
        assert_eq!(
            plan_manual_trim(r(5, 30), r(0, 20)),
            Some(ManualTrim::Shrink(r(20, 30)))
        );
        assert_eq!(
            plan_manual_trim(r(0, 30), r(10, 20)),
            Some(ManualTrim::Split {
                left: r(0, 10),
                right: r(20, 30)
            })
        );
        assert_eq!(overlap_ms(r(0, 30), r(10, 20)), 10);
        assert_eq!(overlap_ms(r(0, 5), r(10, 20)), 0);
    }

    #[test]
    fn subtract_drops_short_cut_pieces_but_keeps_untouched() {
        // Tak terpotong: tetap utuh walau pendek.
        assert_eq!(subtract_ranges(r(0, 1000), &[], 60_000), vec![r(0, 1000)]);
        // Terpotong: sisa < 60 dtk dibuang.
        assert_eq!(
            subtract_ranges(r(0, 200_000), &[r(30_000, 100_000)], 60_000),
            vec![r(100_000, 200_000)]
        );
        // Merge beberapa interval & urut.
        assert_eq!(
            subtract_ranges(
                r(0, 1_000_000),
                &[r(600_000, 700_000), r(100_000, 200_000)],
                60_000
            ),
            vec![r(0, 100_000), r(200_000, 600_000), r(700_000, 1_000_000)]
        );
        assert!(subtract_ranges(r(0, 10), &[r(0, 10)], 0).is_empty());
    }

    #[test]
    fn legacy_rows_are_jakarta_wall_time_without_millis() {
        let start = mz_core::time::parse_js_date("2026-11-16T08:00:00.500Z").unwrap_or_default();
        let end = mz_core::time::parse_js_date("2026-11-16T09:00:00Z").unwrap_or_default();
        let legacy = timeline_range_ms(start, end, None);
        assert_eq!(legacy.start, ms("2026-11-16T01:00:00Z"));
        assert_eq!(legacy.end, ms("2026-11-16T02:00:00Z"));
        let empty = timeline_range_ms(start, end, Some(""));
        assert_eq!(empty, legacy);
        let utc = timeline_range_ms(start, end, Some("Asia/Jakarta"));
        assert_eq!(utc.start, ms("2026-11-16T08:00:00.500Z"));
    }

    #[test]
    fn zone_lookup_matches_moment() {
        assert_eq!(find_zone("asia/jakarta"), Some(chrono_tz::Asia::Jakarta));
        assert_eq!(find_zone("Asia_Jakarta"), Some(chrono_tz::Asia::Jakarta));
        assert_eq!(find_zone("Mars/Base"), None);
        assert_eq!(moment_tz(Some("")), None);
        assert_eq!(moment_tz(None), None);
        assert_eq!(moment_tz(Some("Mars/Base")), Some(Tz::UTC));
        assert_eq!(zone_or_utc(Some("Mars/Base")), Tz::UTC);
    }

    #[test]
    fn midnight_crossing_is_local() {
        let jkt = chrono_tz::Asia::Jakarta;
        // 16:30Z–17:30Z = 23:30–00:30 WIB.
        assert!(!same_local_day(
            ms("2026-11-16T16:30:00Z"),
            ms("2026-11-16T17:30:00Z"),
            jkt
        ));
        assert!(same_local_day(
            ms("2026-11-16T16:30:00Z"),
            ms("2026-11-16T17:30:00Z"),
            Tz::UTC
        ));
    }

    #[test]
    fn diff_truncates_toward_zero() {
        assert_eq!(
            diff_trunc(
                ms("2026-11-16T14:59:59Z"),
                ms("2026-11-16T00:00:00Z"),
                3_600_000
            ),
            14
        );
        assert_eq!(diff_trunc(1999, 0, 1000), 1);
        assert_eq!(diff_trunc(0, 1999, 1000), -1);
    }

    #[test]
    fn quota_period_bounds() {
        let jkt = chrono_tz::Asia::Jakarta;
        // 2026-11-18 (Rabu) 03:00 WIB.
        let start = ms("2026-11-17T20:00:00Z");
        let day = period_bounds(start, jkt, false, 1);
        assert_eq!(
            day,
            r(ms("2026-11-17T17:00:00Z"), ms("2026-11-18T17:00:00Z"))
        );
        // Pekan mulai Senin 16 Nov.
        let week = period_bounds(start, jkt, true, 1);
        assert_eq!(
            week,
            r(ms("2026-11-15T17:00:00Z"), ms("2026-11-22T17:00:00Z"))
        );
        // Pekan mulai Rabu = hari itu sendiri.
        let wed = period_bounds(start, jkt, true, 3);
        assert_eq!(wed.start, ms("2026-11-17T17:00:00Z"));
    }

    #[test]
    fn work_time_windows() {
        let d = NaiveDate::from_ymd_opt(2026, 11, 18).unwrap_or_default(); // Rabu
        let daily = daily_window(d, None);
        assert_eq!(
            (daily.taken_from, daily.taken_to, daily.local),
            (d, d, None)
        );
        let w = weekly_window(d, 1, Some(Tz::UTC));
        assert_eq!(
            w.taken_from,
            NaiveDate::from_ymd_opt(2026, 11, 16).unwrap_or_default()
        );
        assert_eq!(
            w.taken_to,
            NaiveDate::from_ymd_opt(2026, 11, 22).unwrap_or_default()
        );
        assert_eq!(
            w.local,
            Some(r(ms("2026-11-16T00:00:00Z"), ms("2026-11-23T00:00:00Z")))
        );
        // Pekan mulai Minggu.
        let s = weekly_window(d, 0, None);
        assert_eq!(
            s.taken_from,
            NaiveDate::from_ymd_opt(2026, 11, 15).unwrap_or_default()
        );
    }

    #[test]
    fn eligibility_order() {
        let now = ms("2026-11-16T00:00:00Z");
        assert_eq!(
            decide_eligibility("ALL", false, true, false, 0, now),
            Err(reason::MODE_ALL)
        );
        assert_eq!(
            decide_eligibility("NONE", false, true, false, 0, now),
            Err(reason::NOT_ALLOWED)
        );
        assert_eq!(
            decide_eligibility("LIMIT", true, true, false, 0, now),
            Err(reason::EXCEPTION)
        );
        assert_eq!(
            decide_eligibility("LIMIT", true, false, false, now, now),
            Err(reason::MISSING_TIMEZONE)
        );
        assert_eq!(
            decide_eligibility(
                "NONE",
                true,
                false,
                true,
                now - AUTO_APPROVE_MAX_AGE_MS - 1,
                now
            ),
            Err(reason::TOO_OLD)
        );
        assert_eq!(
            decide_eligibility("NONE", true, false, true, now, now),
            Ok(())
        );
        assert!(!ends_in_future(now + FUTURE_TOLERANCE_MS, now));
        assert!(ends_in_future(now + FUTURE_TOLERANCE_MS + 1, now));
    }

    #[test]
    fn duration_texts() {
        assert_eq!(format_manual_time_seconds(59), "0 minute");
        assert_eq!(format_manual_time_seconds(60), "1 minute");
        assert_eq!(format_manual_time_seconds(3600), "1 hour");
        assert_eq!(format_manual_time_seconds(7500), "2 hours 5 minutes");
        assert_eq!(
            format_day_month_year(ms("2026-11-05T23:00:00Z")),
            "05 November 2026"
        );
    }

    #[test]
    fn idle_and_used_seconds() {
        let a = mz_core::time::parse_js_date("2026-11-16T08:00:00Z").unwrap_or_default();
        let b = mz_core::time::parse_js_date("2026-11-16T08:00:01.500Z").unwrap_or_default();
        assert!((used_seconds_of(a, b, None, None) - 1.5).abs() < f64::EPSILON);
        // Baris terbalik tidak menyumbang negatif.
        assert!(used_seconds_of(b, a, None, None).abs() < f64::EPSILON);
        assert_eq!(
            split_to_utc(a, None, Some(-420)),
            ms("2026-11-16T15:00:00Z")
        );
        assert_eq!(split_to_utc(a, Some(b), Some(-420)), ms_of(b));
    }
}
