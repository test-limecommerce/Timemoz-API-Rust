//! `src/leaves/leave-segments.ts`: rentang cuti dipecah per tahun kalender (cuti 31 Des /
//! 1 Jan dibebankan ke saldo tiap tahun) dan deteksi tumpang tindih pengajuan.

use std::collections::HashSet;

use chrono::{Datelike, NaiveDate};
use mz_core::ApiError;
use sqlx::PgExecutor;

use crate::day_count::{DayCount, DayCountError, count_leave_days, to_date_key};
use crate::enums::{LeaveDayType, LeaveStatus};

/// `YearSegment` (+ `days` dari [`count_leave_days_by_year`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct YearSegment {
    pub year: i32,
    pub start: NaiveDate,
    pub end: NaiveDate,
    pub start_day_type: LeaveDayType,
    pub end_day_type: LeaveDayType,
    pub days: f64,
}

/// `splitRangeByYear(...)`: satu segmen per tahun kalender. Segmen pertama memakai
/// `start_day_type`, terakhir `end_day_type`; batas di dalam rentang FULL_DAY. `days` = 0
/// (diisi [`count_leave_days_by_year`]).
pub fn split_range_by_year(
    start: NaiveDate,
    end: NaiveDate,
    start_day_type: LeaveDayType,
    end_day_type: LeaveDayType,
) -> Result<Vec<YearSegment>, DayCountError> {
    if end < start {
        return Err(DayCountError::EndBeforeStart);
    }
    let (start_year, end_year) = (start.year(), end.year());
    let mut out = Vec::new();
    for year in start_year..=end_year {
        let is_first = year == start_year;
        let is_last = year == end_year;
        let seg_start = if is_first {
            start
        } else {
            NaiveDate::from_ymd_opt(year, 1, 1).unwrap_or(start)
        };
        let seg_end = if is_last {
            end
        } else {
            NaiveDate::from_ymd_opt(year, 12, 31).unwrap_or(end)
        };
        let mut seg_start_type = if is_first {
            start_day_type
        } else {
            LeaveDayType::FULL_DAY
        };
        let seg_end_type = if is_last {
            end_day_type
        } else {
            LeaveDayType::FULL_DAY
        };
        // countLeaveDays hanya membaca startDayType untuk rentang sehari, jadi segmen terakhir
        // sehari (mis. 1 Jan) membawa porsi akhir pengajuan di sana.
        if !is_first && is_last && seg_start == seg_end {
            seg_start_type = end_day_type;
        }
        out.push(YearSegment {
            year,
            start: seg_start,
            end: seg_end,
            start_day_type: seg_start_type,
            end_day_type: seg_end_type,
            days: 0.0,
        });
    }
    Ok(out)
}

/// `countLeaveDaysByYear(input)`: `countLeaveDays` atas seluruh rentang (validasi & total sama
/// persis) plus hari yang dibebankan ke tiap tahun; jumlah per tahun = `total_days`.
pub fn count_leave_days_by_year(
    start_date: &str,
    end_date: &str,
    start_day_type: LeaveDayType,
    end_day_type: LeaveDayType,
    excluded_dates: &HashSet<NaiveDate>,
    is_weekly_off: Option<&dyn Fn(NaiveDate) -> bool>,
) -> Result<(DayCount, Vec<YearSegment>), DayCountError> {
    // Seluruh rentang dulu: memvalidasi tanggal & batas 366 hari sebelum dipecah.
    let whole = count_leave_days(
        start_date,
        end_date,
        start_day_type,
        end_day_type,
        excluded_dates,
        is_weekly_off,
    )?;
    let (Some(start), Some(end)) = (
        crate::day_count::parse_date_key(start_date),
        crate::day_count::parse_date_key(end_date),
    ) else {
        return Err(DayCountError::InvalidDate);
    };
    let mut segments = split_range_by_year(start, end, start_day_type, end_day_type)?;
    for seg in &mut segments {
        seg.days = count_leave_days(
            &to_date_key(seg.start),
            &to_date_key(seg.end),
            seg.start_day_type,
            seg.end_day_type,
            excluded_dates,
            is_weekly_off,
        )?
        .total_days;
    }
    Ok((whole, segments))
}

