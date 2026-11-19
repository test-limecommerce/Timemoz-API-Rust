//! Cron manager summary & scheduled reports (Fase 4a) + consumer Bull `report-dispatch`.
//!
//! Node: `tasks/tasks.service.ts` (`handleScheduledReports` :252, `handleManagerSummary` :3076) meng-enqueue
//! ke antrean `report-dispatch`; `tasks/report-dispatch.consumer.ts` (satu processor `*`, concurrency 1)
//! mengerjakannya satu per satu. Di Rust keduanya satu run cron: pencocokan slot -> `claim_once`
//! (pengganti `jobId`) -> dispatch berurutan (mutex proses `DISPATCH_LOCK` = concurrency 1 lintas dua cron).

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use anyhow::Context;
use chrono::{DateTime, Datelike, Months, NaiveDate, NaiveDateTime, Utc};
use chrono_tz::Tz;
use mz_core::AppState;
use mz_core::time::now_naive;
use mz_jobs::{CronJob, Schedule, claim_once};
use serde_json::{Map, Value, json};
use sqlx::PgPool;

use crate::mail::{SummaryKind, send_manager_summary};

/// `REPORT_DISPATCH_JOB_TIMEOUT_MS` (15 menit per job).
pub const REPORT_DISPATCH_JOB_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const DASHBOARD_URL: &str = "https://app.timemoz.com/dashboard";
const SETTINGS_URL: &str = "https://app.timemoz.com/settings/manager-summary";
/// Bull `attempts: 3`, `backoff: fixed 60000`.
const ATTEMPTS: u32 = 3;

/// Concurrency 1 antrean `report-dispatch` (dibagi dua cron).
static DISPATCH_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Jadwal `@Cron('0,30 * * * *')` kedua job (zona proses; slot per company dicocokkan di dalam job).
pub fn cron_jobs() -> Vec<CronJob> {
    vec![
        CronJob {
            name: "manager-summary",
            schedule: Schedule::hourly(&[0, 30]),
            timeout: Duration::from_secs(3600),
            run: |s| Box::pin(run_manager_summary(s)),
        },
        CronJob {
            name: "scheduled-reports",
            schedule: Schedule::hourly(&[0, 30]),
            timeout: Duration::from_secs(3600),
            run: |s| Box::pin(run_scheduled_reports(s)),
        },
    ]
}

// ───────────────────────── logika murni: waktu & slot ─────────────────────────

/// `company.timeZone || 'UTC'`; zona tak dikenal -> UTC (moment-timezone juga tidak melempar).
pub fn tz_or_utc(name: &str) -> Tz {
    if name.is_empty() {
        return chrono_tz::UTC;
    }
    name.parse().unwrap_or(chrono_tz::UTC)
}

/// `moment().tz(tz)`.
pub fn local_now(tz_name: &str, now: DateTime<Utc>) -> DateTime<Tz> {
    now.with_timezone(&tz_or_utc(tz_name))
}

/// `now.format('HH:mm') === sendTime`.
pub fn slot_matches(now: &DateTime<Tz>, send_time: &str) -> bool {
    now.format("%H:%M").to_string() == send_time
}

/// `moment.day()`: 0 = Minggu.
fn dow(now: &DateTime<Tz>) -> u32 {
    now.weekday().num_days_from_sunday()
}

fn ymd(d: NaiveDate) -> String {
    d.format("%Y-%m-%d").to_string()
}

/// `moment().diff(moment(last), 'hours')` (dipotong ke nol).
fn diff_hours(now: DateTime<Utc>, last: NaiveDateTime) -> i64 {
    (now.naive_utc() - last).num_hours()
}

fn yesterday(now: &DateTime<Tz>) -> NaiveDate {
    now.date_naive().pred_opt().unwrap_or(now.date_naive())
}

/// Job yang di-enqueue (payload job Bull + `jobId`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchJob {
    SummaryDaily {
        config_id: i32,
        report_date: String,
        report_date_formatted: String,
        subject_date: String,
        job_id: String,
    },
    SummaryWeekly {
        config_id: i32,
        start_date: String,
        end_date: String,
        start_date_formatted: String,
        end_date_formatted: String,
        job_id: String,
    },
    ScheduledReport {
        report_id: i32,
        start_date: String,
        end_date: String,
        job_id: String,
    },
}

impl DispatchJob {
    /// Padanan `jobId` Bull.
    pub fn job_id(&self) -> &str {
        match self {
            Self::SummaryDaily { job_id, .. }
            | Self::SummaryWeekly { job_id, .. }
            | Self::ScheduledReport { job_id, .. } => job_id,
        }
    }
    pub fn name(&self) -> &'static str {
        match self {
            Self::SummaryDaily { .. } => "manager-summary-daily",
            Self::SummaryWeekly { .. } => "manager-summary-weekly",
            Self::ScheduledReport { .. } => "scheduled-report",
        }
    }
}

/// Baris `managerSummaryConfig` + `company.timeZone` yang dibutuhkan enqueuer.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SummaryConfigRow {
    pub id: i32,
    pub is_daily_enabled: bool,
    pub is_weekly_enabled: bool,
    pub send_time: String,
    pub include_weekends: bool,
    pub last_daily_sent_at: Option<NaiveDateTime>,
    pub last_weekly_sent_at: Option<NaiveDateTime>,
    pub time_zone: String,
}

/// Isi loop `handleManagerSummary` untuk satu config.
pub fn plan_summary(cfg: &SummaryConfigRow, now_utc: DateTime<Utc>) -> Vec<DispatchJob> {
    let mut jobs = Vec::new();
    let now = local_now(&cfg.time_zone, now_utc);
    if !slot_matches(&now, &cfg.send_time) {
        return jobs;
    }
    let slot = ymd(now.date_naive());
    let d = dow(&now);
    let is_weekend = d == 0 || d == 6;
    let is_monday = d == 1;

    if cfg.is_daily_enabled && !(is_weekend && !cfg.include_weekends) {
        let should = cfg
            .last_daily_sent_at
            .is_none_or(|l| diff_hours(now_utc, l) >= 20);
        if should {
            let y = yesterday(&now);
            let f = y.format("%a, %-d %b %Y").to_string();
            jobs.push(DispatchJob::SummaryDaily {
                config_id: cfg.id,
                report_date: ymd(y),
                report_date_formatted: f.clone(),
                subject_date: f,
                job_id: format!("ms-daily:{}:{slot}", cfg.id),
            });
        }
    }
    if cfg.is_weekly_enabled && is_monday {
        let should = cfg
            .last_weekly_sent_at
            .is_none_or(|l| diff_hours(now_utc, l) >= 144);
        if should {
            let end = yesterday(&now);
            let start = end - chrono::Days::new(6);
            jobs.push(DispatchJob::SummaryWeekly {
                config_id: cfg.id,
                start_date: ymd(start),
                end_date: ymd(end),
                start_date_formatted: start.format("%-d %b").to_string(),
                end_date_formatted: end.format("%-d %b %Y").to_string(),
                job_id: format!("ms-weekly:{}:{slot}", cfg.id),
            });
        }
    }
    jobs
}

