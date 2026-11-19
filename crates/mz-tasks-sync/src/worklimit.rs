//! `TasksService.handleCron` (`tasks/tasks.service.ts:88`), `checkWorkLimitsForFinishedDays` (:104),
//! `checkWorkLimits` (:2707), `calculateBreachPoint`, `notifyApprovers`, serta
//! `sendOvertimeAlertEmployee/Approver` (`mail/mail.service.ts:2116,2179`).

use std::collections::{BTreeMap, HashMap};

use chrono::{Datelike, Days, NaiveDate, NaiveDateTime, Utc, Weekday};
use mz_core::AppState;
use mz_core::jsfmt::to_fixed;
use mz_core::mail::{self, Mail};
use mz_core::time::now_naive;
use mz_timesheets::jobs::tz_or_utc;
use serde_json::json;
use sqlx::PgPool;

use crate::sync::run_sync_for_date;

/// `cronLocalDate(days)` (`date/cron-timezone.ts:18`): `YYYY-MM-DD` di Asia/Jakarta, digeser `days`.
pub fn cron_local_date(days: i64, now: chrono::DateTime<Utc>) -> NaiveDate {
    let ymd = now.with_timezone(&mz_jobs::CRON_TZ).date_naive();
    shift(ymd, days)
}

fn shift(d: NaiveDate, days: i64) -> NaiveDate {
    let n = Days::new(days.unsigned_abs());
    let r = if days >= 0 {
        d.checked_add_days(n)
    } else {
        d.checked_sub_days(n)
    };
    r.unwrap_or(d)
}

/// `companyLastFinishedDate(tz)` (`date/cron-timezone.ts:32`): kemarin lokal; zona tak dikenal = UTC.
pub fn company_last_finished_date(time_zone: &str, now: chrono::DateTime<Utc>) -> NaiveDate {
    shift(now.with_timezone(&tz_or_utc(time_zone)).date_naive(), -1)
}

/// Node: `tasks.service.ts:88` `handleCron`.
pub async fn handle_cron(state: AppState) -> anyhow::Result<()> {
    let now = Utc::now();
    run_sync_for_date(&state.db, cron_local_date(-2, now)).await;
    run_sync_for_date(&state.db, cron_local_date(-1, now)).await;
    check_work_limits_for_finished_days(&state, Utc::now()).await
}

/// Node: `tasks.service.ts:105` `checkWorkLimitsForFinishedDays`: tiap company dicek tepat satu
/// tanggal per run (hari terakhir yang sudah selesai di zonanya).
pub async fn check_work_limits_for_finished_days(
    state: &AppState,
    now: chrono::DateTime<Utc>,
) -> anyhow::Result<()> {
    let companies: Vec<(i32, String)> = sqlx::query_as(
        r#"SELECT c.id, c."timeZone" FROM company c
           WHERE EXISTS (SELECT 1 FROM "userWorkTimeLimit" l WHERE l."companyId" = c.id AND l.status = 1)
           ORDER BY c.id"#,
    )
    .fetch_all(&state.db)
    .await?;

    let mut by_date: BTreeMap<NaiveDate, Vec<i32>> = BTreeMap::new();
    for (id, tz) in companies {
        by_date
            .entry(company_last_finished_date(&tz, now))
            .or_default()
            .push(id);
    }
    for (date, ids) in by_date {
        if let Err(err) = check_work_limits(state, date, &ids).await {
            tracing::error!("[WorkLimit] Check failed for: {date}: {err}");
        }
    }
    Ok(())
}

/// `toLocaleDateString('en-GB', { day: '2-digit', month: 'short', year: 'numeric' })`
/// (ICU Node 20+: September = "Sept").
pub fn date_label_en_gb(d: NaiveDate) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sept", "Oct", "Nov", "Dec",
    ];
    let m = MONTHS.get(d.month0() as usize).copied().unwrap_or("");
    format!("{:02} {m} {}", d.day(), d.year())
}