/// `LeaveRangeLike`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeaveRange {
    pub start: NaiveDate,
    pub end: NaiveDate,
    pub start_day_type: LeaveDayType,
    pub end_day_type: LeaveDayType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Portion {
    Full,
    Am,
    Pm,
}

fn portion_on(r: &LeaveRange, d: NaiveDate) -> Portion {
    // Aturan countLeaveDays: pengajuan sehari memakai startDayType.
    let t = if d == r.start {
        r.start_day_type
    } else if d == r.end {
        r.end_day_type
    } else {
        LeaveDayType::FULL_DAY
    };
    match t {
        LeaveDayType::HALF_DAY_MORNING => Portion::Am,
        LeaveDayType::HALF_DAY_AFTERNOON => Portion::Pm,
        LeaveDayType::FULL_DAY => Portion::Full,
    }
}

fn clash_on(a: &LeaveRange, b: &LeaveRange, d: NaiveDate) -> bool {
    let (pa, pb) = (portion_on(a, d), portion_on(b, d));
    pa == Portion::Full || pb == Portion::Full || pa == pb
}

/// `OverlapCalendar`: libur company + predikat libur mingguan (seperti `countLeaveDays`).
#[derive(Clone, Copy)]
pub struct OverlapCalendar<'a> {
    pub excluded_dates: &'a HashSet<NaiveDate>,
    pub is_weekly_off: &'a (dyn Fn(NaiveDate) -> bool + Sync),
}

/// `leaveRangesConflict(a, b, calendar)`: dua cuti mengklaim bagian yang sama dari satu hari
/// kerja. Setengah hari pagi & siang di tanggal sama tidak bentrok. Dengan `calendar`, libur
/// mingguan & hari libur tak pernah bentrok; tanpa itu setiap tanggal bersama dihitung.
pub fn leave_ranges_conflict(
    a: &LeaveRange,
    b: &LeaveRange,
    calendar: Option<OverlapCalendar<'_>>,
) -> bool {
    let from = a.start.max(b.start);
    let to = a.end.min(b.end);
    if from > to {
        return false;
    }
    if let Some(cal) = calendar {
        return from
            .iter_days()
            .take_while(|d| *d <= to)
            .filter(|d| !cal.excluded_dates.contains(d) && !(cal.is_weekly_off)(*d))
            .any(|d| clash_on(a, b, d));
    }
    // Hanya hari pertama & terakhir pengajuan yang bisa setengah hari. Dengan tiga tanggal
    // bersama atau lebih, tanggal tengah ada di dalam kedua rentang → hari penuh bagi keduanya.
    let Some(second) = from.succ_opt() else {
        return clash_on(a, b, from);
    };
    if second < to {
        return true;
    }
    let keys: &[NaiveDate] = if second == to { &[from, to] } else { &[from] };
    keys.iter().any(|d| clash_on(a, b, *d))
}

/// `OverlappingLeave`.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct OverlappingLeave {
    pub id: i32,
    pub uuid: String,
    pub start_date: NaiveDate,
    pub end_date: NaiveDate,
    pub start_day_type: String,
    pub end_day_type: String,
    pub status: String,
}

impl OverlappingLeave {
    fn range(&self) -> LeaveRange {
        LeaveRange {
            start: self.start_date,
            end: self.end_date,
            start_day_type: self
                .start_day_type
                .parse()
                .unwrap_or(LeaveDayType::FULL_DAY),
            end_day_type: self.end_day_type.parse().unwrap_or(LeaveDayType::FULL_DAY),
        }
    }
}