/// Baris `scheduledReport` + `company.timeZone`.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ReportRow {
    pub id: i32,
    pub send_time: String,
    pub frequency: String,
    pub days: Option<Value>,
    pub last_sent_at: Option<NaiveDateTime>,
    pub time_zone: String,
}

/// `shouldSendReport`.
pub fn should_send_report(frequency: &str, days: Option<&Value>, now: &DateTime<Tz>) -> bool {
    match frequency {
        "DAILY" => true,
        "WEEKLY" => {
            let list: Vec<i64> = days
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_i64).collect())
                .unwrap_or_default();
            list.is_empty() || list.contains(&i64::from(dow(now)))
        }
        "MONTHLY" => now.day() == 1,
        "YEARLY" => now.month() == 1 && now.day() == 1,
        _ => false,
    }
}

/// `minHoursFor` / ambang di enqueuer.
pub fn min_hours_for(frequency: &str) -> i64 {
    match frequency {
        "DAILY" => 20,
        "WEEKLY" => 144,
        "MONTHLY" => 600,
        _ => 8000,
    }
}

/// `getDateRange` -> (startDate, endDate) `YYYY-MM-DD`.
pub fn report_date_range(frequency: &str, now: &DateTime<Tz>) -> Option<(String, String)> {
    let today = now.date_naive();
    match frequency {
        "DAILY" => {
            let y = ymd(yesterday(now));
            Some((y.clone(), y))
        }
        "WEEKLY" => {
            let end = yesterday(now);
            Some((ymd(end - chrono::Days::new(6)), ymd(end)))
        }
        "MONTHLY" => {
            let prev = today.checked_sub_months(Months::new(1))?;
            let start = prev.with_day(1)?;
            let end = start.checked_add_months(Months::new(1))?.pred_opt()?;
            Some((ymd(start), ymd(end)))
        }
        "YEARLY" => {
            let year = today.year() - 1;
            Some((
                ymd(NaiveDate::from_ymd_opt(year, 1, 1)?),
                ymd(NaiveDate::from_ymd_opt(year, 12, 31)?),
            ))
        }
        _ => None,
    }
}

/// Isi loop `handleScheduledReports` untuk satu report.
pub fn plan_report(r: &ReportRow, now_utc: DateTime<Utc>) -> Option<DispatchJob> {
    let now = local_now(&r.time_zone, now_utc);
    if !slot_matches(&now, &r.send_time) {
        return None;
    }
    if !should_send_report(&r.frequency, r.days.as_ref(), &now) {
        return None;
    }
    if let Some(last) = r.last_sent_at
        && diff_hours(now_utc, last) < min_hours_for(&r.frequency)
    {
        return None;
    }
    let (start_date, end_date) = report_date_range(&r.frequency, &now)?;
    Some(DispatchJob::ScheduledReport {
        report_id: r.id,
        start_date,
        end_date,
        job_id: format!("scheduled-report:{}:{}", r.id, ymd(now.date_naive())),
    })
}

// ───────────────────────── pemilihan dari DB ─────────────────────────

/// `managerSummaryConfig.findMany(daily OR weekly)` + pencocokan slot. Urutan = `id`.
pub async fn due_summary_jobs(
    db: &PgPool,
    now_utc: DateTime<Utc>,
) -> sqlx::Result<Vec<DispatchJob>> {
    let rows: Vec<SummaryConfigRow> = sqlx::query_as(
        r#"SELECT m.id, m."isDailyEnabled" AS is_daily_enabled, m."isWeeklyEnabled" AS is_weekly_enabled,
                  m."sendTime" AS send_time, m."includeWeekends" AS include_weekends,
                  m."lastDailySentAt" AS last_daily_sent_at, m."lastWeeklySentAt" AS last_weekly_sent_at,
                  c."timeZone" AS time_zone
           FROM "managerSummaryConfig" m JOIN company c ON c.id = m."companyId"
           WHERE m."isDailyEnabled" OR m."isWeeklyEnabled" ORDER BY m.id"#,
    )
    .fetch_all(db)
    .await?;
    Ok(rows.iter().flat_map(|c| plan_summary(c, now_utc)).collect())
}

/// `scheduledReport.findMany(isActive)` + pencocokan slot/frekuensi/`lastSentAt`.
pub async fn due_report_jobs(
    db: &PgPool,
    now_utc: DateTime<Utc>,
) -> sqlx::Result<Vec<DispatchJob>> {
    let rows: Vec<ReportRow> = sqlx::query_as(
        r#"SELECT r.id, r."sendTime" AS send_time, r.frequency::text AS frequency, r.days,
                  r."lastSentAt" AS last_sent_at, c."timeZone" AS time_zone
           FROM "scheduledReport" r JOIN company c ON c.id = r."companyId"
           WHERE r."isActive" ORDER BY r.id"#,
    )
    .fetch_all(db)
    .await?;
    Ok(rows
        .iter()
        .filter_map(|r| plan_report(r, now_utc))
        .collect())
}

// ───────────────────────── pengumpulan data summary ─────────────────────────

/// Parameter config yang memengaruhi pengumpulan.
#[derive(Debug, Clone)]
pub struct SummaryParams {
    /// `Array.isArray(userIds) && length > 0 ? userIds : null`.
    pub user_ids: Option<Vec<i32>>,
    pub low_hours_threshold: i32,
    pub disabled_flags: Vec<String>,
    pub include_weekends: bool,
}

