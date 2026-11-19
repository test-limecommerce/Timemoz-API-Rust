//! Rincian harian timesheet dari baris `trackingTimeline` (murni, tanpa DB) dan
//! pembuat CSV ekspor.

use std::collections::BTreeMap;

use chrono::{Duration, NaiveDate, NaiveDateTime};
use serde_json::{Value, json};
use sqlx::FromRow;

use crate::calc::{duration_seconds, hours2, to_hms};
use crate::common::{date_str, jd, jd_opt};
use crate::jsfmt;

/// Satu baris timeline + project + tanggal tracking.
#[derive(Debug, Clone, FromRow)]
pub struct TimelineRow {
    pub uuid: String,
    #[sqlx(rename = "startAt")]
    pub start_at: NaiveDateTime,
    #[sqlx(rename = "endAt")]
    pub end_at: NaiveDateTime,
    #[sqlx(rename = "startAtUtc")]
    pub start_at_utc: Option<NaiveDateTime>,
    #[sqlx(rename = "endAtUtc")]
    pub end_at_utc: Option<NaiveDateTime>,
    #[sqlx(rename = "offsetMinutes")]
    pub offset_minutes: Option<i32>,
    pub billable: Option<bool>,
    pub note: String,
    #[sqlx(rename = "isOffline")]
    pub is_offline: bool,
    #[sqlx(rename = "manualTimeStatus")]
    pub manual_time_status: Option<String>,
    #[sqlx(rename = "manualTimeApprovalSource")]
    pub manual_time_approval_source: Option<String>,
    #[sqlx(rename = "projectUuid")]
    pub project_uuid: Option<String>,
    #[sqlx(rename = "projectName")]
    pub project_name: Option<String>,
    #[sqlx(rename = "projectBillable")]
    pub project_billable: Option<bool>,
    #[sqlx(rename = "takenDate")]
    pub taken_date: NaiveDate,
}

#[derive(Debug, Clone)]
pub struct ProjectAgg {
    pub name: String,
    pub total: i64,
    pub billable: i64,
}

#[derive(Debug, Clone)]
pub struct Day {
    pub date: NaiveDate,
    pub total: i64,
    pub billable: i64,
    pub non_billable: i64,
    pub projects: Vec<ProjectAgg>,
    pub entries: Vec<Value>,
}

#[derive(Debug, Clone, Default)]
pub struct Breakdown {
    pub days: Vec<Day>,
    pub billable: i64,
    pub non_billable: i64,
}

/// `isBillable = tl.billable !== null ? tl.billable : (project?.billable ?? true)`.
fn is_billable(r: &TimelineRow) -> bool {
    r.billable
        .unwrap_or_else(|| r.project_billable.unwrap_or(true))
}

fn entry_json(r: &TimelineRow, seconds: i64, billable: bool) -> Value {
    let resolve = |utc: Option<NaiveDateTime>, local: NaiveDateTime| {
        utc.or_else(|| {
            r.offset_minutes
                .map(|m| local - Duration::minutes(i64::from(m)))
        })
    };
    json!({
        "uuid": r.uuid,
        "startAt": jd(r.start_at),
        "endAt": jd(r.end_at),
        "startAtUtc": jd_opt(resolve(r.start_at_utc, r.start_at)),
        "endAtUtc": jd_opt(resolve(r.end_at_utc, r.end_at)),
        "durationSeconds": seconds,
        "billable": billable,
        "billableOverride": r.billable,
        "note": r.note,
        "isOffline": r.is_offline,
        "manualTimeStatus": r.manual_time_status,
        "manualTimeApprovalSource": r.manual_time_approval_source,
        "projectUuid": r.project_uuid,
        "projectName": r.project_name.clone().unwrap_or_else(|| "Unknown".into()),
        "projectBillable": r.project_billable.unwrap_or(true),
    })
}

/// Kelompokkan per tanggal tracking (urut naik) lalu per nama project (urut kemunculan).
pub fn build_breakdown(rows: &[TimelineRow]) -> Breakdown {
    let mut map: BTreeMap<NaiveDate, Day> = BTreeMap::new();
    let mut out = Breakdown::default();
    for r in rows {
        let seconds = duration_seconds(r.start_at, r.end_at);
        let billable = is_billable(r);
        let day = map.entry(r.taken_date).or_insert_with(|| Day {
            date: r.taken_date,
            total: 0,
            billable: 0,
            non_billable: 0,
            projects: Vec::new(),
            entries: Vec::new(),
        });
        day.total += seconds;
        if billable {
            day.billable += seconds;
            out.billable += seconds;
        } else {
            day.non_billable += seconds;
            out.non_billable += seconds;
        }
        let name = r.project_name.clone().unwrap_or_else(|| "Unknown".into());
        match day.projects.iter_mut().find(|p| p.name == name) {
            Some(p) => {
                p.total += seconds;
                if billable {
                    p.billable += seconds;
                }
            }
            None => day.projects.push(ProjectAgg {
                name,
                total: seconds,
                billable: if billable { seconds } else { 0 },
            }),
        }
        day.entries.push(entry_json(r, seconds, billable));
    }
    out.days = map.into_values().collect();
    out
}

/// `dailyBreakdown` pada `GET /timesheets/:uuid`.
pub fn daily_json(days: &[Day]) -> Value {
    Value::Array(
        days.iter()
            .map(|d| {
                json!({
                    "date": date_str(d.date),
                    "totalSeconds": d.total,
                    "totalHours": jsfmt::num(hours2(d.total)),
                    "billableSeconds": d.billable,
                    "nonBillableSeconds": d.non_billable,
                    "projects": d.projects.iter()
                        .map(|p| json!({ "projectName": p.name, "totalSeconds": p.total }))
                        .collect::<Vec<_>>(),
                    "entries": d.entries,
                })
            })
            .collect(),
    )
}

