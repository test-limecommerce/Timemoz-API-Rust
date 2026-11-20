//! `src/leaves/leave-window.ts`: hari cuti di dalam jendela tanggal (laporan, analitik,
//! overview, kalender) dengan aturan hari kerja & libur yang sama dengan saldo. Murni, kecuali
//! [`load_clip_ctx`] / [`load_calendar_ctx`] yang memuat libur & pola kerja sekali per panggilan.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::{Datelike, NaiveDate, NaiveDateTime, NaiveTime};
use sqlx::PgConnection;

use crate::calendar::{OffDays, load_holiday_index};
use crate::day_count::{count_leave_days, to_date_key};
use crate::enums::LeaveDayType;

/// `LeaveCalendarCtx`: libur company + pola libur mingguan (`None` = Sabtu & Minggu).
#[derive(Debug, Clone, Default)]
pub struct CalendarCtx {
    pub excluded_dates: Arc<HashSet<NaiveDate>>,
    pub off: Option<OffDays>,
}

impl CalendarCtx {
    /// `isWeeklyOff ?? defaultWeeklyOff`.
    pub fn is_weekly_off(&self, d: NaiveDate) -> bool {
        match &self.off {
            Some(o) => o.is_off(d),
            None => crate::day_count::is_sat_sun(d),
        }
    }

    fn count(
        &self,
        start: NaiveDate,
        end: NaiveDate,
        start_day_type: LeaveDayType,
        end_day_type: LeaveDayType,
    ) -> f64 {
        let is_off = |d| self.is_weekly_off(d);
        let f: &dyn Fn(NaiveDate) -> bool = &is_off;
        // Rentang terpotong selalu ≤ rentang pengajuan (≤ 366 hari, divalidasi saat diajukan).
        count_leave_days(
            &to_date_key(start),
            &to_date_key(end),
            start_day_type,
            end_day_type,
            &self.excluded_dates,
            self.off.as_ref().map(|_| f),
        )
        .map_or(0.0, |c| c.total_days)
    }
}

/// `StoredLeaveWindowRequest`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StoredLeave {
    pub start: NaiveDate,
    pub end: NaiveDate,
    pub start_day_type: LeaveDayType,
    pub end_day_type: LeaveDayType,
    pub total_days: f64,
    pub reviewed_at: Option<NaiveDateTime>,
}

impl StoredLeave {
    /// Dari kolom teks `startDayType`/`endDayType`.
    pub fn new(
        start: NaiveDate,
        end: NaiveDate,
        start_day_type: &str,
        end_day_type: &str,
        total_days: f64,
        reviewed_at: Option<NaiveDateTime>,
    ) -> Self {
        Self {
            start,
            end,
            start_day_type: start_day_type.parse().unwrap_or(LeaveDayType::FULL_DAY),
            end_day_type: end_day_type.parse().unwrap_or(LeaveDayType::FULL_DAY),
            total_days,
            reviewed_at,
        }
    }

    /// Saat pola kerja yang dipakai menagih pengajuan (`reviewedAt ?? startDate`).
    fn pattern_as_of(&self) -> NaiveDateTime {
        self.reviewed_at
            .unwrap_or_else(|| self.start.and_time(NaiveTime::MIN))
    }
}

/// `leaveDaysWithin(request, windowStartKey, windowEndKey, ctx)`: hari cuti di dalam jendela
/// inklusif. Jenis hari awal/akhir hanya berlaku bila hari awal/akhir aslinya ada di jendela.
pub fn leave_days_within(
    r: &StoredLeave,
    window_start: NaiveDate,
    window_end: NaiveDate,
    ctx: &CalendarCtx,
) -> f64 {
    let clip_start = r.start.max(window_start);
    let clip_end = r.end.min(window_end);
    if clip_start > clip_end {
        return 0.0;
    }
    let has_real_start = clip_start == r.start;
    let has_real_end = clip_end == r.end;
    let start_type = if has_real_start {
        r.start_day_type
    } else {
        LeaveDayType::FULL_DAY
    };
    let mut end_type = if has_real_end {
        r.end_day_type
    } else {
        LeaveDayType::FULL_DAY
    };
    let mut effective_start = start_type;
    // countLeaveDays hanya membaca startDayType untuk rentang sehari.
    if clip_start == clip_end {
        if !has_real_start && has_real_end {
            effective_start = end_type;
        }
        end_type = effective_start;
    }
    ctx.count(clip_start, clip_end, effective_start, end_type)
}

/// `leavePatternAsOfKey(r)`: tanggal (UTC) saat pola yang menagih pengajuan.
pub fn leave_pattern_as_of_key(r: &StoredLeave) -> NaiveDate {
    r.pattern_as_of().date()
}