/// Node: `calculateBreachPoint`: titik waktu akumulasi durasi pertama kali melewati `limit_sec`.
pub fn calculate_breach_point(
    timelines: &[(i32, NaiveDateTime, NaiveDateTime)],
    limit_sec: i64,
) -> Option<(NaiveDateTime, i32)> {
    let mut accumulated = 0_i64;
    for (id, start, end) in timelines {
        let duration = (*end - *start).num_milliseconds().div_euclid(1000);
        if duration <= 0 {
            continue;
        }
        let after = accumulated + duration;
        if after > limit_sec {
            let needed = limit_sec - accumulated;
            return Some((*start + chrono::Duration::seconds(needed), *id));
        }
        accumulated = after;
    }
    None
}

/// (name, username) per approver, di-cache per company dalam satu run.
type ApproverCache = HashMap<i32, Vec<(Option<String>, Option<String>)>>;

#[derive(Debug, sqlx::FromRow)]
struct LimitRow {
    id: i32,
    user_id: i32,
    company_id: i32,
    daily_limit_sec: Option<i32>,
    weekly_limit_sec: Option<i32>,
    user_name: Option<String>,
    username: Option<String>,
    company_name: Option<String>,
}

struct Alert<'a> {
    limit_type: &'a str,
    worked_hours: String,
    limit_hours: String,
    overtime_hours: String,
    date: String,
}

async fn load_timelines(
    db: &PgPool,
    user_id: i32,
    from: NaiveDate,
    to: NaiveDate,
) -> sqlx::Result<Vec<(i32, NaiveDateTime, NaiveDateTime)>> {
    sqlx::query_as(
        r#"SELECT tt.id, tt."startAt", tt."endAt" FROM "trackingTimeline" tt
           JOIN tracking t ON t.id = tt."trackingId"
           WHERE tt.status = 1 AND t."userId" = $1 AND t."takenDate" >= $2 AND t."takenDate" <= $3
           ORDER BY tt."startAt" ASC"#,
    )
    .bind(user_id)
    .bind(from)
    .bind(to)
    .fetch_all(db)
    .await
}

#[allow(clippy::too_many_arguments)]
async fn upsert_breach(
    db: &PgPool,
    l: &LimitRow,
    limit_type: &str,
    period: NaiveDate,
    breach_at: NaiveDateTime,
    timeline_id: Option<i32>,
    worked: i32,
    limit_sec: i32,
    overtime: i32,
) -> sqlx::Result<()> {
    sqlx::query(
        r#"INSERT INTO "workLimitBreach"
             ("userId", "companyId", "limitType", "periodDate", "breachAt", "breachTimelineId",
              "workedSec", "limitSec", "overtimeSec")
           VALUES ($1, $2, $3::"WorkLimitType", $4, $5, $6, $7, $8, $9)
           ON CONFLICT ("userId", "companyId", "limitType", "periodDate")
           DO UPDATE SET "breachAt" = EXCLUDED."breachAt",
                         "breachTimelineId" = EXCLUDED."breachTimelineId",
                         "workedSec" = EXCLUDED."workedSec",
                         "limitSec" = EXCLUDED."limitSec",
                         "overtimeSec" = EXCLUDED."overtimeSec",
                         "updatedAt" = $10"#,
    )
    .bind(l.user_id)
    .bind(l.company_id)
    .bind(limit_type)
    .bind(period)
    .bind(breach_at)
    .bind(timeline_id)
    .bind(worked)
    .bind(limit_sec)
    .bind(overtime)
    .bind(now_naive())
    .execute(db)
    .await?;
    Ok(())
}

/// `roleCodeFilter([ADMIN, MANAGER])` + `userAccessLevel.status = 1` (`notifyApprovers`).
async fn approvers(
    db: &PgPool,
    company_id: i32,
) -> sqlx::Result<Vec<(Option<String>, Option<String>)>> {
    sqlx::query_as(
        r#"SELECT u.name, u.username FROM "userAccessLevel" ual
           JOIN "accessLevel" al ON al.id = ual."accessLevelId"
           JOIN users u ON u.id = ual."userId"
           WHERE ual."companyId" = $1 AND ual.status = 1
             AND ((al."isSystem" = true AND al.code IN ('ADMIN', 'MANAGER'))
               OR (al."isSystem" = false AND al."baseRole"::text IN ('ADMIN', 'MANAGER')))
           ORDER BY ual.id"#,
    )
    .bind(company_id)
    .fetch_all(db)
    .await
}

