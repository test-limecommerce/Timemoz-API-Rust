//! `src/leaves/leave-day-count.ts` (hitung hari cuti, murni) + `dateKeyOf`
//! (`holiday-calendar.ts`).

use std::collections::HashSet;

use chrono::{Datelike, NaiveDate};
use mz_core::ApiError;
use serde::Serialize;

use crate::enums::LeaveDayType;

/// `MAX_RANGE_DAYS`.
pub const MAX_RANGE_DAYS: i64 = 366;

/// `toDateKey(d)`: `YYYY-MM-DD` dari kolom `@db.Date`.
pub fn to_date_key(d: NaiveDate) -> String {
    d.format("%Y-%m-%d").to_string()
}

/// `moment.utc(key, 'YYYY-MM-DD', true)` — kunci ketat (`2026-1-5` ditolak).
pub fn parse_date_key(key: &str) -> Option<NaiveDate> {
    mz_overtime::zone::parse_date_key(key)
}

/// `dateKeyOf(value)`: 10 karakter pertama string tanggal DTO (sudah divalidasi).
pub fn date_key_of(value: &str) -> &str {
    value.get(..10).unwrap_or(value)
}

/// Default pola tanpa baris `companyWorkPattern` (FR-047): Sabtu & Minggu libur.
pub fn is_sat_sun(d: NaiveDate) -> bool {
    d.weekday().number_from_monday() >= 6
}

/// Alasan tanggal tidak dihitung (`'WEEKEND' | 'HOLIDAY'`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ExclusionReason {
    #[serde(rename = "WEEKEND")]
    Weekend,
    #[serde(rename = "HOLIDAY")]
    Holiday,
}

/// Satu entri `excluded` (`{ date, reason }`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Exclusion {
    pub date: String,
    pub reason: ExclusionReason,
}

/// `CountLeaveDaysResult`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DayCount {
    #[serde(serialize_with = "crate::js::ser_f64")]
    pub total_days: f64,
    pub excluded: Vec<Exclusion>,
}

/// `BadRequestException` dari `countLeaveDays`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DayCountError {
    InvalidDate,
    EndBeforeStart,
    RangeTooLong,
}

impl DayCountError {
    pub fn message(self) -> &'static str {
        match self {
            Self::InvalidDate => "Tanggal cuti tidak valid",
            Self::EndBeforeStart => "Tanggal selesai tidak boleh sebelum tanggal mulai",
            Self::RangeTooLong => "Rentang cuti maksimal 366 hari",
        }
    }
}

impl std::fmt::Display for DayCountError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for DayCountError {}

impl From<DayCountError> for ApiError {
    /// `new BadRequestException(msg)` → 400 `{message, error: "Bad Request", statusCode}`.
    fn from(e: DayCountError) -> Self {
        ApiError::bad_request(e.message())
    }
}