impl SummaryParams {
    /// Dari kolom `managerSummaryConfig` (`userIds` JSON → id angka bila array tak kosong,
    /// `disabledFlags` JSON → string). Dipakai cron di sini dan `mz-tasks-sync` (test kirim).
    pub fn from_config(
        user_ids: Option<&Value>,
        low_hours_threshold: i32,
        disabled_flags: Option<&Value>,
        include_weekends: bool,
    ) -> Self {
        let user_ids = user_ids
            .and_then(Value::as_array)
            .filter(|a| !a.is_empty())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_i64().and_then(|n| i32::try_from(n).ok()))
                    .collect()
            });
        Self {
            user_ids,
            low_hours_threshold,
            disabled_flags: disabled_flags
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default(),
            include_weekends,
        }
    }

    /// `config.lowHoursThreshold || 14400`.
    fn threshold(&self) -> i64 {
        if self.low_hours_threshold == 0 {
            14400
        } else {
            i64::from(self.low_hours_threshold)
        }
    }
    fn flag_on(&self, name: &str) -> bool {
        !self.disabled_flags.iter().any(|f| f == name)
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Member {
    pub id: i32,
    pub name: String,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct UserProjectRow {
    pub user_id: i32,
    pub user_name: String,
    pub project_id: i32,
    pub project_name: String,
    pub total_seconds: i32,
    pub report_date: NaiveDate,
}

/// `formatSeconds`.
pub fn format_seconds(seconds: i64) -> String {
    format!(
        "{}h {}m",
        seconds.div_euclid(3600),
        (seconds.rem_euclid(3600)) / 60
    )
}

/// `countWorkDays`.
pub fn count_work_days(start: NaiveDate, end: NaiveDate, include_weekends: bool) -> i64 {
    let mut count = 0;
    let mut cur = start;
    while cur <= end {
        let d = cur.weekday().num_days_from_sunday();
        if include_weekends || (d != 0 && d != 6) {
            count += 1;
        }
        match cur.succ_opt() {
            Some(n) => cur = n,
            None => break,
        }
    }
    count
}

struct UserAgg {
    total: i64,
    days: HashSet<NaiveDate>,
    /// (projectId, name, seconds) urut kemunculan (Map JS).
    projects: Vec<(i32, String, i64)>,
}

/// `userMap` Node: urutan masuk = urutan baris.
fn aggregate(rows: &[UserProjectRow]) -> (Vec<(i32, UserAgg)>, HashMap<i32, usize>) {
    let mut users: Vec<(i32, UserAgg)> = Vec::new();
    let mut index: HashMap<i32, usize> = HashMap::new();
    for r in rows {
        let i = *index.entry(r.user_id).or_insert_with(|| {
            users.push((
                r.user_id,
                UserAgg {
                    total: 0,
                    days: HashSet::new(),
                    projects: Vec::new(),
                },
            ));
            users.len() - 1
        });
        let Some((_, u)) = users.get_mut(i) else {
            continue;
        };
        let secs = i64::from(r.total_seconds);
        u.total += secs;
        u.days.insert(r.report_date);
        match u.projects.iter_mut().find(|p| p.0 == r.project_id) {
            Some(p) => p.2 += secs,
            None => u
                .projects
                .push((r.project_id, r.project_name.clone(), secs)),
        }
    }
    (users, index)
}

/// `sort((a,b) => b.seconds - a.seconds)[0]` (stabil: yang pertama menang saat seri).
fn top_by_seconds(items: &[(String, i64)]) -> Option<(String, i64)> {
    let mut best: Option<&(String, i64)> = None;
    for it in items {
        if best.is_none_or(|b| it.1 > b.1) {
            best = Some(it);
        }
    }
    best.cloned()
}

fn project_totals(users: &[(i32, UserAgg)]) -> Vec<(String, i64)> {
    let mut order: Vec<(i32, String, i64)> = Vec::new();
    for (_, u) in users {
        for (pid, name, secs) in &u.projects {
            match order.iter_mut().find(|p| p.0 == *pid) {
                Some(p) => p.2 += secs,
                None => order.push((*pid, name.clone(), *secs)),
            }
        }
    }
    order.into_iter().map(|(_, n, s)| (n, s)).collect()
}

struct TeamRow {
    name: String,
    hours: String,
    primary_project: String,
    activity: &'static str,
    total_seconds: i64,
}

impl TeamRow {
    fn json(&self) -> Value {
        json!({
            "name": self.name,
            "hours": self.hours,
            "primaryProject": self.primary_project,
            "activity": self.activity,
            "isGreen": self.activity == "green",
            "isYellow": self.activity == "yellow",
            "isRed": self.activity == "red",
            "totalSeconds": self.total_seconds,
        })
    }
}

fn build_team_rows(
    members: &[Member],
    users: &[(i32, UserAgg)],
    index: &HashMap<i32, usize>,
    threshold: i64,
) -> Vec<TeamRow> {
    let mut rows: Vec<TeamRow> = members
        .iter()
        .map(|m| {
            let agg = index.get(&m.id).and_then(|i| users.get(*i)).map(|(_, u)| u);
            let total = agg.map_or(0, |u| u.total);
            let primary = agg.and_then(|u| {
                let items: Vec<(String, i64)> =
                    u.projects.iter().map(|p| (p.1.clone(), p.2)).collect();
                top_by_seconds(&items)
            });
            let activity = if total == 0 {
                "red"
            } else if total < threshold {
                "yellow"
            } else {
                "green"
            };
            TeamRow {
                name: m.name.clone(),
                hours: format_seconds(total),
                primary_project: primary.map_or_else(|| "\u{2014}".to_owned(), |p| p.0),
                activity,
                total_seconds: total,
            }
        })
        .collect();
    // order = { red: 2, yellow: 1, green: 0 }; a - b menaik, lalu totalSeconds menurun.
    let ord = |a: &str| match a {
        "red" => 2,
        "yellow" => 1,
        _ => 0,
    };
    rows.sort_by(|a, b| {
        ord(a.activity)
            .cmp(&ord(b.activity))
            .then(b.total_seconds.cmp(&a.total_seconds))
    });
    rows
}

fn top_project_json(top: Option<(String, i64)>) -> Value {
    top.map_or(
        Value::Null,
        |(n, s)| json!({"name": n, "hours": format_seconds(s)}),
    )
}

fn finish_rows(
    rows: &[TeamRow],
    flags: Vec<Value>,
    mut out: Map<String, Value>,
) -> Map<String, Value> {
    out.insert(
        "teamRows".into(),
        Value::Array(rows.iter().take(15).map(TeamRow::json).collect()),
    );
    out.insert("teamRowsExtra".into(), json!(rows.len().saturating_sub(15)));
    out.insert("hasFlags".into(), json!(!flags.is_empty()));
    out.insert("flags".into(), Value::Array(flags));
    out
}

/// Inti `collectDailySummaryData` (murni). `members` kosong ditangani pemanggil.
pub fn build_daily(
    members: &[Member],
    rows: &[UserProjectRow],
    p: &SummaryParams,
    report_date: &str,
) -> Map<String, Value> {
    let (users, index) = aggregate(rows);
    let total_all: i64 = users.iter().map(|(_, u)| u.total).sum();
    let threshold = p.threshold();
    let team = build_team_rows(members, &users, &index, threshold);

    let mut flags = Vec::new();
    if p.flag_on("didnt_track") {
        let names: Vec<&str> = team
            .iter()
            .filter(|r| r.total_seconds == 0)
            .map(|r| r.name.as_str())
            .collect();
        if !names.is_empty() {
            flags.push(json!({"type": "didnt_track", "text": format!("Didn't track: {}", names.join(", "))}));
        }
    }
    if p.flag_on("low_hours") {
        let names: Vec<String> = team
            .iter()
            .filter(|r| r.total_seconds > 0 && r.total_seconds < threshold)
            .map(|r| format!("{} ({})", r.name, r.hours))
            .collect();
        if !names.is_empty() {
            flags.push(
                json!({"type": "low_hours", "text": format!("Low hours: {}", names.join(", "))}),
            );
        }
    }

    let mut out = Map::new();
    out.insert("isEmpty".into(), json!(false));
    out.insert("noMembers".into(), json!(false));
    out.insert("noActivity".into(), json!(total_all == 0));
    out.insert("reportDate".into(), json!(report_date));
    out.insert("totalHours".into(), json!(format_seconds(total_all)));
    out.insert("activeMembers".into(), json!(users.len()));
    out.insert("totalMembers".into(), json!(members.len()));
    out.insert(
        "topProject".into(),
        top_project_json(top_by_seconds(&project_totals(&users))),
    );
    finish_rows(&team, flags, out)
}

/// Inti `collectWeeklySummaryData` (murni).
pub fn build_weekly(
    members: &[Member],
    rows: &[UserProjectRow],
    prev_week_seconds: i64,
    p: &SummaryParams,
    start: NaiveDate,
    end: NaiveDate,
) -> Map<String, Value> {
    let (users, index) = aggregate(rows);
    let total_work_days = count_work_days(start, end, p.include_weekends);
    let total_this: i64 = users.iter().map(|(_, u)| u.total).sum();

    let week_over_week = (prev_week_seconds > 0).then(|| {
        #[allow(clippy::cast_precision_loss)]
        let pct = (total_this - prev_week_seconds) as f64 / prev_week_seconds as f64 * 100.0;
        let dir = if pct >= 0.0 { "up" } else { "down" };
        // Math.round(abs) = floor(x + 0.5)
        format!("{dir} {}% from last week", (pct.abs() + 0.5).floor())
    });

    let weekly_threshold = p.threshold() * total_work_days;
    let team = build_team_rows(members, &users, &index, weekly_threshold);

    let mut flags = Vec::new();
    if p.flag_on("didnt_track") {
        let mut missed_days = Vec::new();
        for m in members {
            let tracked = index
                .get(&m.id)
                .and_then(|i| users.get(*i))
                .map_or(0, |(_, u)| i64::try_from(u.days.len()).unwrap_or(0));
            let missed = total_work_days - tracked;
            if missed > 0 {
                missed_days.push(if missed == total_work_days {
                    m.name.clone()
                } else {
                    format!("{} ({missed} days)", m.name)
                });
            }
        }
        if !missed_days.is_empty() {
            flags.push(json!({"type": "didnt_track", "text": format!("Didn't track: {}", missed_days.join(", "))}));
        }
    }
    if p.flag_on("low_hours") {
        let names: Vec<String> = team
            .iter()
            .filter(|r| r.total_seconds > 0 && r.total_seconds < weekly_threshold)
            .map(|r| format!("{} ({})", r.name, r.hours))
            .collect();
        if !names.is_empty() {
            flags.push(
                json!({"type": "low_hours", "text": format!("Low hours: {}", names.join(", "))}),
            );
        }
    }

    let mut out = Map::new();
    out.insert("isEmpty".into(), json!(false));
    out.insert("noMembers".into(), json!(false));
    out.insert("noActivity".into(), json!(total_this == 0));
    out.insert("startDate".into(), json!(ymd(start)));
    out.insert("endDate".into(), json!(ymd(end)));
    out.insert("totalHours".into(), json!(format_seconds(total_this)));
    out.insert("weekOverWeekChange".into(), json!(week_over_week));
    out.insert("activeMembers".into(), json!(users.len()));
    out.insert("totalMembers".into(), json!(members.len()));
    out.insert(
        "topProject".into(),
        top_project_json(top_by_seconds(&project_totals(&users))),
    );
    finish_rows(&team, flags, out)
}

/// Hasil pengumpulan: `{ isEmpty: true, noMembers: true }` atau data.
#[derive(Debug, Clone, PartialEq)]
pub enum Collected {
    NoMembers,
    Data(Map<String, Value>),
}

async fn members(
    db: &PgPool,
    company_id: i32,
    user_ids: Option<&Vec<i32>>,
) -> sqlx::Result<Vec<Member>> {
    sqlx::query_as(
        r#"SELECT u.id, u.name FROM "userCompany" uc JOIN users u ON u.id = uc."userId"
           WHERE uc."companyId" = $1 AND uc.status = 1
             AND ($2::int[] IS NULL OR uc."userId" = ANY($2))
           ORDER BY uc.id"#,
    )
    .bind(company_id)
    .bind(user_ids)
    .fetch_all(db)
    .await
}

async fn user_project_rows(
    db: &PgPool,
    company_id: i32,
    from: NaiveDate,
    to: NaiveDate,
    user_ids: &[i32],
) -> sqlx::Result<Vec<UserProjectRow>> {
    sqlx::query_as(
        r#"SELECT d."userId" AS user_id, u.name AS user_name, d."projectId" AS project_id,
                  p.name AS project_name, d."totalSeconds" AS total_seconds, d."reportDate" AS report_date
           FROM "dailyUserProjectSummary" d
           JOIN users u ON u.id = d."userId" JOIN project p ON p.id = d."projectId"
           WHERE d."companyId" = $1 AND d."reportDate" BETWEEN $2 AND $3 AND d."userId" = ANY($4)
           ORDER BY d.id"#,
    )
    .bind(company_id)
    .bind(from)
    .bind(to)
    .bind(user_ids)
    .fetch_all(db)
    .await
}

fn parse_day(s: &str) -> anyhow::Result<NaiveDate> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").with_context(|| format!("tanggal tidak valid: {s}"))
}

/// Node: `ManagerSummaryService.collectDailySummaryData`.
pub async fn collect_daily_summary(
    db: &PgPool,
    company_id: i32,
    report_date: &str,
    p: &SummaryParams,
) -> anyhow::Result<Collected> {
    let day = parse_day(report_date)?;
    let m = members(db, company_id, p.user_ids.as_ref()).await?;
    if m.is_empty() {
        return Ok(Collected::NoMembers);
    }
    let ids: Vec<i32> = m.iter().map(|x| x.id).collect();
    let rows = user_project_rows(db, company_id, day, day, &ids).await?;
    Ok(Collected::Data(build_daily(&m, &rows, p, report_date)))
}

/// Node: `ManagerSummaryService.collectWeeklySummaryData`.
pub async fn collect_weekly_summary(
    db: &PgPool,
    company_id: i32,
    start_date: &str,
    end_date: &str,
    p: &SummaryParams,
) -> anyhow::Result<Collected> {
    let (start, end) = (parse_day(start_date)?, parse_day(end_date)?);
    let m = members(db, company_id, p.user_ids.as_ref()).await?;
    if m.is_empty() {
        return Ok(Collected::NoMembers);
    }
    let ids: Vec<i32> = m.iter().map(|x| x.id).collect();
    let rows = user_project_rows(db, company_id, start, end, &ids).await?;
    let prev_start = start - chrono::Days::new(7);
    let prev_end = end - chrono::Days::new(7);
    let prev: i64 = sqlx::query_scalar(
        r#"SELECT COALESCE(SUM("totalSeconds"), 0)::bigint FROM "dailyCompanySummary"
           WHERE "companyId" = $1 AND "reportDate" BETWEEN $2 AND $3"#,
    )
    .bind(company_id)
    .bind(prev_start)
    .bind(prev_end)
    .fetch_one(db)
    .await?;
    Ok(Collected::Data(build_weekly(
        &m, &rows, prev, p, start, end,
    )))
}

// ───────────────────────── isi email ─────────────────────────

/// Konteks template: `{ companyName, ...labelTanggal, ...data, dashboardUrl, settingsUrl }`.
pub fn mail_context(
    company_name: &str,
    labels: &[(&str, &str)],
    data: &Map<String, Value>,
) -> Value {
    let mut ctx = Map::new();
    ctx.insert("companyName".into(), json!(company_name));
    for (k, v) in labels {
        ctx.insert((*k).to_owned(), json!(v));
    }
    for (k, v) in data {
        ctx.insert(k.clone(), v.clone());
    }
    ctx.insert("dashboardUrl".into(), json!(DASHBOARD_URL));
    ctx.insert("settingsUrl".into(), json!(SETTINGS_URL));
    Value::Object(ctx)
}

// ───────────────────────── consumer ─────────────────────────

#[derive(Debug, sqlx::FromRow)]
struct SummaryConfigFull {
    id: i32,
    company_id: i32,
    is_daily_enabled: bool,
    is_weekly_enabled: bool,
    include_weekends: bool,
    emails: Value,
    user_ids: Option<Value>,
    low_hours_threshold: i32,
    disabled_flags: Option<Value>,
    last_daily_sent_at: Option<NaiveDateTime>,
    last_weekly_sent_at: Option<NaiveDateTime>,
    company_name: String,
}

impl SummaryConfigFull {
    fn params(&self) -> SummaryParams {
        SummaryParams::from_config(
            self.user_ids.as_ref(),
            self.low_hours_threshold,
            self.disabled_flags.as_ref(),
            self.include_weekends,
        )
    }
    fn email_list(&self) -> Vec<String> {
        self.emails
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    }
}

async fn load_config(db: &PgPool, id: i32) -> sqlx::Result<Option<SummaryConfigFull>> {
    sqlx::query_as(
        r#"SELECT m.id, m."companyId" AS company_id, m."isDailyEnabled" AS is_daily_enabled,
                  m."isWeeklyEnabled" AS is_weekly_enabled, m."includeWeekends" AS include_weekends,
                  m.emails, m."userIds" AS user_ids, m."lowHoursThreshold" AS low_hours_threshold,
                  m."disabledFlags" AS disabled_flags, m."lastDailySentAt" AS last_daily_sent_at,
                  m."lastWeeklySentAt" AS last_weekly_sent_at, c.name AS company_name
           FROM "managerSummaryConfig" m JOIN company c ON c.id = m."companyId" WHERE m.id = $1"#,
    )
    .bind(id)
    .fetch_optional(db)
    .await
}

/// `Math.abs(hoursSince) < min` (selisih pecahan, dua arah).
fn sent_recently(last: Option<NaiveDateTime>, min_hours: i64) -> bool {
    last.is_some_and(|l| {
        let secs = (Utc::now().naive_utc() - l).num_seconds().abs();
        secs < min_hours * 3600
    })
}

/// Node: `report-dispatch.consumer.ts:156` `handleManagerSummaryDaily`.
pub async fn handle_summary_daily(
    state: &AppState,
    config_id: i32,
    report_date: &str,
    report_date_formatted: &str,
    subject_date: &str,
) -> anyhow::Result<()> {
    let Some(cfg) = load_config(&state.db, config_id).await? else {
        return Ok(());
    };
    if !cfg.is_daily_enabled {
        return Ok(());
    }
    if sent_recently(cfg.last_daily_sent_at, 20) {
        tracing::warn!(company = %cfg.company_name, "Skip daily summary: already sent recently");
        return Ok(());
    }
    let data =
        match collect_daily_summary(&state.db, cfg.company_id, report_date, &cfg.params()).await? {
            Collected::NoMembers => return Ok(()),
            Collected::Data(d) => d,
        };
    let emails = cfg.email_list();
    if emails.is_empty() {
        tracing::warn!(company = %cfg.company_name, "Daily summary skipped: no recipients");
        return Ok(());
    }
    let ctx = mail_context(
        &cfg.company_name,
        &[
            ("reportDateFormatted", report_date_formatted),
            ("subjectDate", subject_date),
        ],
        &data,
    );
    if send_manager_summary(state, SummaryKind::Daily, &emails, &ctx).await {
        sqlx::query(r#"UPDATE "managerSummaryConfig" SET "lastDailySentAt" = $1 WHERE id = $2"#)
            .bind(now_naive())
            .bind(cfg.id)
            .execute(&state.db)
            .await?;
        tracing::info!(company = %cfg.company_name, "Daily summary sent");
    } else {
        tracing::error!(company = %cfg.company_name, "Daily summary failed");
    }
    Ok(())
}

/// Node: `report-dispatch.consumer.ts:213` `handleManagerSummaryWeekly`.
pub async fn handle_summary_weekly(
    state: &AppState,
    config_id: i32,
    start_date: &str,
    end_date: &str,
    start_formatted: &str,
    end_formatted: &str,
) -> anyhow::Result<()> {
    let Some(cfg) = load_config(&state.db, config_id).await? else {
        return Ok(());
    };
    if !cfg.is_weekly_enabled {
        return Ok(());
    }
    if sent_recently(cfg.last_weekly_sent_at, 144) {
        tracing::warn!(company = %cfg.company_name, "Skip weekly summary: already sent recently");
        return Ok(());
    }
    let data = match collect_weekly_summary(
        &state.db,
        cfg.company_id,
        start_date,
        end_date,
        &cfg.params(),
    )
    .await?
    {
        Collected::NoMembers => return Ok(()),
        Collected::Data(d) => d,
    };
    let emails = cfg.email_list();
    if emails.is_empty() {
        tracing::warn!(company = %cfg.company_name, "Weekly summary skipped: no recipients");
        return Ok(());
    }
    let ctx = mail_context(
        &cfg.company_name,
        &[
            ("startDateFormatted", start_formatted),
            ("endDateFormatted", end_formatted),
        ],
        &data,
    );
    if send_manager_summary(state, SummaryKind::Weekly, &emails, &ctx).await {
        sqlx::query(r#"UPDATE "managerSummaryConfig" SET "lastWeeklySentAt" = $1 WHERE id = $2"#)
            .bind(now_naive())
            .bind(cfg.id)
            .execute(&state.db)
            .await?;
        tracing::info!(company = %cfg.company_name, "Weekly summary sent");
    } else {
        tracing::error!(company = %cfg.company_name, "Weekly summary failed");
    }
    Ok(())
}

#[derive(Debug, sqlx::FromRow)]
struct ReportFull {
    id: i32,
    name: String,
    frequency: String,
    emails: Value,
    subject: String,
    message: String,
    user_ids: Option<Value>,
    project_ids: Option<Value>,
    sections: Option<Value>,
    is_active: bool,
    last_sent_at: Option<NaiveDateTime>,
    created_by_id: i32,
}

/// Payload `SendReportDto` dari baris report (`|| []`, `|| ['company','project','user']`).
#[allow(clippy::too_many_arguments)]
pub fn report_payload(
    r_emails: &Value,
    subject: &str,
    message: &str,
    start: &str,
    end: &str,
    user_ids: Option<&Value>,
    project_ids: Option<&Value>,
    sections: Option<&Value>,
) -> Value {
    let arr_or_empty = |v: Option<&Value>| match v {
        Some(Value::Array(a)) => Value::Array(a.clone()),
        _ => json!([]),
    };
    json!({
        "emails": r_emails,
        "subject": subject,
        "message": message,
        "startDate": start,
        "endDate": end,
        "userIds": arr_or_empty(user_ids),
        "projectIds": arr_or_empty(project_ids),
        "sections": match sections {
            Some(Value::Array(a)) => Value::Array(a.clone()),
            _ => json!(["company", "project", "user"]),
        },
    })
}

/// Node: `report-dispatch.consumer.ts:99` `handleScheduledReport`.
pub async fn handle_scheduled_report(
    state: &AppState,
    report_id: i32,
    start_date: &str,
    end_date: &str,
) -> anyhow::Result<()> {
    let report: Option<ReportFull> = sqlx::query_as(
        r#"SELECT id, name, frequency::text AS frequency, emails, subject, message,
                  "userIds" AS user_ids, "projectIds" AS project_ids, sections,
                  "isActive" AS is_active, "lastSentAt" AS last_sent_at, "createdById" AS created_by_id
           FROM "scheduledReport" WHERE id = $1"#,
    )
    .bind(report_id)
    .fetch_optional(&state.db)
    .await?;
    let Some(report) = report.filter(|r| r.is_active) else {
        return Ok(());
    };
    if sent_recently(report.last_sent_at, min_hours_for(&report.frequency)) {
        tracing::warn!(report = %report.name, "Skip scheduled report: sent recently");
        return Ok(());
    }
    let payload = report_payload(
        &report.emails,
        &report.subject,
        &report.message,
        start_date,
        end_date,
        report.user_ids.as_ref(),
        report.project_ids.as_ref(),
        report.sections.as_ref(),
    );
    let dto: mz_screenshots::reports::dto::SendReportDto =
        serde_json::from_value(payload).context("payload scheduled report")?;
    // Node meng-`await` sendReport tanpa memeriksa `success`; hanya HttpException yang melempar.
    mz_screenshots::reports::service::send_report(state, report.created_by_id, &dto)
        .await
        .map_err(|e| anyhow::anyhow!("sendReport gagal: {e:?}"))?;
    sqlx::query(r#"UPDATE "scheduledReport" SET "lastSentAt" = $1 WHERE id = $2"#)
        .bind(now_naive())
        .bind(report.id)
        .execute(&state.db)
        .await?;
    tracing::info!(report = %report.name, "Scheduled report sent successfully");
    Ok(())
}

/// Pengatur tempo consumer (Node: `REST_MIN_MS`/`REST_MAX_MS`, backoff 60 dtk, timeout 15 menit).
#[derive(Debug, Clone, Copy)]
pub struct Pacing {
    pub rest_min: Duration,
    pub rest_max: Duration,
    pub retry_delay: Duration,
    pub job_timeout: Duration,
}

impl Default for Pacing {
    fn default() -> Self {
        Self {
            rest_min: Duration::from_secs(2),
            rest_max: Duration::from_secs(60),
            retry_delay: Duration::from_secs(60),
            job_timeout: REPORT_DISPATCH_JOB_TIMEOUT,
        }
    }
}

async fn run_job(state: &AppState, job: &DispatchJob) -> anyhow::Result<()> {
    match job {
        DispatchJob::ScheduledReport {
            report_id,
            start_date,
            end_date,
            ..
        } => handle_scheduled_report(state, *report_id, start_date, end_date).await,
        DispatchJob::SummaryDaily {
            config_id,
            report_date,
            report_date_formatted,
            subject_date,
            ..
        } => {
            handle_summary_daily(
                state,
                *config_id,
                report_date,
                report_date_formatted,
                subject_date,
            )
            .await
        }
        DispatchJob::SummaryWeekly {
            config_id,
            start_date,
            end_date,
            start_date_formatted,
            end_date_formatted,
            ..
        } => {
            handle_summary_weekly(
                state,
                *config_id,
                start_date,
                end_date,
                start_date_formatted,
                end_date_formatted,
            )
            .await
        }
    }
}

/// Consumer `report-dispatch` (concurrency 1). Job gagal diulang hingga 3 percobaan, jeda tetap
/// (`attempts: 3`, `backoff: fixed 60s`) pada putaran berikutnya agar job lain tidak tertahan.
pub async fn dispatch(state: &AppState, jobs: Vec<DispatchJob>, pacing: &Pacing) {
    if jobs.is_empty() {
        return;
    }
    let _guard = DISPATCH_LOCK.lock().await;
    let mut pending = jobs;
    for attempt in 1..=ATTEMPTS {
        if attempt > 1 {
            tokio::time::sleep(pacing.retry_delay).await;
        }
        let mut failed = Vec::new();
        for job in pending {
            let started = Instant::now();
            let res = tokio::time::timeout(pacing.job_timeout, run_job(state, &job)).await;
            match res {
                Ok(Ok(())) => {
                    let elapsed = started.elapsed();
                    tracing::info!(
                        job = job.name(),
                        id = job.job_id(),
                        ms = elapsed.as_millis() as u64,
                        "report-dispatch selesai"
                    );
                    tokio::time::sleep(elapsed.clamp(pacing.rest_min, pacing.rest_max)).await;
                }
                Ok(Err(err)) => {
                    tracing::error!(job = job.name(), id = job.job_id(), attempt, error = %err, "report-dispatch gagal");
                    failed.push(job);
                }
                Err(_) => {
                    tracing::error!(
                        job = job.name(),
                        id = job.job_id(),
                        attempt,
                        "report-dispatch timeout"
                    );
                    failed.push(job);
                }
            }
        }
        pending = failed;
        if pending.is_empty() {
            break;
        }
    }
}

/// `addBulk` dengan `jobId` unik: hanya job yang berhasil di-claim (24 jam) yang diteruskan.
pub async fn claim_jobs(state: &AppState, jobs: Vec<DispatchJob>) -> Vec<DispatchJob> {
    let mut out = Vec::new();
    for job in jobs {
        let key = format!("mz:job:{}", job.job_id());
        match claim_once(state, &key, Duration::from_secs(24 * 3600)).await {
            Ok(true) => out.push(job),
            Ok(false) => tracing::debug!(id = job.job_id(), "job sudah di-enqueue"),
            Err(err) => tracing::error!(id = job.job_id(), error = %err, "claim job gagal"),
        }
    }
    out
}

/// Node: `tasks.service.ts:3076` `handleManagerSummary`.
async fn run_manager_summary(state: mz_core::AppState) -> anyhow::Result<()> {
    tracing::info!("Running manager summary check...");
    let jobs = due_summary_jobs(&state.db, Utc::now()).await?;
    let jobs = claim_jobs(&state, jobs).await;
    if !jobs.is_empty() {
        tracing::info!(count = jobs.len(), "Enqueued manager summary job(s)");
    }
    dispatch(&state, jobs, &Pacing::default()).await;
    Ok(())
}

/// Node: `tasks.service.ts:252` `handleScheduledReports`.
async fn run_scheduled_reports(state: mz_core::AppState) -> anyhow::Result<()> {
    tracing::info!("Running scheduled reports check...");
    let jobs = due_report_jobs(&state.db, Utc::now()).await?;
    let jobs = claim_jobs(&state, jobs).await;
    if !jobs.is_empty() {
        tracing::info!(
            count = jobs.len(),
            "Enqueued scheduled report(s) for dispatch"
        );
    }
    dispatch(&state, jobs, &Pacing::default()).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn utc(y: i32, m: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, mi, 0).unwrap()
    }

    fn cfg(tz: &str, send: &str) -> SummaryConfigRow {
        SummaryConfigRow {
            id: 7,
            is_daily_enabled: true,
            is_weekly_enabled: true,
            send_time: send.into(),
            include_weekends: false,
            last_daily_sent_at: None,
            last_weekly_sent_at: None,
            time_zone: tz.into(),
        }
    }

    #[test]
    fn slot_uses_company_zone() {
        // 2026-11-17 (Selasa) 01:00 UTC = 08:00 Jakarta.
        let now = utc(2026, 11, 17, 1, 0);
        assert!(slot_matches(&local_now("Asia/Jakarta", now), "08:00"));
        assert!(!slot_matches(&local_now("UTC", now), "08:00"));
        assert!(slot_matches(&local_now("", now), "01:00"));
        assert!(slot_matches(&local_now("Bukan/Zona", now), "01:00"));
    }

    #[test]
    fn daily_plan_formats_yesterday() {
        let jobs = plan_summary(&cfg("Asia/Jakarta", "08:00"), utc(2026, 11, 17, 1, 0));
        assert_eq!(
            jobs,
            vec![DispatchJob::SummaryDaily {
                config_id: 7,
                report_date: "2026-11-16".into(),
                report_date_formatted: "Mon, 16 Nov 2026".into(),
                subject_date: "Mon, 16 Nov 2026".into(),
                job_id: "ms-daily:7:2026-11-17".into(),
            }]
        );
        // Menit lain tidak cocok.
        assert!(plan_summary(&cfg("Asia/Jakarta", "08:00"), utc(2026, 11, 17, 1, 30)).is_empty());
    }

    #[test]
    fn weekend_skipped_unless_included() {
        // 2026-11-21 Sabtu 08:00 UTC.
        let now = utc(2026, 11, 21, 8, 0);
        assert!(plan_summary(&cfg("UTC", "08:00"), now).is_empty());
        let mut c = cfg("UTC", "08:00");
        c.include_weekends = true;
        assert_eq!(plan_summary(&c, now).len(), 1);
        // Zona yang masih Jumat: 2026-11-20 20:00 UTC = Sabtu 03:00 Jakarta -> lewat; LA Jumat -> kirim.
        let now = utc(2026, 11, 21, 4, 0); // Jumat 20:00 di Los_Angeles
        assert_eq!(
            plan_summary(&cfg("America/Los_Angeles", "20:00"), now).len(),
            1
        );
    }

    #[test]
    fn weekly_only_monday_with_range() {
        let now = utc(2026, 11, 16, 8, 0); // Senin
        let jobs = plan_summary(&cfg("UTC", "08:00"), now);
        assert_eq!(jobs.len(), 2);
        assert_eq!(
            jobs[1],
            DispatchJob::SummaryWeekly {
                config_id: 7,
                start_date: "2026-11-09".into(),
                end_date: "2026-11-15".into(),
                start_date_formatted: "9 Nov".into(),
                end_date_formatted: "15 Nov 2026".into(),
                job_id: "ms-weekly:7:2026-11-16".into(),
            }
        );
        let mut c = cfg("UTC", "08:00");
        c.is_daily_enabled = false;
        assert!(plan_summary(&c, utc(2026, 11, 17, 8, 0)).is_empty());
    }

    #[test]
    fn last_sent_guards() {
        let now = utc(2026, 11, 17, 8, 0);
        let mut c = cfg("UTC", "08:00");
        c.last_daily_sent_at = Some(utc(2026, 11, 16, 12, 30).naive_utc()); // 19h30
        assert!(plan_summary(&c, now).is_empty());
        c.last_daily_sent_at = Some(utc(2026, 11, 16, 11, 0).naive_utc()); // 21h
        assert_eq!(plan_summary(&c, now).len(), 1);
        let monday = utc(2026, 11, 16, 8, 0);
        let mut c = cfg("UTC", "08:00");
        c.is_daily_enabled = false;
        c.last_weekly_sent_at = Some(utc(2026, 11, 10, 9, 0).naive_utc()); // 143h
        assert!(plan_summary(&c, monday).is_empty());
        c.last_weekly_sent_at = Some(utc(2026, 11, 10, 8, 0).naive_utc()); // 144h
        assert_eq!(plan_summary(&c, monday).len(), 1);
    }

    fn report(freq: &str, days: Option<Value>) -> ReportRow {
        ReportRow {
            id: 3,
            send_time: "09:00".into(),
            frequency: freq.into(),
            days,
            last_sent_at: None,
            time_zone: "UTC".into(),
        }
    }

    #[test]
    fn report_frequency_rules() {
        let tue = utc(2026, 11, 17, 9, 0);
        assert!(plan_report(&report("DAILY", None), tue).is_some());
        // WEEKLY: kosong = tiap hari; [2] = Selasa; [1] = Senin.
        assert!(plan_report(&report("WEEKLY", Some(json!([]))), tue).is_some());
        assert!(plan_report(&report("WEEKLY", Some(json!([2]))), tue).is_some());
        assert!(plan_report(&report("WEEKLY", Some(json!([1, 3]))), tue).is_none());
        assert!(plan_report(&report("MONTHLY", None), tue).is_none());
        assert!(plan_report(&report("MONTHLY", None), utc(2026, 12, 1, 9, 0)).is_some());
        assert!(plan_report(&report("YEARLY", None), utc(2026, 12, 1, 9, 0)).is_none());
        assert!(plan_report(&report("YEARLY", None), utc(2027, 1, 1, 9, 0)).is_some());
        assert!(plan_report(&report("DAILY", None), utc(2026, 11, 17, 9, 30)).is_none());
    }

    #[test]
    fn report_min_hours_and_ranges() {
        let now = utc(2026, 11, 17, 9, 0);
        let mut r = report("DAILY", None);
        r.last_sent_at = Some(utc(2026, 11, 16, 13, 0).naive_utc()); // 20h
        assert!(plan_report(&r, now).is_some());
        r.last_sent_at = Some(utc(2026, 11, 16, 14, 0).naive_utc()); // 19h
        assert!(plan_report(&r, now).is_none());

        let t = local_now("UTC", utc(2026, 3, 1, 9, 0));
        assert_eq!(
            report_date_range("MONTHLY", &t),
            Some(("2026-02-01".into(), "2026-02-28".into()))
        );
        let t = local_now("UTC", utc(2026, 1, 1, 9, 0));
        assert_eq!(
            report_date_range("YEARLY", &t),
            Some(("2025-01-01".into(), "2025-12-31".into()))
        );
        assert_eq!(
            report_date_range("MONTHLY", &t),
            Some(("2025-12-01".into(), "2025-12-31".into()))
        );
        assert_eq!(
            report_date_range("WEEKLY", &t),
            Some(("2025-12-25".into(), "2025-12-31".into()))
        );
        assert_eq!(report_date_range("DAILY", &t).unwrap().0, "2025-12-31");
        let id = plan_report(&report("DAILY", None), now).unwrap();
        assert_eq!(id.job_id(), "scheduled-report:3:2026-11-17");
    }

    fn member(id: i32, name: &str) -> Member {
        Member {
            id,
            name: name.into(),
        }
    }
    fn row(u: i32, un: &str, p: i32, pn: &str, secs: i32, d: (i32, u32, u32)) -> UserProjectRow {
        UserProjectRow {
            user_id: u,
            user_name: un.into(),
            project_id: p,
            project_name: pn.into(),
            total_seconds: secs,
            report_date: NaiveDate::from_ymd_opt(d.0, d.1, d.2).unwrap(),
        }
    }
    fn params() -> SummaryParams {
        SummaryParams {
            user_ids: None,
            low_hours_threshold: 0,
            disabled_flags: vec![],
            include_weekends: false,
        }
    }

    #[test]
    fn format_and_workdays() {
        assert_eq!(format_seconds(3725), "1h 2m");
        assert_eq!(format_seconds(0), "0h 0m");
        let s = NaiveDate::from_ymd_opt(2026, 11, 9).unwrap();
        let e = NaiveDate::from_ymd_opt(2026, 11, 15).unwrap();
        assert_eq!(count_work_days(s, e, false), 5);
        assert_eq!(count_work_days(s, e, true), 7);
    }

    #[test]
    fn daily_data_shape() {
        let members = [member(1, "Ani"), member(2, "Budi"), member(3, "Cici")];
        let rows = [
            row(1, "Ani", 10, "Alpha", 20000, (2026, 11, 16)),
            row(1, "Ani", 11, "Beta", 1000, (2026, 11, 16)),
            row(2, "Budi", 11, "Beta", 3600, (2026, 11, 16)),
        ];
        let d = build_daily(&members, &rows, &params(), "2026-11-16");
        assert_eq!(d["totalHours"], "6h 50m");
        assert_eq!(d["activeMembers"], 2);
        assert_eq!(d["totalMembers"], 3);
        assert_eq!(d["noActivity"], false);
        assert_eq!(d["topProject"], json!({"name": "Alpha", "hours": "5h 33m"}));
        let team = d["teamRows"].as_array().unwrap();
        // green dulu, lalu yellow, lalu red (a - b menaik di Node).
        let names: Vec<&str> = team.iter().map(|r| r["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["Ani", "Budi", "Cici"]);
        assert_eq!(team[0]["activity"], "green");
        assert_eq!(team[0]["primaryProject"], "Alpha");
        assert_eq!(team[1]["isYellow"], true);
        assert_eq!(team[2]["primaryProject"], "\u{2014}");
        let flags = d["flags"].as_array().unwrap();
        assert_eq!(flags[0]["text"], "Didn't track: Cici");
        assert_eq!(flags[1]["text"], "Low hours: Budi (1h 0m)");
        assert_eq!(d["hasFlags"], true);
        // Flag dinonaktifkan.
        let mut p = params();
        p.disabled_flags = vec!["didnt_track".into(), "low_hours".into()];
        let d = build_daily(&members, &rows, &p, "2026-11-16");
        assert_eq!(d["hasFlags"], false);
    }

    #[test]
    fn daily_extra_rows_and_no_activity() {
        let members: Vec<Member> = (1..=17).map(|i| member(i, &format!("U{i}"))).collect();
        let d = build_daily(&members, &[], &params(), "2026-11-16");
        assert_eq!(d["noActivity"], true);
        assert_eq!(d["teamRows"].as_array().unwrap().len(), 15);
        assert_eq!(d["teamRowsExtra"], 2);
        assert_eq!(d["topProject"], Value::Null);
    }

    #[test]
    fn weekly_data_shape() {
        let members = [member(1, "Ani"), member(2, "Budi")];
        let day = |d| (2026, 11, d);
        let rows = [
            row(1, "Ani", 10, "Alpha", 28800, day(9)),
            row(1, "Ani", 10, "Alpha", 28800, day(10)),
            row(2, "Budi", 10, "Alpha", 3600, day(9)),
        ];
        let s = NaiveDate::from_ymd_opt(2026, 11, 9).unwrap();
        let e = NaiveDate::from_ymd_opt(2026, 11, 15).unwrap();
        let d = build_weekly(&members, &rows, 40000, &params(), s, e);
        assert_eq!(d["totalHours"], "17h 0m");
        // (61200-40000)/40000 = 53%.
        assert_eq!(d["weekOverWeekChange"], "up 53% from last week");
        // ambang mingguan 14400*5 = 72000 -> Ani 57600 kuning.
        assert_eq!(d["teamRows"][0]["name"], "Ani");
        assert_eq!(d["teamRows"][0]["activity"], "yellow");
        let flags = d["flags"].as_array().unwrap();
        assert_eq!(
            flags[0]["text"],
            "Didn't track: Ani (3 days), Budi (4 days)"
        );
        let d = build_weekly(&members, &rows, 0, &params(), s, e);
        assert_eq!(d["weekOverWeekChange"], Value::Null);
        let d = build_weekly(&members, &rows, 122400, &params(), s, e);
        assert_eq!(d["weekOverWeekChange"], "down 50% from last week");
    }

    #[test]
    fn email_subject_and_body() {
        let data = build_daily(&[member(1, "Ani")], &[], &params(), "2026-11-16");
        let ctx = mail_context(
            "Acme",
            &[
                ("reportDateFormatted", "Mon, 16 Nov 2026"),
                ("subjectDate", "Mon, 16 Nov 2026"),
            ],
            &data,
        );
        assert_eq!(
            crate::mail::summary_subject(SummaryKind::Daily, &ctx),
            "Daily Team Summary \u{2014} Mon, 16 Nov 2026"
        );
        let body = crate::mail::summary_body_html(
            SummaryKind::Daily,
            Some("noreply@x.test"),
            &["a@b.test".to_owned()],
            &ctx,
            "<p>Ani</p>",
        );
        assert_eq!(
            body["to"][0],
            json!({"email": "a@b.test", "name": "a@b.test"})
        );
        assert_eq!(body["category"], "Manager Summary Daily");
        assert_eq!(body["attachments"], json!([]));
        assert!(body["html"].as_str().unwrap().contains("Ani"));

        let wk = mail_context(
            "Acme",
            &[
                ("startDateFormatted", "9 Nov"),
                ("endDateFormatted", "15 Nov 2026"),
            ],
            &Map::new(),
        );
        assert_eq!(
            crate::mail::summary_subject(SummaryKind::Weekly, &wk),
            "Weekly Team Summary \u{2014} 9 Nov \u{2013} 15 Nov 2026"
        );
    }

    #[test]
    fn payload_defaults() {
        let p = report_payload(
            &json!(["a@b.c"]),
            "S",
            "M",
            "2026-11-16",
            "2026-11-16",
            None,
            None,
            None,
        );
        assert_eq!(p["sections"], json!(["company", "project", "user"]));
        assert_eq!(p["userIds"], json!([]));
        let dto: mz_screenshots::reports::dto::SendReportDto = serde_json::from_value(p).unwrap();
        assert_eq!(dto.emails(), vec!["a@b.c".to_owned()]);
    }

    #[test]
    fn cron_registry() {
        let jobs = cron_jobs();
        let names: Vec<&str> = jobs.iter().map(|j| j.name).collect();
        assert_eq!(names, ["manager-summary", "scheduled-reports"]);
    }
}