/// `findOverlappingLeave(db, input)`: pengajuan lain pertama karyawan (company sama, status
/// yang diberikan) yang mengklaim hari yang sama. Baca saja: pemanggil yang harus bebas balapan
/// menjalankannya di bawah `lock_balance`.
#[allow(clippy::too_many_arguments)]
pub async fn find_overlapping_leave(
    exec: impl PgExecutor<'_>,
    company_id: i32,
    user_id: i32,
    exclude_id: Option<i32>,
    statuses: &[LeaveStatus],
    range: &LeaveRange,
    calendar: Option<OverlapCalendar<'_>>,
) -> sqlx::Result<Option<OverlappingLeave>> {
    let statuses: Vec<&str> = statuses.iter().map(|s| s.as_str()).collect();
    let candidates: Vec<OverlappingLeave> = sqlx::query_as(
        r#"SELECT id, uuid::text AS uuid, "startDate" AS start_date, "endDate" AS end_date,
                  "startDayType"::text AS start_day_type, "endDayType"::text AS end_day_type,
                  status::text AS status
             FROM "leaveRequest"
            WHERE "companyId" = $1 AND "userId" = $2 AND ($3::int IS NULL OR id <> $3)
              AND status::text = ANY($4) AND "startDate" <= $5 AND "endDate" >= $6
            ORDER BY "startDate" ASC, id ASC"#,
    )
    .bind(company_id)
    .bind(user_id)
    .bind(exclude_id)
    .bind(&statuses)
    .bind(range.end)
    .bind(range.start)
    .fetch_all(exec)
    .await?;
    Ok(candidates
        .into_iter()
        .find(|c| leave_ranges_conflict(range, &c.range(), calendar)))
}

