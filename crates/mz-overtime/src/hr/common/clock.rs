//! Jam company untuk lembur: `companyTimeZoneOrUtc` (`calendar/day-info.ts`),
//! `OvertimeCalendarService.companyClock` dan `WorkPatternService.currentOffDays`.

use chrono::{NaiveDate, NaiveDateTime};
use mz_core::{ApiError, ApiResult};
use sqlx::PgConnection;

use crate::calendar::{WorkPatternRow, resolve_pattern};
use crate::zone::{find_zone, local_date_key, ms_of};

/// `companyTimeZoneOrUtc(tz)`: nama zona APA ADANYA bila dikenal (lookup tak peka huruf),
/// selain itu `"UTC"` (null/kosong/tak dikenal).
pub fn company_time_zone_or_utc(tz: Option<&str>) -> String {
    match tz {
        Some(t) if !t.is_empty() && find_zone(t).is_some() => t.to_owned(),
        _ => "UTC".to_owned(),
    }
}

/// `CompanyClock`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompanyClock {
    /// Hasil [`company_time_zone_or_utc`].
    pub time_zone: String,
    /// `company.workweekStartDay` (0 = Minggu … 6 = Sabtu).
    pub workweek_start_day: i32,
    /// Tanggal lokal company pada `now`.
    pub local_today: NaiveDate,
}

/// `companyClock(companyId, now)`. Company tidak ada → `404 'Company not found'`.
pub async fn company_clock(
    conn: &mut PgConnection,
    company_id: i32,
    now: NaiveDateTime,
) -> ApiResult<CompanyClock> {
    let row: Option<(i32, String)> =
        sqlx::query_as(r#"SELECT "workweekStartDay", "timeZone" FROM company WHERE id = $1"#)
            .bind(company_id)
            .fetch_optional(conn)
            .await?;
    let Some((workweek_start_day, tz)) = row else {
        return Err(ApiError::not_found("Company not found"));
    };
    let time_zone = company_time_zone_or_utc(Some(&tz));
    Ok(CompanyClock {
        local_today: local_date_key(ms_of(now), Some(&time_zone)),
        time_zone,
        workweek_start_day,
    })
}

#[derive(sqlx::FromRow)]
struct PatternRow {
    id: i32,
    work_days: i16,
    off_days: Vec<i32>,
    short_day: Option<i32>,
    effective_from: NaiveDate,
    created_at: NaiveDateTime,
    superseded_at: Option<NaiveDateTime>,
}

/// `WorkPatternService.loadRows(companyId, ·, toKey)`: semua baris `effectiveFrom ≤ to`.
pub async fn load_pattern_rows(
    conn: &mut PgConnection,
    company_id: i32,
    to: NaiveDate,
) -> sqlx::Result<Vec<WorkPatternRow>> {
    let rows: Vec<PatternRow> = sqlx::query_as(
        r#"SELECT id, "workDays" AS work_days, "offDays" AS off_days, "shortDay" AS short_day,
                  "effectiveFrom" AS effective_from, "createdAt" AS created_at,
                  "supersededAt" AS superseded_at
           FROM "companyWorkPattern"
           WHERE "companyId" = $1 AND "effectiveFrom" <= $2"#,
    )
    .bind(company_id)
    .bind(to)
    .fetch_all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let mut off_days = r.off_days;
            off_days.sort_unstable();
            WorkPatternRow {
                id: r.id,
                work_days: i32::from(r.work_days),
                off_days,
                short_day: r.short_day,
                effective_from: r.effective_from,
                created_at_ms: ms_of(r.created_at),
                superseded_at_ms: r.superseded_at.map(ms_of),
            }
        })
        .collect())
}

/// `currentOffDays(companyId, timeZone, now)`: hari libur mingguan pola yang berlaku
/// hari ini (ISO 1 = Senin … 7 = Minggu); tanpa pola → `[6, 7]`.
pub async fn current_off_days(
    conn: &mut PgConnection,
    company_id: i32,
    time_zone: Option<&str>,
    now: NaiveDateTime,
) -> sqlx::Result<Vec<i32>> {
    let now_ms = ms_of(now);
    let today = local_date_key(now_ms, time_zone);
    let rows = load_pattern_rows(conn, company_id, today).await?;
    Ok(resolve_pattern(&rows, today, now_ms).off_days)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::indexing_slicing, clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn time_zone_or_utc() {
        assert_eq!(
            company_time_zone_or_utc(Some("Asia/Jakarta")),
            "Asia/Jakarta"
        );
        assert_eq!(
            company_time_zone_or_utc(Some("asia/jakarta")),
            "asia/jakarta"
        );
        assert_eq!(company_time_zone_or_utc(Some("Bogus/Zone")), "UTC");
        assert_eq!(company_time_zone_or_utc(Some("")), "UTC");
        assert_eq!(company_time_zone_or_utc(None), "UTC");
    }
}
