//! Dasbor JSON: `GET /leaves/report/summary` (`getLeaveAnalytics`, `leaves.service.ts:1156`)
//! dan `GET /leaves/overview` (`getOverview`, `:1445`). Semua tanggal = UTC (server Node `TZ=UTC`).

use std::collections::HashMap;

use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime};
use mz_core::{ApiResult, AppState};
use mz_leave::enums::LeaveSickMode;
use mz_leave::math::resolve_sick_mode;
use mz_leave::models::{BALANCE_COLUMNS, LeaveBalance};
use mz_leave::policy::{load_policy, policy_carry_over_expired, require_time_zone};
use serde_json::{Value, json};

use crate::dto::{AnalyticsQuery, OverviewQuery};
use crate::model::Visibility;
use crate::util::{fmt_date, fmt_date_time, invalid_date, js_local_date, month_label, round};

fn num(x: f64) -> Value {
    mz_core::jsfmt::num(x)
}

fn opt_num(x: Option<f64>) -> Value {
    x.map_or(Value::Null, num)
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct LeaveLite {
    id: i32,
    uuid: String,
    user_id: i32,
    leave_type: Option<String>,
    start_date: NaiveDate,
    end_date: NaiveDate,
    total_days: f64,
    status: String,
    created_at: NaiveDateTime,
    reviewed_at: Option<NaiveDateTime>,
    type_id: Option<i32>,
    type_name: Option<String>,
    user_name: Option<String>,
}

const LITE_SELECT: &str = r#"SELECT lr.id, lr.uuid::text AS uuid, lr."userId" AS user_id,
    lr."leaveType"::text AS leave_type, lr."startDate" AS start_date, lr."endDate" AS end_date,
    lr."totalDays" AS total_days, lr.status::text AS status, lr."createdAt" AS created_at,
    lr."reviewedAt" AS reviewed_at, lt.id AS type_id, lt.name AS type_name, u.name AS user_name
  FROM "leaveRequest" lr
  LEFT JOIN "companyLeaveType" lt ON lt.id = lr."leaveTypeId"
  LEFT JOIN users u ON u.id = lr."userId"
 WHERE lr."companyId" = "#;

impl LeaveLite {
    /// `leaveTypeConfig?.name ?? leaveType ?? fallback`.
    fn type_name(&self, fallback: &str) -> String {
        self.type_name
            .clone()
            .or_else(|| self.leave_type.clone())
            .unwrap_or_else(|| fallback.to_owned())
    }

    /// Kunci kelompok jenis: `cfg:<id>` / `legacy:<type|other>`.
    fn type_key(&self) -> String {
        match self.type_id {
            Some(id) => format!("cfg:{id}"),
            None => format!("legacy:{}", self.leave_type.as_deref().unwrap_or("other")),
        }
    }

    fn overlaps(&self, from: NaiveDate, to: NaiveDate) -> bool {
        self.start_date <= to && self.end_date >= from
    }
}

#[derive(Debug, sqlx::FromRow)]
struct EmployeeRow {
    user_id: i32,
    name: Option<String>,
    username: Option<String>,
}

/// Karyawan aktif company (`userCompany` status 1), urut nama user.
async fn employees(
    state: &AppState,
    company_id: i32,
    user_id: Option<i64>,
) -> sqlx::Result<Vec<EmployeeRow>> {
    let mut qb = sqlx::QueryBuilder::new(
        r#"SELECT uc."userId" AS user_id, u.name, u.username
             FROM "userCompany" uc LEFT JOIN users u ON u.id = uc."userId"
            WHERE uc."companyId" = "#,
    );
    qb.push_bind(company_id).push(" AND uc.status = 1");
    if let Some(u) = user_id {
        qb.push(r#" AND uc."userId" = "#).push_bind(u);
    }
    qb.push(r#" ORDER BY u.name ASC, uc.id ASC"#);
    qb.build_query_as().fetch_all(&state.db).await
}

/// `dailyUserProjectSummary.groupBy({ by: userId, _sum: totalSeconds })` dalam rentang tanggal.
async fn work_seconds(
    state: &AppState,
    company_id: i32,
    user_id: Option<i64>,
    from: NaiveDate,
    to: NaiveDate,
) -> sqlx::Result<HashMap<i32, f64>> {
    let mut qb = sqlx::QueryBuilder::new(
        r#"SELECT "userId", COALESCE(SUM("totalSeconds"), 0)::float8
             FROM "dailyUserProjectSummary" WHERE "companyId" = "#,
    );
    qb.push_bind(company_id);
    if let Some(u) = user_id {
        qb.push(r#" AND "userId" = "#).push_bind(u);
    }
    qb.push(r#" AND "reportDate" >= "#)
        .push_bind(from)
        .push(r#" AND "reportDate" <= "#)
        .push_bind(to)
        .push(r#" GROUP BY "userId""#);
    let rows: Vec<(i32, f64)> = qb.build_query_as().fetch_all(&state.db).await?;
    Ok(rows.into_iter().collect())
}

async fn balances(
    state: &AppState,
    company_id: i32,
    year: i32,
    user_id: Option<i64>,
) -> sqlx::Result<Vec<LeaveBalance>> {
    let mut qb = sqlx::QueryBuilder::new(format!(
        r#"SELECT {BALANCE_COLUMNS} FROM "leaveBalance" b WHERE b."companyId" = "#
    ));
    qb.push_bind(company_id)
        .push(" AND b.year = ")
        .push_bind(year);
    if let Some(u) = user_id {
        qb.push(r#" AND b."userId" = "#).push_bind(u);
    }
    qb.push(" ORDER BY b.id");
    qb.build_query_as().fetch_all(&state.db).await
}

fn year_i32(y: i64) -> ApiResult<i32> {
    i32::try_from(y).map_err(|_| invalid_date())
}

/// `getLeaveAnalytics(reviewerId, query)`.
#[allow(clippy::too_many_lines)]
pub async fn analytics(
    state: &AppState,
    reviewer_id: i32,
    company_id: i32,
    q: &AnalyticsQuery,
) -> ApiResult<Value> {
    let now = mz_core::time::now_naive();
    let year = q.year.unwrap_or_else(|| i64::from(now.year()));
    let is_current_year = year == i64::from(now.year());
    let workday_hours = q.workday_hours.unwrap_or(8.0);
    let year32 = year_i32(year)?;

    let period_start = NaiveDate::from_ymd_opt(year32, 1, 1).ok_or_else(invalid_date)?;
    let period_end_at = if is_current_year {
        now
    } else {
        NaiveDate::from_ymd_opt(year32, 12, 31)
            .ok_or_else(invalid_date)?
            .and_hms_milli_opt(23, 59, 59, 999)
            .ok_or_else(invalid_date)?
    };
    let period_end = period_end_at.date();
    let last_month: u32 = if is_current_year { now.month() } else { 12 };
    let user_scope = q.user_id;

    let vis = Visibility::load(&state.db, reviewer_id, company_id).await?;
    let mut qb = sqlx::QueryBuilder::new(LITE_SELECT);
    qb.push_bind(company_id);
    if let Some(u) = user_scope {
        qb.push(r#" AND lr."userId" = "#).push_bind(u);
    }
    qb.push(" AND ");
    vis.push(&mut qb, "lr");
    qb.push(r#" AND lr."startDate" <= "#)
        .push_bind(period_end)
        .push(r#" AND lr."endDate" >= "#)
        .push_bind(period_start)
        .push(" ORDER BY lr.id");
    let requests: Vec<LeaveLite> = qb.build_query_as().fetch_all(&state.db).await?;

    let emps = employees(state, company_id, user_scope).await?;
    let bals = balances(state, company_id, year32, user_scope).await?;
    let policy = load_policy(&state.db, company_id).await?;
    let work = work_seconds(state, company_id, user_scope, period_start, period_end).await?;

    // ── Status ──
    let count = |s: &str| requests.iter().filter(|r| r.status == s).count();
    let status_breakdown = json!({
        "pending": count("PENDING"),
        "approved": count("APPROVED"),
        "rejected": count("REJECTED"),
        "needsRevision": count("NEEDS_REVISION"),
        "cancelled": 0,
    });

    // ── Pemakaian per jenis (diajukan vs disetujui) ──
    struct TypeEntry {
        leave_type_id: Option<i32>,
        name: String,
        requested_count: i64,
        requested_days: f64,
        approved_count: i64,
        approved_days: f64,
    }
    let mut types: Vec<(String, TypeEntry)> = Vec::new();
    for r in &requests {
        let key = r.type_key();
        let idx = match types.iter().position(|(k, _)| *k == key) {
            Some(i) => i,
            None => {
                types.push((
                    key,
                    TypeEntry {
                        leave_type_id: r.type_id,
                        name: r.type_name("Lainnya"),
                        requested_count: 0,
                        requested_days: 0.0,
                        approved_count: 0,
                        approved_days: 0.0,
                    },
                ));
                types.len() - 1
            }
        };
        if let Some((_, e)) = types.get_mut(idx) {
            e.requested_count += 1;
            e.requested_days += r.total_days;
            if r.status == "APPROVED" {
                e.approved_count += 1;
                e.approved_days += r.total_days;
            }
        }
    }
    let mut type_list: Vec<TypeEntry> = types.into_iter().map(|(_, e)| e).collect();
    type_list.sort_by_key(|a| std::cmp::Reverse(a.requested_count));
    let usage_by_leave_type: Vec<Value> = type_list
        .iter()
        .map(|e| {
            json!({
                "leaveTypeId": e.leave_type_id,
                "name": e.name,
                "requestedCount": e.requested_count,
                "requestedDays": num(round(e.requested_days)),
                "approvedCount": e.approved_count,
                "approvedDays": num(round(e.approved_days)),
            })
        })
        .collect();

    // ── Orang cuti per bulan (disetujui, beririsan) ──
    let approved: Vec<&LeaveLite> = requests.iter().filter(|r| r.status == "APPROVED").collect();
    let mut monthly = Vec::new();
    for m in 1..=last_month {
        let m_start = NaiveDate::from_ymd_opt(year32, m, 1).ok_or_else(invalid_date)?;
        let m_end = js_local_date(year, i64::from(m), 0).ok_or_else(invalid_date)?;
        let in_month: Vec<&&LeaveLite> = approved
            .iter()
            .filter(|r| r.overlaps(m_start, m_end))
            .collect();
        let people: std::collections::HashSet<i32> = in_month.iter().map(|r| r.user_id).collect();
        monthly.push(json!({
            "month": m,
            "monthLabel": month_label(i64::from(m) - 1),
            "peopleCount": people.len(),
            "requestCount": in_month.len(),
            "totalDays": num(round(in_month.iter().map(|r| r.total_days).sum::<f64>())),
        }));
    }

    // ── Waktu proses (ajuan → keputusan) ──
    #[allow(clippy::cast_precision_loss)]
    let hours: Vec<f64> = requests
        .iter()
        .filter(|r| r.status == "APPROVED" || r.status == "REJECTED")
        .filter_map(|r| {
            r.reviewed_at
                .map(|at| (at - r.created_at).num_milliseconds() as f64 / 3_600_000.0)
        })
        .filter(|h| *h >= 0.0)
        .collect();
    #[allow(clippy::cast_precision_loss)]
    let approval_time = json!({
        "avgHours": opt_num((!hours.is_empty()).then(|| round(hours.iter().sum::<f64>() / hours.len() as f64))),
        "minHours": opt_num(hours.iter().copied().reduce(f64::min).map(round)),
        "maxHours": opt_num(hours.iter().copied().reduce(f64::max).map(round)),
        "decidedCount": hours.len(),
    });

    // ── Per karyawan ──
    let balance_by_user: HashMap<i32, &LeaveBalance> =
        bals.iter().map(|b| (b.user_id, b)).collect();
    let mut approved_days: HashMap<i32, f64> = HashMap::new();
    let mut requested_days: HashMap<i32, f64> = HashMap::new();
    for r in &requests {
        *requested_days.entry(r.user_id).or_default() += r.total_days;
        if r.status == "APPROVED" {
            *approved_days.entry(r.user_id).or_default() += r.total_days;
        }
    }
    let tz = require_time_zone(&state.db, company_id).await?;
    let carry_expired = policy_carry_over_expired(policy.as_ref(), year32, &tz, now);
    let sick_mode = resolve_sick_mode(policy.as_ref());
    let has_sick_balance = sick_mode != LeaveSickMode::DEDUCT_ANNUAL;
    let sick_limited = sick_mode == LeaveSickMode::LIMITED;

    let employee_reports: Vec<Value> = emps
        .iter()
        .map(|e| {
            let b = balance_by_user.get(&e.user_id).copied();
            let carry = match b {
                Some(b) if !carry_expired => (b.carry_over_days - b.carry_over_used).max(0.0),
                _ => 0.0,
            };
            let annual_remaining =
                b.map_or(0.0, |b| round(b.annual_leave_total - b.annual_leave_used + carry));
            let sick_remaining = b
                .filter(|_| sick_limited)
                .map(|b| round(b.sick_leave_total - b.sick_leave_used));
            let work_hours = round(work.get(&e.user_id).copied().unwrap_or(0.0) / 3600.0);
            let leave_days = round(approved_days.get(&e.user_id).copied().unwrap_or(0.0));
            let leave_hours = round(leave_days * workday_hours);
            let total_hours = work_hours + leave_hours;
            let pct = |x: f64| {
                if total_hours > 0.0 {
                    round(x / total_hours * 100.0)
                } else {
                    0.0
                }
            };
            json!({
                "userId": e.user_id,
                "name": e.name.clone().unwrap_or_else(|| "-".to_owned()),
                "username": e.username.clone().unwrap_or_else(|| "-".to_owned()),
                "balance": {
                    "annual": {
                        "total": num(b.map_or(0.0, |b| round(b.annual_leave_total))),
                        "used": num(b.map_or(0.0, |b| round(b.annual_leave_used))),
                        "carryOverAvailable": num(round(carry)),
                        "remaining": num(annual_remaining),
                    },
                    "sick": if has_sick_balance {
                        json!({
                            "mode": sick_mode.as_str(),
                            "total": num(b.filter(|_| sick_limited).map_or(0.0, |b| round(b.sick_leave_total))),
                            "used": num(b.map_or(0.0, |b| round(b.sick_leave_used))),
                            "remaining": opt_num(sick_remaining),
                        })
                    } else {
                        Value::Null
                    },
                },
                "leave": {
                    "requestedDays": num(round(requested_days.get(&e.user_id).copied().unwrap_or(0.0))),
                    "approvedDays": num(leave_days),
                },
                "comparison": {
                    "workHours": num(work_hours),
                    "leaveDays": num(leave_days),
                    "leaveHours": num(leave_hours),
                    "workPercent": num(pct(work_hours)),
                    "leavePercent": num(pct(leave_hours)),
                },
            })
        })
        .collect();

    Ok(json!({
        "year": year,
        "period": { "start": fmt_date(period_start), "end": fmt_date(period_end) },
        "generatedAt": fmt_date_time(Some(now)),
        "workdayHours": num(workday_hours),
        "statusBreakdown": status_breakdown,
        "approvalTime": approval_time,
        "usageByLeaveType": usage_by_leave_type,
        "monthlyOnLeave": monthly,
        "employees": employee_reports,
    }))
}

/// Inisial nama (`parts[0][0] + parts[last][0]`, huruf besar; nama kosong → `?`).
pub fn initials(name: &str) -> String {
    let parts: Vec<&str> = name.split_whitespace().collect();
    let first = |s: &str| s.chars().next().map(String::from).unwrap_or_default();
    let raw = match parts.as_slice() {
        [] => "?".to_owned(),
        [one] => {
            let c = first(one);
            if c.is_empty() { "?".to_owned() } else { c }
        }
        [head, .., last] => format!("{}{}", first(head), first(last)),
    };
    raw.to_uppercase()
}

/// `getOverview(userId, query)`.
#[allow(clippy::too_many_lines)]
pub async fn overview(
    state: &AppState,
    user_id: i32,
    company_id: i32,
    q: &OverviewQuery,
) -> ApiResult<Value> {
    let now = mz_core::time::now_naive();
    let today = now.date();
    let year = q.year.unwrap_or_else(|| i64::from(now.year()));
    let month = q.month.unwrap_or_else(|| i64::from(now.month()));
    let workday_hours = q.workday_hours.unwrap_or(8.0);
    let year32 = year_i32(year)?;

    let year_start = NaiveDate::from_ymd_opt(year32, 1, 1).ok_or_else(invalid_date)?;
    let year_end = NaiveDate::from_ymd_opt(year32, 12, 31).ok_or_else(invalid_date)?;
    let month_start = js_local_date(year, month - 1, 1).ok_or_else(invalid_date)?;
    let month_end = js_local_date(year, month, 0).ok_or_else(invalid_date)?;

    // Minggu berjalan Senin–Minggu (UTC).
    let days_from_mon = i64::from(today.weekday().num_days_from_monday());
    let week_start = today - Duration::days(days_from_mon);
    let week_end = week_start + Duration::days(6);
    // Mendesak = PENDING yang mulai ≤ 7 hari dari hari ini.
    let urgent_cutoff = today + Duration::days(7);

    // Kuartal sebelumnya (perbandingan approval rate).
    let current_quarter = now.month0() / 3 + 1;
    let prev_q = if current_quarter == 1 {
        4
    } else {
        current_quarter - 1
    };
    let prev_q_year = if current_quarter == 1 { year - 1 } else { year };
    let prev_q_month0 = i64::from((prev_q - 1) * 3);
    let prev_q_start = js_local_date(prev_q_year, prev_q_month0, 1)
        .ok_or_else(invalid_date)?
        .and_time(NaiveTime::MIN);
    let prev_q_end = js_local_date(prev_q_year, prev_q_month0 + 3, 0)
        .ok_or_else(invalid_date)?
        .and_hms_milli_opt(23, 59, 59, 999)
        .ok_or_else(invalid_date)?;

    let vis = Visibility::load(&state.db, user_id, company_id).await?;
    let scoped = |tail: &dyn Fn(&mut sqlx::QueryBuilder<'_, sqlx::Postgres>)| {
        let mut qb = sqlx::QueryBuilder::new(LITE_SELECT);
        qb.push_bind(company_id).push(" AND ");
        vis.push(&mut qb, "lr");
        tail(&mut qb);
        qb
    };

    let mut pending_q = scoped(&|qb| {
        qb.push(
            r#" AND lr.status = 'PENDING'::"LeaveStatus" ORDER BY lr."startDate" ASC, lr.id ASC"#,
        );
    });
    let pending: Vec<LeaveLite> = pending_q.build_query_as().fetch_all(&state.db).await?;

    let mut year_q = scoped(&|qb| {
        qb.push(r#" AND lr."startDate" <= "#)
            .push_bind(year_end)
            .push(r#" AND lr."endDate" >= "#)
            .push_bind(year_start)
            .push(" ORDER BY lr.id");
    });
    let year_requests: Vec<LeaveLite> = year_q.build_query_as().fetch_all(&state.db).await?;

    let mut week_q = sqlx::QueryBuilder::new(
        r#"SELECT COUNT(DISTINCT lr."userId") FROM "leaveRequest" lr WHERE lr."companyId" = "#,
    );
    week_q.push_bind(company_id).push(" AND ");
    vis.push(&mut week_q, "lr");
    week_q
        .push(r#" AND lr.status = 'APPROVED'::"LeaveStatus" AND lr."startDate" <= "#)
        .push_bind(week_end)
        .push(r#" AND lr."endDate" >= "#)
        .push_bind(week_start);
    let leaves_this_week: i64 = week_q.build_query_scalar().fetch_one(&state.db).await?;

    let total_employees: i64 = sqlx::query_scalar(
        r#"SELECT COUNT(*) FROM "userCompany" WHERE "companyId" = $1 AND status = 1"#,
    )
    .bind(company_id)
    .fetch_one(&state.db)
    .await?;
    let year_balances = balances(state, company_id, year32, None).await?;

    let mut prev_q_query = sqlx::QueryBuilder::new(
        r#"SELECT lr.status::text FROM "leaveRequest" lr WHERE lr."companyId" = "#,
    );
    prev_q_query.push_bind(company_id).push(" AND ");
    vis.push(&mut prev_q_query, "lr");
    prev_q_query
        .push(r#" AND lr.status IN ('APPROVED'::"LeaveStatus", 'REJECTED'::"LeaveStatus") AND lr."reviewedAt" >= "#)
        .push_bind(prev_q_start)
        .push(r#" AND lr."reviewedAt" <= "#)
        .push_bind(prev_q_end);
    let prev_q_decided: Vec<String> = prev_q_query
        .build_query_scalar()
        .fetch_all(&state.db)
        .await?;

    let month_work = work_seconds(state, company_id, None, month_start, month_end).await?;
    let emps = employees(state, company_id, None).await?;

    // ── Statistik ──
    let pending_count = pending.len();
    let urgent_count = pending
        .iter()
        .filter(|r| r.start_date <= urgent_cutoff)
        .count();
    let total_allocated: f64 = year_balances.iter().map(|b| b.annual_leave_total).sum();
    let total_used: f64 = year_balances.iter().map(|b| b.annual_leave_used).sum();
    let team_quota = if total_allocated > 0.0 {
        round(total_used / total_allocated * 100.0)
    } else {
        0.0
    };
    let decided: Vec<&LeaveLite> = year_requests
        .iter()
        .filter(|r| r.status == "APPROVED" || r.status == "REJECTED")
        .collect();
    let approved_year_count = decided.iter().filter(|r| r.status == "APPROVED").count();
    #[allow(clippy::cast_precision_loss)]
    let approval_rate = (!decided.is_empty())
        .then(|| round(approved_year_count as f64 / decided.len() as f64 * 100.0));
    let prev_approved = prev_q_decided.iter().filter(|s| *s == "APPROVED").count();
    #[allow(clippy::cast_precision_loss)]
    let prev_rate = (!prev_q_decided.is_empty())
        .then(|| round(prev_approved as f64 / prev_q_decided.len() as f64 * 100.0));
    let rate_change = approval_rate.zip(prev_rate).map(|(a, b)| round(a - b));

    // ── Daftar menunggu persetujuan (+ penanda bentrok) ──
    let active_pool: Vec<&LeaveLite> = year_requests
        .iter()
        .filter(|r| r.status == "PENDING" || r.status == "APPROVED")
        .collect();
    let pending_approvals: Vec<Value> = pending
        .iter()
        .map(|req| {
            let has_conflict = active_pool.iter().any(|r| {
                r.id != req.id
                    && r.user_id != req.user_id
                    && r.start_date <= req.end_date
                    && r.end_date >= req.start_date
            });
            json!({
                "uuid": req.uuid,
                "userId": req.user_id,
                "employeeName": req.user_name.clone().unwrap_or_else(|| "-".to_owned()),
                "employeeInitials": initials(req.user_name.as_deref().unwrap_or_default()),
                "leaveTypeName": req.type_name("-"),
                "startDate": fmt_date(req.start_date),
                "endDate": fmt_date(req.end_date),
                "totalDays": num(req.total_days),
                "hasConflict": has_conflict,
            })
        })
        .collect();

    // ── Pemakaian per jenis (disetujui, tahun terpilih) ──
    let mut usage: Vec<(String, Option<i32>, String, f64)> = Vec::new();
    for r in year_requests.iter().filter(|r| r.status == "APPROVED") {
        let key = r.type_key();
        match usage.iter_mut().find(|(k, ..)| *k == key) {
            Some(e) => e.3 += r.total_days,
            None => usage.push((key, r.type_id, r.type_name("Lainnya"), r.total_days)),
        }
    }
    let mut usage: Vec<(Option<i32>, String, f64)> = usage
        .into_iter()
        .map(|(_, id, name, days)| (id, name, round(days)))
        .collect();
    usage.sort_by(|a, b| b.2.total_cmp(&a.2));
    let total_approved_days = round(usage.iter().map(|e| e.2).sum());
    let usage_by_type = json!({
        "year": year,
        "totalDays": num(total_approved_days),
        "items": usage.iter().map(|(id, name, days)| json!({
            "leaveTypeId": id,
            "leaveTypeName": name,
            "days": num(*days),
            "percentage": num(if total_approved_days > 0.0 { round(days / total_approved_days * 100.0) } else { 0.0 }),
        })).collect::<Vec<_>>(),
    });

    // ── Kerja vs cuti bulan terpilih ──
    let mut leave_days: HashMap<i32, f64> = HashMap::new();
    for r in year_requests
        .iter()
        .filter(|r| r.status == "APPROVED" && r.overlaps(month_start, month_end))
    {
        *leave_days.entry(r.user_id).or_default() += r.total_days;
    }
    let work_vs_leave = json!({
        "year": year,
        "month": month,
        "monthLabel": month_label(month - 1),
        "employees": emps.iter().map(|e| {
            let days = leave_days.get(&e.user_id).copied().unwrap_or(0.0);
            json!({
                "userId": e.user_id,
                "name": e.name.clone().unwrap_or_else(|| "-".to_owned()),
                "workHours": num(round(month_work.get(&e.user_id).copied().unwrap_or(0.0) / 3600.0)),
                "leaveDays": num(round(days)),
                "leaveHours": num(round(days * workday_hours)),
            })
        }).collect::<Vec<_>>(),
    });

    Ok(json!({
        "stats": {
            "pendingCount": pending_count,
            "urgentCount": urgent_count,
            "leavesThisWeek": leaves_this_week,
            "totalEmployees": total_employees,
            "teamQuotaUsagePercent": num(team_quota),
            "quotaYear": year,
            "approvalRatePercent": opt_num(approval_rate),
            "approvalRatePrevQuarterPercent": opt_num(prev_rate),
            "approvalRateChangeVsPrevQuarter": opt_num(rate_change),
            "prevQuarterLabel": format!("Q{prev_q}"),
        },
        "pendingApprovals": pending_approvals,
        "usageByType": usage_by_type,
        "workVsLeaveThisMonth": work_vs_leave,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initials_like_node() {
        assert_eq!(initials("ani budi citra"), "AC");
        assert_eq!(initials("  ani "), "A");
        assert_eq!(initials(""), "?");
        assert_eq!(initials("élan"), "É");
    }
}
