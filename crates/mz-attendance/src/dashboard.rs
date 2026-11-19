//! Port `src/mobile-dashboard/mobile-dashboard.service.ts` (`getSummary`): minggu berjalan
//! Senin–Minggu dalam UTC.

use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime};
use mz_core::time::{JsDate, now_naive};
use mz_core::{ApiResult, AppState};
use serde_json::{Value, json};

const DAY_NAMES: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

fn ms(dt: NaiveDateTime) -> i64 {
    dt.and_utc().timestamp_millis()
}

/// `getWeekBounds()`: Senin 00:00:00.000 .. Minggu 23:59:59.999 (UTC).
pub fn week_bounds(now: NaiveDateTime) -> (NaiveDateTime, NaiveDateTime) {
    let from_monday = i64::from(now.weekday().num_days_from_monday());
    let monday = now.date() - Duration::days(from_monday);
    let start = monday.and_hms_milli_opt(0, 0, 0, 0).unwrap_or_default();
    let end = (monday + Duration::days(6))
        .and_hms_milli_opt(23, 59, 59, 999)
        .unwrap_or_default();
    (start, end)
}

#[derive(sqlx::FromRow)]
struct Shift {
    id: i32,
    #[sqlx(rename = "clockInAt")]
    clock_in_at: NaiveDateTime,
    #[sqlx(rename = "clockOutAt")]
    clock_out_at: Option<NaiveDateTime>,
    #[sqlx(rename = "durationSeconds")]
    duration_seconds: Option<i32>,
    status: String,
}

#[derive(sqlx::FromRow)]
struct Event {
    uuid: String,
    title: String,
    description: Option<String>,
    location: Option<String>,
    #[sqlx(rename = "startAt")]
    start_at: JsDate,
    #[sqlx(rename = "endAt")]
    end_at: JsDate,
    #[sqlx(rename = "isAllDay")]
    is_all_day: bool,
    status: String,
    #[sqlx(rename = "htmlLink")]
    html_link: Option<String>,
}

