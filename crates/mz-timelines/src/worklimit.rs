//! `remainingWorkTimeSeconds` + `checkWorkTimeLimit` (`tracking-timelines.service.ts:3540`).

use chrono::{NaiveDate, NaiveDateTime};
use chrono_tz::Tz;
use sqlx::PgConnection;

use crate::range::{WorkWindow, daily_window, timeline_range_ms, used_seconds_of, weekly_window};

/// Baris `userWorkTimeLimit` aktif (+ `company.workweekStartDay ?? 1`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limit {
    pub daily: Option<i32>,
    pub weekly: Option<i32>,
    pub workweek_start_day: i32,
}

/// Sisa detik; `None` di sisi tertentu = sisi itu tanpa batas.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Remaining {
    pub daily: Option<f64>,
    pub weekly: Option<f64>,
}

/// Baca batas aktif (`findFirst({ userId, companyId, status: 1 })`).
pub async fn load_limit(
    conn: &mut PgConnection,
    user_id: i32,
    company_id: i32,
) -> sqlx::Result<Option<Limit>> {
    let row: Option<(Option<i32>, Option<i32>, Option<i32>)> = sqlx::query_as(
        r#"SELECT l."dailyLimitSec", l."weeklyLimitSec", c."workweekStartDay"
           FROM "userWorkTimeLimit" l LEFT JOIN company c ON c.id = l."companyId"
           WHERE l."userId" = $1 AND l."companyId" = $2 AND l.status = 1 LIMIT 1"#,
    )
    .bind(user_id)
    .bind(company_id)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.map(|(daily, weekly, wsd)| Limit {
        daily,
        weekly,
        workweek_start_day: wsd.unwrap_or(1),
    }))
}

#[derive(sqlx::FromRow)]
struct UsageRow {
    start_at: NaiveDateTime,
    end_at: NaiveDateTime,
    start_at_utc: Option<NaiveDateTime>,
    end_at_utc: Option<NaiveDateTime>,
    timezone: Option<String>,
    by_taken: bool,
}

/// Union baris `takenDate` di jendela + (bila `local`) baris yang MULAI di
/// jendela lokal — satu query, dedupe otomatis per baris.
async fn used_seconds(conn: &mut PgConnection, user_id: i32, w: &WorkWindow) -> sqlx::Result<f64> {
    use crate::range::{OVERLAP_WINDOW_MARGIN_MS, ndt};
    let (has_local, lo, hi) = match w.local {
        Some(r) => (
            true,
            ndt(r.start - OVERLAP_WINDOW_MARGIN_MS),
            ndt(r.end + OVERLAP_WINDOW_MARGIN_MS),
        ),
        None => (false, ndt(0), ndt(0)),
    };
    let rows: Vec<UsageRow> = sqlx::query_as(
        r#"SELECT tt."startAt" AS start_at, tt."endAt" AS end_at, tt."startAtUtc" AS start_at_utc,
                  tt."endAtUtc" AS end_at_utc, tt.timezone,
                  (t."takenDate" BETWEEN $2 AND $3) AS by_taken
           FROM "trackingTimeline" tt JOIN tracking t ON t.id = tt."trackingId"
           WHERE tt.status = 1 AND t."userId" = $1
             AND ((t."takenDate" BETWEEN $2 AND $3)
                  OR ($4 AND tt."startAt" >= $5 AND tt."startAt" < $6))"#,
    )
    .bind(user_id)
    .bind(w.taken_from)
    .bind(w.taken_to)
    .bind(has_local)
    .bind(lo)
    .bind(hi)
    .fetch_all(&mut *conn)
    .await?;
    Ok(sum_usage(&rows, w))
}

fn sum_usage(rows: &[UsageRow], w: &WorkWindow) -> f64 {
    rows.iter()
        .filter(|r| {
            r.by_taken
                || w.local.is_some_and(|l| {
                    let s = timeline_range_ms(r.start_at, r.end_at, r.timezone.as_deref()).start;
                    s >= l.start && s < l.end
                })
        })
        .map(|r| used_seconds_of(r.start_at, r.end_at, r.start_at_utc, r.end_at_utc))
        .sum()
}