/// `leaveNeedsPattern(r, fromKey, toKey)`: perlu dihitung ulang (melewati tepi jendela luar
/// atau melintasi beberapa bulan); selain itu `totalDays` tersimpan sudah tepat.
pub fn leave_needs_pattern(r: &StoredLeave, from: NaiveDate, to: NaiveDate) -> bool {
    r.start < from
        || r.end > to
        || (r.start.year(), r.start.month()) != (r.end.year(), r.end.month())
}

/// `leaveDaysWithinStored(request, windowStart, windowEnd, ctxFor)`: pengajuan yang seluruhnya
/// di dalam jendela memakai `totalDays` tersimpan (pola saat disetujui, FR-046); hanya yang
/// melewati tepi yang dipotong dengan pola per `ctxFor(request)`.
pub fn leave_days_within_stored(
    r: &StoredLeave,
    window_start: NaiveDate,
    window_end: NaiveDate,
    ctx_for: &ClipCtx,
) -> f64 {
    if r.start >= window_start && r.end <= window_end {
        return r.total_days;
    }
    leave_days_within(r, window_start, window_end, &ctx_for.ctx_for(r))
}

/// `monthRangeKeys(year, month)`: hari pertama & terakhir bulan (1-based).
pub fn month_range(year: i32, month: u32) -> Option<(NaiveDate, NaiveDate)> {
    let first = NaiveDate::from_ymd_opt(year, month, 1)?;
    let next = if month == 12 {
        NaiveDate::from_ymd_opt(year.checked_add(1)?, 1, 1)?
    } else {
        NaiveDate::from_ymd_opt(year, month + 1, 1)?
    };
    Some((first, next.pred_opt()?))
}

/// `bucketLeavesByDate(requests, year, month, ctx)`: pengajuan per tanggal bulan itu, hanya
/// tanggal kerja company (bukan libur mingguan, bukan hari libur). Tanggal kosong dilewati.
pub fn bucket_leaves_by_date<'a, T>(
    requests: &'a [T],
    first: NaiveDate,
    last: NaiveDate,
    ctx: &CalendarCtx,
    range_of: impl Fn(&T) -> (NaiveDate, NaiveDate),
) -> Vec<(NaiveDate, Vec<&'a T>)> {
    let mut out = Vec::new();
    for day in first.iter_days().take_while(|d| *d <= last) {
        if ctx.is_weekly_off(day) || ctx.excluded_dates.contains(&day) {
            continue;
        }
        let leaves: Vec<&T> = requests
            .iter()
            .filter(|r| {
                let (s, e) = range_of(r);
                s <= day && e >= day
            })
            .collect();
        if !leaves.is_empty() {
            out.push((day, leaves));
        }
    }
    out
}

/// `loadCalendarCtx(companyId, fromKey, toKey)`: libur + pola libur mingguan (per sekarang).
pub async fn load_calendar_ctx(
    conn: &mut PgConnection,
    company_id: i32,
    from: NaiveDate,
    to: NaiveDate,
    now: NaiveDateTime,
) -> sqlx::Result<CalendarCtx> {
    let holidays = load_holiday_index(&mut *conn, company_id, from, to).await?;
    let off = crate::calendar::off_day_predicate(conn, company_id, from, to, now).await?;
    Ok(CalendarCtx {
        excluded_dates: Arc::new(holidays.excluded_dates),
        off: Some(off),
    })
}

/// Hasil `loadClipCtx`: konteks kalender per tanggal persetujuan.
#[derive(Debug, Clone, Default)]
pub struct ClipCtx {
    holidays: Arc<HashSet<NaiveDate>>,
    by_day: HashMap<NaiveDate, CalendarCtx>,
}

impl ClipCtx {
    /// `ctxFor(r)`: pola per tanggal persetujuan; tanpa entri → hanya libur (pola bawaan).
    pub fn ctx_for(&self, r: &StoredLeave) -> CalendarCtx {
        self.by_day
            .get(&leave_pattern_as_of_key(r))
            .cloned()
            .unwrap_or_else(|| CalendarCtx {
                excluded_dates: Arc::clone(&self.holidays),
                off: None,
            })
    }
}

/// `loadClipCtx(companyId, fromKey, toKey, requests)`: libur dimuat sekali, baris pola kerja
/// dimuat SEKALI (hanya bila ada pengajuan yang perlu dipotong), lalu satu resolver per tanggal
/// persetujuan (pola per `reviewedAt`, selain itu `startDate`).
pub async fn load_clip_ctx(
    conn: &mut PgConnection,
    company_id: i32,
    from: NaiveDate,
    to: NaiveDate,
    requests: &[StoredLeave],
) -> sqlx::Result<ClipCtx> {
    let holidays = Arc::new(
        load_holiday_index(&mut *conn, company_id, from, to)
            .await?
            .excluded_dates,
    );
    let mut by_day: HashMap<NaiveDate, CalendarCtx> = HashMap::new();
    let mut rows = None;
    for r in requests {
        if !leave_needs_pattern(r, from, to) {
            continue;
        }
        let key = leave_pattern_as_of_key(r);
        if by_day.contains_key(&key) {
            continue;
        }
        if rows.is_none() {
            rows = Some(
                mz_overtime::hr::common::clock::load_pattern_rows(&mut *conn, company_id, to)
                    .await?,
            );
        }
        let off = OffDays::from_rows(rows.clone().unwrap_or_default(), to, r.pattern_as_of());
        by_day.insert(
            key,
            CalendarCtx {
                excluded_dates: Arc::clone(&holidays),
                off: Some(off),
            },
        );
    }
    Ok(ClipCtx { holidays, by_day })
}

