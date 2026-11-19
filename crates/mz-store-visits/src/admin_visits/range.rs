//! `AdminStoreVisitsService.resolveDateRange` + `clampToHistoryWindow` (§7.4): batas tanggal
//! dihitung di `company.timeZone` (moment-timezone), hasilnya momen UTC (`timestamp(3)`).

use axum::http::StatusCode;
use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;
use mz_core::ApiError;

use crate::enums::StoreVisitDateRange;
use crate::plan_dates::parse_plan_date;

/// `{ gte?, lte? }` atas `checkInAt` (UTC naif).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DateWindow {
    pub gte: Option<NaiveDateTime>,
    pub lte: Option<NaiveDateTime>,
}

fn invalid(message: String) -> ApiError {
    ApiError::coded(StatusCode::BAD_REQUEST, "INVALID_DATE_RANGE", message)
}

/// Tengah malam lokal `date` → UTC (`startOf('day')`; celah DST → momen sah paling awal).
pub fn local_start(tz: Tz, date: NaiveDate) -> NaiveDateTime {
    local_to_utc(tz, date.and_time(NaiveTime::MIN))
}

/// `endOf('day')`: 23:59:59.999 lokal → UTC.
pub fn local_end(tz: Tz, date: NaiveDate) -> NaiveDateTime {
    let t = NaiveTime::from_hms_milli_opt(23, 59, 59, 999).unwrap_or(NaiveTime::MIN);
    local_to_utc(tz, date.and_time(t))
}

fn local_to_utc(tz: Tz, local: NaiveDateTime) -> NaiveDateTime {
    match tz.from_local_datetime(&local) {
        chrono::LocalResult::Single(d) => d.naive_utc(),
        chrono::LocalResult::Ambiguous(a, _) => a.naive_utc(),
        // Celah DST: moment menggeser maju sebesar celahnya.
        chrono::LocalResult::None => tz
            .from_local_datetime(&(local + Duration::hours(1)))
            .earliest()
            .map_or(local, |d| d.naive_utc()),
    }
}

fn month_bounds(y: i32, m: u32) -> (NaiveDate, NaiveDate) {
    let first = NaiveDate::from_ymd_opt(y, m, 1).unwrap_or(NaiveDate::MIN);
    let next = if m == 12 {
        NaiveDate::from_ymd_opt(y + 1, 1, 1)
    } else {
        NaiveDate::from_ymd_opt(y, m + 1, 1)
    }
    .unwrap_or(first);
    (first, next.pred_opt().unwrap_or(first))
}

/// `resolveDateRange(query, timeZone)` pada jam `now` (UTC). `start`/`end` sudah lolos pola DTO;
/// tanggal di luar kalender → 400 `INVALID_DATE_RANGE`.
pub fn resolve_date_range(
    start: Option<&str>,
    end: Option<&str>,
    range: StoreVisitDateRange,
    tz: Tz,
    now: DateTime<Utc>,
) -> Result<DateWindow, ApiError> {
    if start.is_some() || end.is_some() {
        let day = |v: &str, field: &str| {
            parse_plan_date(v)
                .ok_or_else(|| invalid(format!("{field} is not a valid calendar date")))
        };
        // `query.startDate ? ... : undefined` — string kosong tidak lolos DTO, jadi tak sampai sini.
        let gte = start
            .filter(|s| !s.is_empty())
            .map(|s| day(s, "startDate"))
            .transpose()?
            .map(|d| local_start(tz, d));
        let lte = end
            .filter(|s| !s.is_empty())
            .map(|s| day(s, "endDate"))
            .transpose()?
            .map(|d| local_end(tz, d));
        if let (Some(g), Some(l)) = (gte, lte)
            && g > l
        {
            return Err(invalid("startDate must be on or before endDate".into()));
        }
        return Ok(DateWindow { gte, lte });
    }
    let today = now.with_timezone(&tz).date_naive();
    let w = |a: NaiveDate, b: NaiveDate| DateWindow {
        gte: Some(local_start(tz, a)),
        lte: Some(local_end(tz, b)),
    };
    Ok(match range {
        StoreVisitDateRange::today => w(today, today),
        StoreVisitDateRange::yesterday => {
            let y = today.pred_opt().unwrap_or(today);
            w(y, y)
        }
        StoreVisitDateRange::thisWeek => {
            let monday = today - Duration::days(i64::from(today.weekday().num_days_from_monday()));
            w(monday, monday + Duration::days(6))
        }
        StoreVisitDateRange::thisMonth => {
            let (a, b) = month_bounds(today.year(), today.month());
            w(a, b)
        }
        StoreVisitDateRange::lastMonth => {
            let (y, m) = if today.month() == 1 {
                (today.year() - 1, 12)
            } else {
                (today.year(), today.month() - 1)
            };
            let (a, b) = month_bounds(y, m);
            w(a, b)
        }
        StoreVisitDateRange::last7Days => w(today - Duration::days(6), today),
    })
}