/// `overlapError(hit)`: 400 `{ code: 'LEAVE_OVERLAP', message }`.
pub fn overlap_error(hit: &OverlappingLeave) -> ApiError {
    let (start, end) = (to_date_key(hit.start_date), to_date_key(hit.end_date));
    let range = if start == end {
        start
    } else {
        format!("{start} s.d. {end}")
    };
    ApiError::coded(
        http::StatusCode::BAD_REQUEST,
        "LEAVE_OVERLAP",
        format!(
            "Tanggal cuti bertabrakan dengan pengajuan lain ({range}, status {})",
            hit.status
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use LeaveDayType::{FULL_DAY, HALF_DAY_AFTERNOON, HALF_DAY_MORNING};

    fn d(s: &str) -> NaiveDate {
        crate::day_count::parse_date_key(s).unwrap_or_default()
    }

    fn r(s: &str, e: &str, st: LeaveDayType, et: LeaveDayType) -> LeaveRange {
        LeaveRange {
            start: d(s),
            end: d(e),
            start_day_type: st,
            end_day_type: et,
        }
    }

    fn by_year(
        s: &str,
        e: &str,
        st: LeaveDayType,
        et: LeaveDayType,
    ) -> Vec<(i32, String, String, LeaveDayType, LeaveDayType, f64)> {
        count_leave_days_by_year(s, e, st, et, &HashSet::new(), None)
            .map(|(_, segs)| {
                segs.into_iter()
                    .map(|g| {
                        (
                            g.year,
                            to_date_key(g.start),
                            to_date_key(g.end),
                            g.start_day_type,
                            g.end_day_type,
                            g.days,
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    // leave-segments.spec.ts › splitRangeByYear
    #[test]
    fn single_year_is_one_segment() {
        assert_eq!(
            by_year(
                "2026-10-05",
                "2026-10-09",
                HALF_DAY_AFTERNOON,
                HALF_DAY_MORNING
            ),
            vec![(
                2026,
                "2026-10-05".into(),
                "2026-10-09".into(),
                HALF_DAY_AFTERNOON,
                HALF_DAY_MORNING,
                4.0
            )]
        );
    }

    #[test]
    fn cross_year_split_keeps_edges() {
        // 2026-12-30 Rabu .. 2027-01-04 Senin: 30, 31 Des + 1 Jan (Jum), 4 Jan.
        let segs = by_year(
            "2026-12-30",
            "2027-01-04",
            HALF_DAY_AFTERNOON,
            HALF_DAY_MORNING,
        );
        assert_eq!(
            segs,
            vec![
                (
                    2026,
                    "2026-12-30".into(),
                    "2026-12-31".into(),
                    HALF_DAY_AFTERNOON,
                    FULL_DAY,
                    1.5
                ),
                (
                    2027,
                    "2027-01-01".into(),
                    "2027-01-04".into(),
                    FULL_DAY,
                    HALF_DAY_MORNING,
                    1.5
                ),
            ]
        );
    }

    #[test]
    fn one_day_last_segment_carries_end_portion() {
        let segs = by_year("2026-12-31", "2027-01-01", FULL_DAY, HALF_DAY_MORNING);
        assert_eq!(
            segs.last().map(|s| (s.3, s.5)),
            Some((HALF_DAY_MORNING, 0.5))
        );
        assert_eq!(segs.iter().map(|s| s.5).sum::<f64>(), 1.5);
    }

    #[test]
    fn whole_range_validated_first() {
        assert_eq!(
            count_leave_days_by_year(
                "2026-10-09",
                "2026-10-05",
                FULL_DAY,
                FULL_DAY,
                &HashSet::new(),
                None
            )
            .err(),
            Some(DayCountError::EndBeforeStart)
        );
    }

    // › leaveRangesConflict
    #[test]
    fn half_days_on_same_date() {
        let am = r("2026-10-07", "2026-10-07", HALF_DAY_MORNING, FULL_DAY);
        let pm = r("2026-10-07", "2026-10-07", HALF_DAY_AFTERNOON, FULL_DAY);
        assert!(!leave_ranges_conflict(&am, &pm, None));
        assert!(leave_ranges_conflict(&am, &am, None));
        let full = r("2026-10-07", "2026-10-07", FULL_DAY, FULL_DAY);
        assert!(leave_ranges_conflict(&am, &full, None));
    }

    #[test]
    fn adjacent_half_days() {
        // a berakhir pagi 8 Okt, b mulai siang 8 Okt → tidak bentrok.
        let a = r("2026-10-06", "2026-10-08", FULL_DAY, HALF_DAY_MORNING);
        let b = r("2026-10-08", "2026-10-09", HALF_DAY_AFTERNOON, FULL_DAY);
        assert!(!leave_ranges_conflict(&a, &b, None));
        // Dua tanggal bersama: 8 Okt (a pagi, b siang) dan 9 Okt (a penuh) → bentrok.
        let a = r("2026-10-06", "2026-10-09", FULL_DAY, FULL_DAY);
        let b = r(
            "2026-10-08",
            "2026-10-09",
            HALF_DAY_AFTERNOON,
            HALF_DAY_MORNING,
        );
        assert!(leave_ranges_conflict(&a, &b, None));
        let a = r("2026-10-01", "2026-10-02", FULL_DAY, FULL_DAY);
        let b = r("2026-10-03", "2026-10-04", FULL_DAY, FULL_DAY);
        assert!(!leave_ranges_conflict(&a, &b, None));
    }

    #[test]
    fn calendar_skips_off_days() {
        // Hanya bersinggungan di akhir pekan (10–11 Okt 2026).
        let a = r("2026-10-08", "2026-10-11", FULL_DAY, FULL_DAY);
        let b = r("2026-10-10", "2026-10-12", FULL_DAY, FULL_DAY);
        let none = HashSet::new();
        let weekend = |x: NaiveDate| crate::day_count::is_sat_sun(x);
        let cal = OverlapCalendar {
            excluded_dates: &none,
            is_weekly_off: &weekend,
        };
        assert!(!leave_ranges_conflict(
            &r("2026-10-08", "2026-10-11", FULL_DAY, FULL_DAY),
            &r("2026-10-10", "2026-10-11", FULL_DAY, FULL_DAY),
            Some(cal)
        ));
        assert!(leave_ranges_conflict(&a, &b, None));
        let holiday: HashSet<NaiveDate> = [d("2026-10-12")].into_iter().collect();
        let cal = OverlapCalendar {
            excluded_dates: &holiday,
            is_weekly_off: &weekend,
        };
        assert!(!leave_ranges_conflict(&a, &b, Some(cal)));
    }

    #[test]
    fn overlap_message() {
        let hit = OverlappingLeave {
            id: 1,
            uuid: "u".into(),
            start_date: d("2026-10-05"),
            end_date: d("2026-10-05"),
            start_day_type: "FULL_DAY".into(),
            end_day_type: "FULL_DAY".into(),
            status: "PENDING".into(),
        };
        let e = format!("{:?}", overlap_error(&hit));
        assert!(e.contains("LEAVE_OVERLAP"));
        assert!(e.contains("(2026-10-05, status PENDING)"));
    }
}