/// `sendOvertimeAlertEmployee` (`mail.service.ts:2116`).
async fn send_employee(state: &AppState, l: &LimitRow, a: &Alert<'_>) {
    let (Some(email), name, company) = (
        l.username.as_deref(),
        l.user_name.as_deref().unwrap_or("-"),
        l.company_name.as_deref().unwrap_or("-"),
    ) else {
        return;
    };
    let subject = format!("{} Work Limit Exceeded \u{2014} {}", a.limit_type, a.date);
    let _ = mail::send(
        state,
        Mail {
            to_email: email,
            to_name: name,
            subject: &subject,
            text: None,
            template: "overtime-alert-employee",
            context: json!({
                "employeeName": name, "limitType": a.limit_type, "workedHours": a.worked_hours,
                "limitHours": a.limit_hours, "overtimeHours": a.overtime_hours,
                "date": a.date, "companyName": company,
            }),
            category: "Overtime Alert",
            label: "send overtime alert employee",
        },
    )
    .await;
}

/// `notifyApprovers` + `sendOvertimeAlertApprover` (`mail.service.ts:2179`); approver per company di-cache per run.
async fn notify_approvers(
    state: &AppState,
    l: &LimitRow,
    a: &Alert<'_>,
    cache: &mut ApproverCache,
) -> sqlx::Result<()> {
    if let std::collections::hash_map::Entry::Vacant(e) = cache.entry(l.company_id) {
        e.insert(approvers(&state.db, l.company_id).await?);
    }
    let employee = l.user_name.as_deref().unwrap_or("-");
    let company = l.company_name.as_deref().unwrap_or("-");
    for (name, username) in cache
        .get(&l.company_id)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        let Some(email) = username.as_deref().filter(|e| !e.is_empty()) else {
            continue;
        };
        let aname = name.as_deref().unwrap_or("-");
        let subject = format!(
            "[Staff Alert] {employee} Exceeded {} Work Limit",
            a.limit_type
        );
        let _ = mail::send(
            state,
            Mail {
                to_email: email,
                to_name: aname,
                subject: &subject,
                text: None,
                template: "overtime-alert-approver",
                context: json!({
                    "approverName": aname, "employeeName": employee, "limitType": a.limit_type,
                    "workedHours": a.worked_hours, "limitHours": a.limit_hours,
                    "overtimeHours": a.overtime_hours, "date": a.date, "companyName": company,
                }),
                category: "Overtime Alert Approver",
                label: "send overtime alert approver",
            },
        )
        .await;
    }
    Ok(())
}

fn hours(sec: i64) -> String {
    #[allow(clippy::cast_precision_loss)]
    to_fixed(sec as f64 / 3600.0, 2)
}