/// `clampToHistoryWindow(window, companyId, timeZone)`: `days <= 0` = tanpa batas; selain itu
/// `gte` dinaikkan ke awal hari `days - 1` hari lalu (zona company) bila lebih awal / kosong.
pub fn clamp_to_history_window(
    window: DateWindow,
    days: i32,
    tz: Tz,
    now: DateTime<Utc>,
) -> DateWindow {
    if days <= 0 {
        return window;
    }
    let today = now.with_timezone(&tz).date_naive();
    let floor = local_start(tz, today - Duration::days(i64::from(days) - 1));
    if window.gte.is_some_and(|g| g >= floor) {
        return window;
    }
    DateWindow {
        gte: Some(floor),
        ..window
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }
    fn n(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.3f").unwrap()
    }
    const JKT: Tz = chrono_tz::Asia::Jakarta;

    #[test]
    fn defaults_to_last7_days() {
        let w = resolve_date_range(
            None,
            None,
            StoreVisitDateRange::last7Days,
            chrono_tz::UTC,
            at("2026-08-20T10:00:00Z"),
        )
        .unwrap();
        assert_eq!(w.gte, Some(n("2026-08-14 00:00:00.000")));
        assert_eq!(w.lte, Some(n("2026-08-20 23:59:59.999")));
    }

    #[test]
    fn cuts_day_at_company_boundary() {
        // 20 Agu 20:00 UTC = 21 Agu 03:00 WIB → "today" = 21 Agu WIB.
        let w = resolve_date_range(
            None,
            None,
            StoreVisitDateRange::today,
            JKT,
            at("2026-08-20T20:00:00Z"),
        )
        .unwrap();
        assert_eq!(w.gte, Some(n("2026-08-20 17:00:00.000")));
        assert_eq!(w.lte, Some(n("2026-08-21 16:59:59.999")));
    }

    #[test]
    fn presets() {
        let now = at("2026-08-20T10:00:00Z"); // Kamis
        let tz = chrono_tz::UTC;
        let w = resolve_date_range(None, None, StoreVisitDateRange::thisWeek, tz, now).unwrap();
        assert_eq!(w.gte, Some(n("2026-08-17 00:00:00.000")));
        assert_eq!(w.lte, Some(n("2026-08-23 23:59:59.999")));
        let w = resolve_date_range(
            None,
            None,
            StoreVisitDateRange::lastMonth,
            tz,
            at("2026-03-31T10:00:00Z"),
        )
        .unwrap();
        assert_eq!(w.gte, Some(n("2026-02-01 00:00:00.000")));
        assert_eq!(w.lte, Some(n("2026-02-28 23:59:59.999")));
        let w = resolve_date_range(None, None, StoreVisitDateRange::thisMonth, tz, now).unwrap();
        assert_eq!(w.lte, Some(n("2026-08-31 23:59:59.999")));
        let w = resolve_date_range(None, None, StoreVisitDateRange::yesterday, tz, now).unwrap();
        assert_eq!(w.gte, Some(n("2026-08-19 00:00:00.000")));
    }

    #[test]
    fn explicit_dates_win_and_open_ended() {
        let now = at("2026-08-20T10:00:00Z");
        let w = resolve_date_range(
            Some("2026-08-01"),
            Some("2026-08-02"),
            StoreVisitDateRange::today,
            JKT,
            now,
        )
        .unwrap();
        assert_eq!(w.gte, Some(n("2026-07-31 17:00:00.000")));
        assert_eq!(w.lte, Some(n("2026-08-02 16:59:59.999")));
        let w = resolve_date_range(
            Some("2026-08-01"),
            None,
            StoreVisitDateRange::today,
            chrono_tz::UTC,
            now,
        )
        .unwrap();
        assert_eq!(w.lte, None);
    }

    #[test]
    fn rejects_bad_ranges() {
        let now = at("2026-08-20T10:00:00Z");
        let e = resolve_date_range(
            Some("2026-08-03"),
            Some("2026-08-02"),
            StoreVisitDateRange::today,
            chrono_tz::UTC,
            now,
        )
        .unwrap_err();
        assert!(
            matches!(e, ApiError::Body { status, ref body } if status == StatusCode::BAD_REQUEST
            && body["message"] == "startDate must be on or before endDate")
        );
        let e = resolve_date_range(
            Some("2026-02-31"),
            None,
            StoreVisitDateRange::today,
            chrono_tz::UTC,
            now,
        )
        .unwrap_err();
        assert!(
            matches!(e, ApiError::Body { ref body, .. } if body["code"] == "INVALID_DATE_RANGE"
            && body["message"] == "startDate is not a valid calendar date")
        );
    }

    #[test]
    fn history_clamp() {
        let now = at("2026-08-20T10:00:00Z");
        let w = DateWindow {
            gte: Some(n("2026-01-01 00:00:00.000")),
            lte: None,
        };
        assert_eq!(clamp_to_history_window(w, 0, chrono_tz::UTC, now), w);
        let c = clamp_to_history_window(w, 30, chrono_tz::UTC, now);
        assert_eq!(c.gte, Some(n("2026-07-22 00:00:00.000")));
        let late = DateWindow {
            gte: Some(n("2026-08-19 00:00:00.000")),
            lte: None,
        };
        assert_eq!(clamp_to_history_window(late, 30, chrono_tz::UTC, now), late);
        let open = clamp_to_history_window(DateWindow::default(), 1, chrono_tz::UTC, now);
        assert_eq!(open.gte, Some(n("2026-08-20 00:00:00.000")));
    }
}