/// Data header/ringkasan CSV.
pub struct CsvInput<'a> {
    pub employee_name: &'a str,
    pub period_start: NaiveDate,
    pub period_end: NaiveDate,
    pub pay_period_type: &'a str,
    pub status: &'a str,
    pub submitted_at: Option<NaiveDate>,
    pub total_seconds: i64,
    pub total_amount: Option<f64>,
    pub currency: &'a str,
}

/// Isi CSV (tanpa BOM). Persis `exportCsv`.
pub fn build_csv(i: &CsvInput<'_>, days: &[Day]) -> String {
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!("Timesheet Report - {}", i.employee_name));
    lines.push(format!(
        "Period,{} - {}",
        date_str(i.period_start),
        date_str(i.period_end)
    ));
    lines.push(format!("Pay Period Type,{}", i.pay_period_type));
    lines.push(format!("Status,{}", i.status));
    lines.push(format!(
        "Submitted At,{}",
        i.submitted_at.map_or_else(|| "-".to_owned(), date_str)
    ));
    lines.push(String::new());
    lines.push("Date,Project,Duration,Hours,Billable Hours,Non-Billable Hours".to_owned());
    for day in days {
        let date = date_str(day.date);
        for p in &day.projects {
            lines.push(format!(
                "{date},\"{}\",{},{},{},{}",
                p.name,
                to_hms(p.total),
                jsfmt::num_string(hours2(p.total)),
                to_hms(p.billable),
                to_hms(p.total - p.billable)
            ));
        }
        lines.push(format!(
            "{date},DAILY TOTAL,{},{},,",
            to_hms(day.total),
            jsfmt::num_string(hours2(day.total))
        ));
    }
    lines.push(String::new());
    lines.push("SUMMARY".to_owned());
    lines.push(format!(
        "Total Hours,{}",
        jsfmt::num_string(hours2(i.total_seconds))
    ));
    lines.push(format!("Total Working Days,{}", days.len()));
    if let Some(a) = i.total_amount {
        lines.push(format!(
            "Total Amount,\"{} {}\"",
            i.currency,
            jsfmt::num_string(a)
        ));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dt(d: u32, h: u32, m: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 4, d)
            .and_then(|x| x.and_hms_opt(h, m, 0))
            .unwrap_or_default()
    }

    fn row(
        day: u32,
        from: (u32, u32),
        to: (u32, u32),
        project: &str,
        billable: Option<bool>,
        pb: Option<bool>,
    ) -> TimelineRow {
        TimelineRow {
            uuid: "u".into(),
            start_at: dt(day, from.0, from.1),
            end_at: dt(day, to.0, to.1),
            start_at_utc: None,
            end_at_utc: None,
            offset_minutes: Some(420),
            billable,
            note: String::new(),
            is_offline: false,
            manual_time_status: None,
            manual_time_approval_source: None,
            project_uuid: None,
            project_name: Some(project.into()),
            project_billable: pb,
            taken_date: NaiveDate::from_ymd_opt(2026, 4, day).unwrap_or_default(),
        }
    }

    #[test]
    fn groups_by_day_and_project_with_billable_rules() {
        let rows = vec![
            row(2, (9, 0), (10, 0), "B", None, Some(true)),
            row(1, (9, 0), (10, 30), "A", Some(false), Some(true)),
            row(2, (11, 0), (11, 30), "A", None, Some(false)),
            row(2, (12, 0), (13, 0), "B", None, None),
        ];
        let b = build_breakdown(&rows);
        assert_eq!(b.days.len(), 2);
        assert_eq!(b.days[0].date.to_string(), "2026-04-01");
        assert_eq!(b.days[1].total, 3600 + 1800 + 3600);
        assert_eq!(b.days[1].projects[0].name, "B");
        assert_eq!(b.days[1].projects[0].total, 7200);
        assert_eq!(b.billable, 7200);
        assert_eq!(b.non_billable, 5400 + 1800);
        // startAtUtc diturunkan dari offsetMinutes.
        let e = &b.days[0].entries[0];
        assert_eq!(e["startAtUtc"], "2026-04-01T02:00:00.000Z");
        assert_eq!(e["billableOverride"], false);
    }

    #[test]
    fn csv_layout() {
        let rows = vec![row(1, (9, 0), (10, 30), "A \"x\"", None, Some(true))];
        let b = build_breakdown(&rows);
        let csv = build_csv(
            &CsvInput {
                employee_name: "Budi",
                period_start: NaiveDate::from_ymd_opt(2026, 4, 1).unwrap_or_default(),
                period_end: NaiveDate::from_ymd_opt(2026, 4, 30).unwrap_or_default(),
                pay_period_type: "MONTHLY",
                status: "APPROVED",
                submitted_at: None,
                total_seconds: 5400,
                total_amount: Some(150.5),
                currency: "USD",
            },
            &b.days,
        );
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines[0], "Timesheet Report - Budi");
        assert_eq!(lines[1], "Period,2026-04-01 - 2026-04-30");
        assert_eq!(lines[4], "Submitted At,-");
        assert_eq!(
            lines[7],
            "2026-04-01,\"A \"x\"\",01:30:00,1.5,01:30:00,00:00:00"
        );
        assert_eq!(lines[8], "2026-04-01,DAILY TOTAL,01:30:00,1.5,,");
        assert_eq!(lines[11], "Total Hours,1.5");
        assert_eq!(lines[12], "Total Working Days,1");
        assert_eq!(lines[13], "Total Amount,\"USD 150.5\"");
    }
}