/// Node: `tasks.service.ts:2707` `checkWorkLimits(date, companyIds)`.
pub async fn check_work_limits(
    state: &AppState,
    date: NaiveDate,
    company_ids: &[i32],
) -> anyhow::Result<()> {
    let db = &state.db;
    tracing::info!(
        "[WorkLimit] Checking work limits for: {date} ({} company)",
        company_ids.len()
    );
    let is_monday = date.weekday() == Weekday::Mon;
    let prev_monday = shift(date, -7);
    let prev_sunday = shift(date, -1);
    let range_start = if is_monday { prev_monday } else { date };

    let breach_rows: Vec<(i32, i64, i64)> = sqlx::query_as(
        r#"SELECT
             l.id AS "limitId",
             COALESCE(SUM(s."totalSeconds") FILTER (WHERE s."reportDate" = $1::date), 0)::bigint AS "dailySec",
             COALESCE(SUM(s."totalSeconds") FILTER (WHERE s."reportDate" BETWEEN $2::date AND $3::date), 0)::bigint AS "weeklySec"
           FROM "userWorkTimeLimit" l
           LEFT JOIN "dailyUserProjectSummary" s
             ON s."userId" = l."userId"
            AND s."companyId" = l."companyId"
            AND s."reportDate" BETWEEN $4::date AND $1::date
           WHERE l.status = 1
             AND l."companyId" = ANY($5::int[])
           GROUP BY l.id
           HAVING (
               l."dailyLimitSec" IS NOT NULL
               AND COALESCE(SUM(s."totalSeconds") FILTER (WHERE s."reportDate" = $1::date), 0) > l."dailyLimitSec"
             )
             OR (
               $6::boolean
               AND l."weeklyLimitSec" IS NOT NULL
               AND COALESCE(SUM(s."totalSeconds") FILTER (WHERE s."reportDate" BETWEEN $2::date AND $3::date), 0) > l."weeklyLimitSec"
             )
           ORDER BY l.id"#,
    )
    .bind(date)
    .bind(prev_monday)
    .bind(prev_sunday)
    .bind(range_start)
    .bind(company_ids)
    .bind(is_monday)
    .fetch_all(db)
    .await?;

    if breach_rows.is_empty() {
        tracing::info!("[WorkLimit] Check completed for: {date}");
        return Ok(());
    }
    let sums: HashMap<i32, (i64, i64)> =
        breach_rows.iter().map(|(i, d, w)| (*i, (*d, *w))).collect();
    let ids: Vec<i32> = breach_rows.iter().map(|r| r.0).collect();

    // `status: 1` diulang (jendela TOCTOU sama seperti Node).
    let limits: Vec<LimitRow> = sqlx::query_as(
        r#"SELECT l.id, l."userId" AS user_id, l."companyId" AS company_id,
                  l."dailyLimitSec" AS daily_limit_sec, l."weeklyLimitSec" AS weekly_limit_sec,
                  u.name AS user_name, u.username AS username, c.name AS company_name
           FROM "userWorkTimeLimit" l
           LEFT JOIN users u ON u.id = l."userId"
           LEFT JOIN company c ON c.id = l."companyId"
           WHERE l.id = ANY($1::int[]) AND l.status = 1
           ORDER BY l.id"#,
    )
    .bind(&ids)
    .fetch_all(db)
    .await?;

    let mut cache = HashMap::new();
    let noon = |d: NaiveDate| d.and_hms_opt(12, 0, 0).unwrap_or_default();

    for l in &limits {
        if l.username.as_deref().is_none_or(str::is_empty) {
            continue;
        }
        let Some(&(daily_sec, weekly_sec)) = sums.get(&l.id) else {
            continue;
        };

        // ── DAILY ──
        if let Some(limit) = l.daily_limit_sec.filter(|lim| daily_sec > i64::from(*lim)) {
            let r: anyhow::Result<()> = async {
                let overtime = daily_sec - i64::from(limit);
                let tl = load_timelines(db, l.user_id, date, date).await?;
                let bp = calculate_breach_point(&tl, i64::from(limit));
                upsert_breach(
                    db,
                    l,
                    "DAILY",
                    date,
                    bp.map_or_else(|| noon(date), |b| b.0),
                    bp.map(|b| b.1),
                    i32::try_from(daily_sec)?,
                    limit,
                    i32::try_from(overtime)?,
                )
                .await?;
                let alert = Alert {
                    limit_type: "Daily",
                    worked_hours: hours(daily_sec),
                    limit_hours: hours(i64::from(limit)),
                    overtime_hours: hours(overtime),
                    date: date_label_en_gb(date),
                };
                tracing::info!(
                    "[WorkLimit] Daily overtime: user={}, worked={}h, limit={}h",
                    l.user_name.as_deref().unwrap_or("-"),
                    alert.worked_hours,
                    alert.limit_hours
                );
                send_employee(state, l, &alert).await;
                notify_approvers(state, l, &alert, &mut cache).await?;
                Ok(())
            }
            .await;
            if let Err(err) = r {
                tracing::error!(
                    "[WorkLimit] Daily check failed: limitId={}, userId={}: {err}",
                    l.id,
                    l.user_id
                );
            }
        }

        // ── WEEKLY (hanya Senin) ──
        if is_monday
            && let Some(limit) = l
                .weekly_limit_sec
                .filter(|lim| weekly_sec > i64::from(*lim))
        {
            let r: anyhow::Result<()> = async {
                let overtime = weekly_sec - i64::from(limit);
                let tl = load_timelines(db, l.user_id, prev_monday, prev_sunday).await?;
                let bp = calculate_breach_point(&tl, i64::from(limit));
                upsert_breach(
                    db,
                    l,
                    "WEEKLY",
                    prev_monday,
                    bp.map_or_else(|| noon(prev_sunday), |b| b.0),
                    bp.map(|b| b.1),
                    i32::try_from(weekly_sec)?,
                    limit,
                    i32::try_from(overtime)?,
                )
                .await?;
                let alert = Alert {
                    limit_type: "Weekly",
                    worked_hours: hours(weekly_sec),
                    limit_hours: hours(i64::from(limit)),
                    overtime_hours: hours(overtime),
                    date: format!(
                        "{} \u{2013} {}",
                        date_label_en_gb(prev_monday),
                        date_label_en_gb(prev_sunday)
                    ),
                };
                tracing::info!(
                    "[WorkLimit] Weekly overtime: user={}, worked={}h, limit={}h",
                    l.user_name.as_deref().unwrap_or("-"),
                    alert.worked_hours,
                    alert.limit_hours
                );
                send_employee(state, l, &alert).await;
                notify_approvers(state, l, &alert, &mut cache).await?;
                Ok(())
            }
            .await;
            if let Err(err) = r {
                tracing::error!(
                    "[WorkLimit] Weekly check failed: limitId={}, userId={}: {err}",
                    l.id,
                    l.user_id
                );
            }
        }
    }
    tracing::info!("[WorkLimit] Check completed for: {date}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn dt(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").unwrap_or_default()
    }
    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap_or_default()
    }

    #[test]
    fn breach_point_inside_second_segment() {
        let tl = vec![
            (1, dt("2026-11-10 01:00:00"), dt("2026-11-10 02:00:00")),
            (2, dt("2026-11-10 03:00:00"), dt("2026-11-10 05:00:00")),
        ];
        // limit 5400s: 3600 di segmen 1, sisa 1800s di segmen 2 -> 03:30
        assert_eq!(
            calculate_breach_point(&tl, 5400),
            Some((dt("2026-11-10 03:30:00"), 2))
        );
        assert_eq!(calculate_breach_point(&tl, 10800), None);
        assert_eq!(calculate_breach_point(&[], 1), None);
    }

    #[test]
    fn zero_or_negative_segments_skipped() {
        let tl = vec![
            (1, dt("2026-11-10 02:00:00"), dt("2026-11-10 01:00:00")),
            (2, dt("2026-11-10 03:00:00"), dt("2026-11-10 03:00:00")),
            (3, dt("2026-11-10 04:00:00"), dt("2026-11-10 04:00:10")),
        ];
        assert_eq!(
            calculate_breach_point(&tl, 5),
            Some((dt("2026-11-10 04:00:05"), 3))
        );
    }

    #[test]
    fn cron_dates_use_jakarta() {
        // 20:50 UTC 2026-11-16 = 03:50 WIB 2026-11-17
        let now = Utc
            .with_ymd_and_hms(2026, 11, 16, 20, 50, 0)
            .single()
            .unwrap_or_default();
        assert_eq!(cron_local_date(0, now), d("2026-11-17"));
        assert_eq!(cron_local_date(-1, now), d("2026-11-16"));
        assert_eq!(cron_local_date(-2, now), d("2026-11-15"));
    }

    #[test]
    fn company_finished_date_by_zone() {
        let now = Utc
            .with_ymd_and_hms(2026, 11, 16, 20, 50, 0)
            .single()
            .unwrap_or_default();
        assert_eq!(company_last_finished_date("UTC", now), d("2026-11-15"));
        assert_eq!(
            company_last_finished_date("Asia/Jakarta", now),
            d("2026-11-16")
        );
        assert_eq!(
            company_last_finished_date("America/New_York", now),
            d("2026-11-15")
        );
        assert_eq!(
            company_last_finished_date("Bukan/Zona", now),
            d("2026-11-15")
        );
        assert_eq!(company_last_finished_date("", now), d("2026-11-15"));
    }

    #[test]
    fn labels_and_hours() {
        assert_eq!(date_label_en_gb(d("2026-04-09")), "09 Apr 2026");
        assert_eq!(date_label_en_gb(d("2026-09-01")), "01 Sept 2026");
        assert_eq!(hours(450), "0.13"); // toFixed(2) JS membulatkan 0.125 ke atas
        assert_eq!(hours(3600), "1.00");
    }
}