/// `countLeaveDays({ startDate, endDate, startDayType, endDayType, excludedDates, isWeeklyOff })`.
/// Tanggal = kunci `YYYY-MM-DD` ketat. `is_weekly_off = None` → Sabtu & Minggu. Hari libur
/// mingguan diperiksa SEBELUM `excluded_dates` (libur di hari libur mingguan → `WEEKEND`).
pub fn count_leave_days(
    start_date: &str,
    end_date: &str,
    start_day_type: LeaveDayType,
    end_day_type: LeaveDayType,
    excluded_dates: &HashSet<NaiveDate>,
    is_weekly_off: Option<&dyn Fn(NaiveDate) -> bool>,
) -> Result<DayCount, DayCountError> {
    let (Some(start), Some(end)) = (parse_date_key(start_date), parse_date_key(end_date)) else {
        return Err(DayCountError::InvalidDate);
    };
    if end < start {
        return Err(DayCountError::EndBeforeStart);
    }
    let span_days = (end - start).num_days() + 1;
    if span_days > MAX_RANGE_DAYS {
        return Err(DayCountError::RangeTooLong);
    }
    let single_day = span_days == 1;
    let mut excluded = Vec::new();
    let mut total = 0.0;
    for (i, day) in start.iter_days().take_while(|d| *d <= end).enumerate() {
        let off = match is_weekly_off {
            Some(f) => f(day),
            None => is_sat_sun(day),
        };
        if off {
            excluded.push(Exclusion {
                date: to_date_key(day),
                reason: ExclusionReason::Weekend,
            });
            continue;
        }
        if excluded_dates.contains(&day) {
            excluded.push(Exclusion {
                date: to_date_key(day),
                reason: ExclusionReason::Holiday,
            });
            continue;
        }
        let last = i64::try_from(i).unwrap_or(i64::MAX) == span_days - 1;
        let half = (i == 0 && start_day_type != LeaveDayType::FULL_DAY)
            || (!single_day && last && end_day_type != LeaveDayType::FULL_DAY);
        let value = if half { 0.5 } else { 1.0 };
        total += value;
    }
    Ok(DayCount {
        total_days: total,
        excluded,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use LeaveDayType::{FULL_DAY, HALF_DAY_AFTERNOON, HALF_DAY_MORNING};

    fn d(s: &str) -> NaiveDate {
        parse_date_key(s).unwrap_or_default()
    }

    fn set(keys: &[&str]) -> HashSet<NaiveDate> {
        keys.iter().map(|k| d(k)).collect()
    }

    fn full(s: &str, e: &str) -> DayCount {
        count_leave_days(s, e, FULL_DAY, FULL_DAY, &HashSet::new(), None).unwrap_or(DayCount {
            total_days: -1.0,
            excluded: vec![],
        })
    }

    fn reasons(r: &DayCount) -> Vec<ExclusionReason> {
        r.excluded.iter().map(|e| e.reason).collect()
    }

    // leave-day-count.spec.ts › countLeaveDays (2026-10-05 = Senin)
    #[test]
    fn counts_weekdays() {
        assert_eq!(full("2026-10-05", "2026-10-09").total_days, 5.0);
        let r = full("2026-10-05", "2026-10-12");
        assert_eq!(r.total_days, 6.0);
        assert_eq!(
            reasons(&r),
            vec![ExclusionReason::Weekend, ExclusionReason::Weekend]
        );
        assert_eq!(full("2026-10-10", "2026-10-11").total_days, 0.0);
    }

    #[test]
    fn holidays_and_half_days() {
        let r = count_leave_days(
            "2026-10-05",
            "2026-10-09",
            FULL_DAY,
            FULL_DAY,
            &set(&["2026-10-07"]),
            None,
        )
        .unwrap_or_else(|_| full("x", "y"));
        assert_eq!(r.total_days, 4.0);
        assert_eq!(
            r.excluded,
            vec![Exclusion {
                date: "2026-10-07".into(),
                reason: ExclusionReason::Holiday
            }]
        );
        let r = count_leave_days(
            "2026-10-05",
            "2026-10-07",
            HALF_DAY_AFTERNOON,
            HALF_DAY_MORNING,
            &HashSet::new(),
            None,
        );
        assert_eq!(r.map(|x| x.total_days), Ok(2.0));
        let r = count_leave_days(
            "2026-10-07",
            "2026-10-07",
            HALF_DAY_MORNING,
            FULL_DAY,
            &set(&["2026-10-07"]),
            None,
        );
        assert_eq!(r.map(|x| x.total_days), Ok(0.0));
        let r = count_leave_days(
            "2026-10-07",
            "2026-10-07",
            HALF_DAY_MORNING,
            FULL_DAY,
            &HashSet::new(),
            None,
        );
        assert_eq!(r.map(|x| x.total_days), Ok(0.5));
        // Sehari: endDayType diabaikan.
        let r = count_leave_days(
            "2026-10-07",
            "2026-10-07",
            FULL_DAY,
            HALF_DAY_MORNING,
            &HashSet::new(),
            None,
        );
        assert_eq!(r.map(|x| x.total_days), Ok(1.0));
    }

    #[test]
    fn rejects_bad_ranges() {
        let e = |s, en| count_leave_days(s, en, FULL_DAY, FULL_DAY, &HashSet::new(), None).err();
        assert_eq!(
            e("2026-10-09", "2026-10-05"),
            Some(DayCountError::EndBeforeStart)
        );
        assert_eq!(
            e("2026-01-01", "2027-01-02"),
            Some(DayCountError::RangeTooLong)
        );
        assert_eq!(e("2026-01-01", "2027-01-01"), None);
        assert_eq!(
            e("2026-02-30", "2026-03-01"),
            Some(DayCountError::InvalidDate)
        );
        assert_eq!(
            e("2026-1-05", "2026-03-01"),
            Some(DayCountError::InvalidDate)
        );
        assert_eq!(
            DayCountError::EndBeforeStart.message(),
            "Tanggal selesai tidak boleh sebelum tanggal mulai"
        );
    }

    // › isWeeklyOff (pola 6 hari: hanya Minggu libur)
    #[test]
    fn weekly_off_predicate() {
        let sundays = set(&["2026-10-25", "2026-11-01"]);
        let sunday_only = |k: NaiveDate| sundays.contains(&k);
        let f: &dyn Fn(NaiveDate) -> bool = &sunday_only;
        let none = HashSet::new();
        let r = count_leave_days(
            "2026-10-30",
            "2026-10-31",
            FULL_DAY,
            FULL_DAY,
            &none,
            Some(f),
        );
        assert_eq!(
            r.as_ref().map(|x| (x.total_days, x.excluded.len())),
            Ok((2.0, 0))
        );
        let r = count_leave_days(
            "2026-10-31",
            "2026-10-31",
            FULL_DAY,
            FULL_DAY,
            &none,
            Some(f),
        );
        assert_eq!(r.map(|x| x.total_days), Ok(1.0));
        let r = count_leave_days(
            "2026-10-30",
            "2026-11-01",
            FULL_DAY,
            FULL_DAY,
            &none,
            Some(f),
        );
        assert_eq!(
            r.map(|x| (x.total_days, x.excluded)),
            Ok((
                2.0,
                vec![Exclusion {
                    date: "2026-11-01".into(),
                    reason: ExclusionReason::Weekend
                }]
            ))
        );
        let r = full("2026-10-30", "2026-10-31");
        assert_eq!(
            (r.total_days, reasons(&r)),
            (1.0, vec![ExclusionReason::Weekend])
        );
        let r = count_leave_days(
            "2026-10-30",
            "2026-11-01",
            FULL_DAY,
            FULL_DAY,
            &set(&["2026-11-01"]),
            Some(f),
        );
        assert_eq!(
            r.map(|x| (x.total_days, reasons(&x))),
            Ok((2.0, vec![ExclusionReason::Weekend]))
        );
    }

    #[test]
    fn keys() {
        assert_eq!(date_key_of("2026-10-05T00:00:00.000Z"), "2026-10-05");
        assert_eq!(date_key_of("2026"), "2026");
        assert_eq!(to_date_key(d("2026-01-02")), "2026-01-02");
        assert!(
            is_sat_sun(d("2026-10-10"))
                && is_sat_sun(d("2026-10-11"))
                && !is_sat_sun(d("2026-10-09"))
        );
    }
}