#[cfg(test)]
mod tests {
    use super::*;
    use LeaveDayType::{FULL_DAY, HALF_DAY_AFTERNOON, HALF_DAY_MORNING};

    fn d(s: &str) -> NaiveDate {
        crate::day_count::parse_date_key(s).unwrap_or_default()
    }

    fn leave(s: &str, e: &str, st: LeaveDayType, et: LeaveDayType, total: f64) -> StoredLeave {
        StoredLeave {
            start: d(s),
            end: d(e),
            start_day_type: st,
            end_day_type: et,
            total_days: total,
            reviewed_at: None,
        }
    }

    // leave-window.spec.ts › leaveDaysWithin
    #[test]
    fn clips_to_month() {
        let ctx = CalendarCtx::default();
        // 2026-09-28 (Sen) .. 2026-10-02 (Jum): 3 hari Sep, 2 hari Okt.
        let r = leave(
            "2026-09-28",
            "2026-10-02",
            HALF_DAY_AFTERNOON,
            HALF_DAY_MORNING,
            4.0,
        );
        assert_eq!(
            leave_days_within(&r, d("2026-09-01"), d("2026-09-30"), &ctx),
            2.5
        );
        assert_eq!(
            leave_days_within(&r, d("2026-10-01"), d("2026-10-31"), &ctx),
            1.5
        );
        assert_eq!(
            leave_days_within(&r, d("2026-11-01"), d("2026-11-30"), &ctx),
            0.0
        );
    }

    #[test]
    fn one_day_clip_uses_real_end_portion() {
        let ctx = CalendarCtx::default();
        let r = leave("2026-09-30", "2026-10-01", FULL_DAY, HALF_DAY_MORNING, 1.5);
        assert_eq!(
            leave_days_within(&r, d("2026-10-01"), d("2026-10-31"), &ctx),
            0.5
        );
        assert_eq!(
            leave_days_within(&r, d("2026-09-01"), d("2026-09-30"), &ctx),
            1.0
        );
    }

    #[test]
    fn stored_total_inside_window() {
        let clip = ClipCtx::default();
        let r = leave("2026-10-05", "2026-10-06", FULL_DAY, FULL_DAY, 7.0);
        assert_eq!(
            leave_days_within_stored(&r, d("2026-10-01"), d("2026-10-31"), &clip),
            7.0
        );
        let cross = leave("2026-09-30", "2026-10-01", FULL_DAY, FULL_DAY, 9.0);
        assert_eq!(
            leave_days_within_stored(&cross, d("2026-10-01"), d("2026-10-31"), &clip),
            1.0
        );
    }

    #[test]
    fn needs_pattern() {
        let r = leave("2026-10-05", "2026-10-06", FULL_DAY, FULL_DAY, 2.0);
        assert!(!leave_needs_pattern(&r, d("2026-01-01"), d("2026-12-31")));
        let r = leave("2026-09-30", "2026-10-01", FULL_DAY, FULL_DAY, 2.0);
        assert!(leave_needs_pattern(&r, d("2026-01-01"), d("2026-12-31")));
        let r = leave("2025-12-31", "2025-12-31", FULL_DAY, FULL_DAY, 1.0);
        assert!(leave_needs_pattern(&r, d("2026-01-01"), d("2026-12-31")));
    }

    #[test]
    fn month_ranges() {
        assert_eq!(
            month_range(2026, 2),
            Some((d("2026-02-01"), d("2026-02-28")))
        );
        assert_eq!(
            month_range(2028, 2),
            Some((d("2028-02-01"), d("2028-02-29")))
        );
        assert_eq!(
            month_range(2026, 12),
            Some((d("2026-12-01"), d("2026-12-31")))
        );
        assert_eq!(month_range(2026, 13), None);
    }

    #[test]
    fn bucket_skips_weekends_and_holidays() {
        let ctx = CalendarCtx {
            excluded_dates: Arc::new([d("2026-10-07")].into_iter().collect()),
            off: None,
        };
        let reqs = vec![(d("2026-10-05"), d("2026-10-11"))];
        let out = bucket_leaves_by_date(&reqs, d("2026-10-01"), d("2026-10-31"), &ctx, |r| *r);
        let days: Vec<String> = out.iter().map(|(k, _)| to_date_key(*k)).collect();
        assert_eq!(
            days,
            vec!["2026-10-05", "2026-10-06", "2026-10-08", "2026-10-09"]
        );
    }
}