/// `remainingWorkTimeSeconds(userId, companyId, takenDate, { client, zone })`
/// dengan baris batas yang sudah dibaca.
pub async fn remaining_for(
    conn: &mut PgConnection,
    user_id: i32,
    limit: Limit,
    taken: NaiveDate,
    zone: Option<Tz>,
) -> sqlx::Result<Remaining> {
    let mut out = Remaining::default();
    if let Some(daily) = limit.daily {
        let used = used_seconds(conn, user_id, &daily_window(taken, zone)).await?;
        out.daily = Some((f64::from(daily) - used).max(0.0));
    }
    if let Some(weekly) = limit.weekly {
        let w = weekly_window(taken, limit.workweek_start_day, zone);
        let used = used_seconds(conn, user_id, &w).await?;
        out.weekly = Some((f64::from(weekly) - used).max(0.0));
    }
    Ok(out)
}

/// `checkWorkTimeLimit`: `Some("daily" | "weekly")` = terlampaui.
pub fn exceeded(remaining: Remaining, new_segment_sec: i64) -> Option<&'static str> {
    #[allow(clippy::cast_precision_loss)]
    let seg = new_segment_sec as f64;
    if remaining.daily.is_some_and(|d| seg > d) {
        return Some("daily");
    }
    if remaining.weekly.is_some_and(|w| seg > w) {
        return Some("weekly");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::range::{MsRange, ms_of};

    fn dt(s: &str) -> NaiveDateTime {
        mz_core::time::parse_js_date(s).unwrap_or_default()
    }

    fn row(start: &str, end: &str, tz: Option<&str>, by_taken: bool) -> UsageRow {
        UsageRow {
            start_at: dt(start),
            end_at: dt(end),
            start_at_utc: None,
            end_at_utc: None,
            timezone: tz.map(str::to_owned),
            by_taken,
        }
    }

    #[test]
    fn union_counts_taken_date_and_local_start_once() {
        let local = MsRange::new(
            ms_of(dt("2026-11-15T17:00:00Z")),
            ms_of(dt("2026-11-16T17:00:00Z")),
        );
        let w = WorkWindow {
            taken_from: NaiveDate::default(),
            taken_to: NaiveDate::default(),
            local: Some(local),
        };
        let rows = vec![
            row(
                "2026-11-16T01:00:00Z",
                "2026-11-16T02:00:00Z",
                Some("Asia/Jakarta"),
                true,
            ),
            // Menempel ke tracking lain tapi mulai di hari lokal itu.
            row(
                "2026-11-16T03:00:00Z",
                "2026-11-16T03:30:00Z",
                Some("Asia/Jakarta"),
                false,
            ),
            // Di luar jendela lokal.
            row(
                "2026-11-16T18:00:00Z",
                "2026-11-16T19:00:00Z",
                Some("Asia/Jakarta"),
                false,
            ),
            // Legacy: jam dinding 23:00 WIB tgl 16 = 16:00Z → masuk.
            row("2026-11-16T23:00:00Z", "2026-11-16T23:10:00Z", None, false),
        ];
        assert!((sum_usage(&rows, &w) - (3600.0 + 1800.0 + 600.0)).abs() < 1e-9);
        let no_zone = WorkWindow { local: None, ..w };
        assert!((sum_usage(&rows, &no_zone) - 3600.0).abs() < 1e-9);
    }

    #[test]
    fn limit_check_daily_before_weekly() {
        let r = Remaining {
            daily: Some(100.0),
            weekly: Some(50.0),
        };
        assert_eq!(exceeded(r, 101), Some("daily"));
        assert_eq!(exceeded(r, 60), Some("weekly"));
        assert_eq!(exceeded(r, 50), None);
        assert_eq!(exceeded(Remaining::default(), 1_000_000), None);
    }
}