/// Node: `getSummary` (:41).
pub async fn summary(state: &AppState, user_id: i32) -> ApiResult<Value> {
    let db = &state.db;
    let company_id = mz_core::company::require_company_id(db, user_id).await?;
    let now = now_naive();
    let (week_start, week_end) = week_bounds(now);

    let shifts = sqlx::query_as::<_, Shift>(
        r#"SELECT id, "clockInAt", "clockOutAt", "durationSeconds", status::text AS status
             FROM "mobileShift"
            WHERE "userId" = $1 AND "companyId" = $2 AND "clockInAt" >= $3 AND "clockInAt" <= $4
            ORDER BY "clockInAt" ASC"#,
    )
    .bind(user_id)
    .bind(company_id)
    .bind(week_start)
    .bind(week_end)
    .fetch_all(db);
    // Prisma `findFirst` tanpa `orderBy`.
    let limit = sqlx::query_as::<_, (Option<i32>, Option<i32>)>(
        r#"SELECT "dailyLimitSec", "weeklyLimitSec" FROM "userWorkTimeLimit"
            WHERE "userId" = $1 AND "companyId" = $2 AND status = 1 LIMIT 1"#,
    )
    .bind(user_id)
    .bind(company_id)
    .fetch_optional(db);
    // `reportDate` adalah kolom DATE: Prisma mengirim batasnya sebagai tanggal.
    let projects = sqlx::query_as::<_, (i32, i64)>(
        r#"SELECT "projectId", COALESCE(SUM("totalSeconds"), 0)::bigint
             FROM "dailyUserProjectSummary"
            WHERE "userId" = $1 AND "reportDate" >= $2 AND "reportDate" <= $3
            GROUP BY "projectId"
            ORDER BY SUM("totalSeconds") DESC
            LIMIT 5"#,
    )
    .bind(user_id)
    .bind(week_start.date())
    .bind(week_end.date())
    .fetch_all(db);
    let events = sqlx::query_as::<_, Event>(
        r#"SELECT uuid::text AS uuid, title, description, location, "startAt", "endAt", "isAllDay",
                  status, "htmlLink"
             FROM "googleCalendarEvent"
            WHERE "companyId" = $1 AND ("userId" = $2 OR "userId" IS NULL)
              AND "startAt" >= $3 AND status <> 'cancelled'
            ORDER BY "startAt" ASC
            LIMIT 20"#,
    )
    .bind(company_id)
    .bind(user_id)
    .bind(now)
    .fetch_all(db);
    let (shifts, limit, projects, events) = tokio::try_join!(shifts, limit, projects, events)?;

    let shift_ids: Vec<i32> = shifts.iter().map(|s| s.id).collect();
    let breaks: Vec<(i32, Option<i32>)> = if shift_ids.is_empty() {
        Vec::new()
    } else {
        sqlx::query_as(
            r#"SELECT "shiftId", "durationSeconds" FROM "mobileBreak"
                WHERE "shiftId" = ANY($1) AND status IN ('closed', 'exceeded')"#,
        )
        .bind(&shift_ids)
        .fetch_all(db)
        .await?
    };
    let (daily_limit, weekly_limit) = limit.unwrap_or((None, None));

    let monday: NaiveDate = week_start.date();
    let mut days = Vec::with_capacity(7);
    let mut totals = (0_i64, 0_i64, 0_i64);
    let mut today_check_in: Value = Value::Null;
    let today = now.date();
    for i in 0..7 {
        let date = monday + Duration::days(i);
        let day_start = date.and_hms_milli_opt(0, 0, 0, 0).unwrap_or_default();
        let day_end = date.and_hms_milli_opt(23, 59, 59, 999).unwrap_or_default();
        let day_shifts: Vec<&Shift> = shifts
            .iter()
            .filter(|s| s.clock_in_at >= day_start && s.clock_in_at <= day_end)
            .collect();
        let (mut work, mut brk) = (0_i64, 0_i64);
        for s in &day_shifts {
            if let Some(d) = s.duration_seconds {
                work += i64::from(d);
            } else if s.status == "active" {
                work += (ms(now) - ms(s.clock_in_at)).div_euclid(1000);
            }
            brk += breaks
                .iter()
                .filter(|(sid, _)| *sid == s.id)
                .map(|(_, d)| i64::from(d.unwrap_or(0)))
                .sum::<i64>();
        }
        let overtime = match daily_limit {
            Some(l) if work > i64::from(l) => work - i64::from(l),
            _ => 0,
        };
        let clock_in = day_shifts
            .first()
            .map_or(Value::Null, |s| json!(JsDate(s.clock_in_at)));
        let clock_out = day_shifts
            .last()
            .and_then(|s| s.clock_out_at)
            .map_or(Value::Null, |d| json!(JsDate(d)));
        if date == today {
            today_check_in = clock_in.clone();
        }
        totals.0 += work;
        totals.1 += brk;
        totals.2 += overtime;
        let dow = usize::try_from(date.weekday().num_days_from_sunday()).unwrap_or(0);
        days.push(json!({
            "date": date.format("%Y-%m-%d").to_string(),
            "dayName": DAY_NAMES.get(dow).copied().unwrap_or("Sunday"),
            "clockInAt": clock_in,
            "clockOutAt": clock_out,
            "workDurationSeconds": work,
            "breakDurationSeconds": brk,
            "overtimeSeconds": overtime,
        }));
    }
    let total_overtime = match weekly_limit {
        Some(l) if totals.0 > i64::from(l) => totals.0 - i64::from(l),
        _ => totals.2,
    };

    let ids: Vec<i32> = projects.iter().map(|(id, _)| *id).collect();
    let names: Vec<(i32, String, String)> = if ids.is_empty() {
        Vec::new()
    } else {
        sqlx::query_as(r#"SELECT id, name, uuid::text FROM project WHERE id = ANY($1)"#)
            .bind(&ids)
            .fetch_all(db)
            .await?
    };
    let top: Vec<Value> = projects
        .iter()
        .map(|(pid, total)| {
            let p = names.iter().find(|(id, _, _)| id == pid);
            json!({
                "projectUuid": p.map(|(_, _, u)| u.clone()),
                "projectName": p.map_or_else(|| "Unknown".to_owned(), |(_, n, _)| n.clone()),
                "totalSeconds": total,
            })
        })
        .collect();
    let upcoming: Vec<Value> = events
        .into_iter()
        .map(|e| {
            json!({
                "uuid": e.uuid,
                "title": e.title,
                "description": e.description,
                "location": e.location,
                "startAt": e.start_at,
                "endAt": e.end_at,
                "isAllDay": e.is_all_day,
                "status": e.status,
                "htmlLink": e.html_link,
            })
        })
        .collect();

    Ok(json!({
        "weekRange": {
            "start": week_start.date().format("%Y-%m-%d").to_string(),
            "end": week_end.date().format("%Y-%m-%d").to_string(),
        },
        "todayCheckInAt": today_check_in,
        "days": days,
        "weeklyTotals": {
            "totalWorkSeconds": totals.0,
            "totalBreakSeconds": totals.1,
            "totalOvertimeSeconds": total_overtime,
        },
        "topProjects": top,
        "upcomingEvents": upcoming,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn week_is_monday_to_sunday() {
        let at = |s: &str| mz_core::time::parse_js_date(s).unwrap_or_default();
        let (s, e) = week_bounds(at("2026-11-17T10:00:00Z")); // Selasa
        assert_eq!(JsDate(s).to_js_string(), "2026-11-16T00:00:00.000Z");
        assert_eq!(JsDate(e).to_js_string(), "2026-11-22T23:59:59.999Z");
        let (s, _) = week_bounds(at("2026-11-22T23:00:00Z")); // Minggu
        assert_eq!(JsDate(s).to_js_string(), "2026-11-16T00:00:00.000Z");
        let (s, _) = week_bounds(at("2026-11-16T00:00:00Z")); // Senin
        assert_eq!(JsDate(s).to_js_string(), "2026-11-16T00:00:00.000Z");
    }
}
