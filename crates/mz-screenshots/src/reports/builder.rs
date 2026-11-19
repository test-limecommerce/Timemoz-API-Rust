//! Port `src/reports/work-report.builder.ts` (+ `tracking-timelines/timeline-instant.ts`):
//! susun data per karyawan -> per hari untuk PDF.

use chrono::{DateTime, Utc};
use mz_core::time::JsDate;

use super::pdf::work_report::{AppUsage, Day, Employee, Entry};
use super::support::{OrderedMap, date_part};

/// Entri project yang sama dengan jeda <= 60 detik digabung jadi satu baris.
const MERGE_GAP_MS: i64 = 60 * 1000;

/// Bentuk baris `trackingTimeline` yang dibutuhkan report jam kerja.
#[derive(Debug, Clone)]
pub struct TimelineRow {
    pub id: i32,
    pub uuid: String,
    pub note: Option<String>,
    pub start_at: JsDate,
    pub end_at: JsDate,
    pub start_at_utc: Option<JsDate>,
    pub end_at_utc: Option<JsDate>,
    pub timezone: Option<String>,
    pub billable: Option<bool>,
    pub project_id: i32,
    pub project_uuid: String,
    pub project_name: String,
    pub project_billable: bool,
    pub taken_date: JsDate,
    pub user_id: i32,
    pub user_name: String,
    pub user_username: String,
}

/// Hasil agregasi tabel `app` per (timeline, nama aplikasi).
#[derive(Debug, Clone)]
pub struct AppRow {
    pub tracking_timeline_id: i32,
    pub name: String,
    pub seconds: f64,
}

struct ClockPoint {
    ms: i64,
    label: String,
}

/// `timelineInstant`: zona dikenal -> kolom timestamp sudah instan (`utcAt` diutamakan);
/// selain itu (data lama) jam dinding dibaca sebagai Asia/Jakarta (UTC+7 tetap).
fn clock_of(at: JsDate, utc_at: Option<JsDate>, timezone: Option<&str>) -> ClockPoint {
    let tz = timezone.and_then(|z| z.parse::<chrono_tz::Tz>().ok());
    match tz {
        Some(tz) => {
            let ms = utc_at.unwrap_or(at).timestamp_millis();
            let label = DateTime::<Utc>::from_timestamp_millis(ms)
                .map(|d| d.with_timezone(&tz).format("%H:%M").to_string())
                .unwrap_or_default();
            ClockPoint { ms, label }
        }
        None => ClockPoint {
            ms: at.timestamp_millis() - 7 * 60 * 60 * 1000,
            label: at.0.format("%H:%M").to_string(),
        },
    }
}

fn duration_seconds(t: &TimelineRow) -> i64 {
    let start = t.start_at_utc.unwrap_or(t.start_at);
    let end = t.end_at_utc.unwrap_or(t.end_at);
    ((end.timestamp_millis() - start.timestamp_millis()).div_euclid(1000)).max(0)
}

fn clean_note(raw: Option<&str>) -> Option<String> {
    let note = raw.unwrap_or("").trim();
    if note.is_empty() || note == "-" || note == "null" {
        None
    } else {
        Some(note.to_owned())
    }
}

struct DraftEntry {
    project_id: i32,
    project: String,
    end_ms: i64,
    start: String,
    end: String,
    seconds: i64,
    notes: Vec<String>,
}

struct DraftDay {
    date: String,
    entries: Vec<DraftEntry>,
    apps: OrderedMap<String, i64>,
}

struct DraftEmployee {
    id: i32,
    name: String,
    email: String,
    days: OrderedMap<String, usize>,
    day_store: Vec<DraftDay>,
}

/// `buildWorkReportEmployees`.
pub fn build_work_report_employees(
    timelines: &[TimelineRow],
    app_rows: &[AppRow],
) -> Vec<Employee> {
    let mut employees: OrderedMap<i32, DraftEmployee> = OrderedMap::new();
    // timeline id -> (user id, indeks hari di DraftEmployee.day_store)
    let mut day_of_timeline: std::collections::HashMap<i32, (i32, usize)> =
        std::collections::HashMap::new();

    let mut sorted: Vec<(&TimelineRow, ClockPoint, ClockPoint, i64)> = timelines
        .iter()
        .map(|t| {
            (
                t,
                clock_of(t.start_at, t.start_at_utc, t.timezone.as_deref()),
                clock_of(t.end_at, t.end_at_utc, t.timezone.as_deref()),
                duration_seconds(t),
            )
        })
        .collect();
    sorted.sort_by_key(|(_, start, _, _)| start.ms);

    for (t, start, end, seconds) in sorted {
        let employee = employees.entry_or_insert_with(t.user_id, || DraftEmployee {
            id: t.user_id,
            name: if !t.user_name.is_empty() {
                t.user_name.clone()
            } else if !t.user_username.is_empty() {
                t.user_username.clone()
            } else {
                format!("User {}", t.user_id)
            },
            email: t.user_username.clone(),
            days: OrderedMap::new(),
            day_store: Vec::new(),
        });

        let date = date_part(t.taken_date.0);
        let day_idx = *employee.days.entry_or_insert_with(date.clone(), || {
            employee.day_store.push(DraftDay {
                date: date.clone(),
                entries: Vec::new(),
                apps: OrderedMap::new(),
            });
            employee.day_store.len() - 1
        });
        day_of_timeline.insert(t.id, (t.user_id, day_idx));

        let note = clean_note(t.note.as_deref());
        let Some(day) = employee.day_store.get_mut(day_idx) else {
            continue;
        };
        if let Some(last) = day.entries.last_mut()
            && last.project_id == t.project_id
            && start.ms - last.end_ms <= MERGE_GAP_MS
        {
            if end.ms > last.end_ms {
                last.end_ms = end.ms;
                last.end = end.label;
            }
            last.seconds += seconds;
            if let Some(n) = note
                && !last.notes.contains(&n)
            {
                last.notes.push(n);
            }
            continue;
        }
        day.entries.push(DraftEntry {
            project_id: t.project_id,
            project: t.project_name.clone(),
            end_ms: end.ms,
            start: start.label,
            end: end.label,
            seconds,
            notes: note.into_iter().collect(),
        });
    }

    for row in app_rows {
        #[allow(clippy::cast_possible_truncation)]
        let seconds = (row.seconds + 0.5).floor() as i64;
        let name = row.name.trim();
        let Some((user_id, day_idx)) = day_of_timeline.get(&row.tracking_timeline_id) else {
            continue;
        };
        if seconds <= 0 || name.is_empty() {
            continue;
        }
        if let Some(day) = employees
            .get_mut(user_id)
            .and_then(|e| e.day_store.get_mut(*day_idx))
        {
            *day.apps.entry_or_insert_with(name.to_owned(), || 0) += seconds;
        }
    }

    let mut out: Vec<Employee> = employees
        .into_values()
        .into_iter()
        .map(|employee| {
            let mut days: Vec<Day> = employee
                .day_store
                .into_iter()
                .map(|day| {
                    let entries: Vec<Entry> = day
                        .entries
                        .into_iter()
                        .map(|e| Entry {
                            start: Some(e.start),
                            end: Some(e.end),
                            project: e.project,
                            notes: if e.notes.is_empty() {
                                "-".to_owned()
                            } else {
                                e.notes.join(" | ")
                            },
                            seconds: e.seconds,
                        })
                        .collect();
                    let names: Vec<String> = day.apps.keys().cloned().collect();
                    let mut apps: Vec<AppUsage> = names
                        .into_iter()
                        .zip(day.apps.into_values())
                        .map(|(name, seconds)| AppUsage { name, seconds })
                        .collect();
                    apps.sort_by_key(|a| std::cmp::Reverse(a.seconds));
                    Day {
                        date: day.date,
                        seconds: entries.iter().map(|e| e.seconds).sum(),
                        entries,
                        apps,
                    }
                })
                .collect();
            days.sort_by(|a, b| a.date.cmp(&b.date));
            Employee {
                id: employee.id,
                name: employee.name,
                email: Some(employee.email).filter(|e| !e.is_empty()),
                seconds: days.iter().map(|d| d.seconds).sum(),
                days,
            }
        })
        .collect();
    out.sort_by(|a, b| {
        b.seconds
            .cmp(&a.seconds)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use mz_core::time::parse_js_date;

    fn d(s: &str) -> JsDate {
        JsDate(parse_js_date(s).unwrap_or_default())
    }

    #[allow(clippy::too_many_arguments)]
    fn row(
        id: i32,
        user: i32,
        project: i32,
        start: &str,
        end: &str,
        tz: Option<&str>,
        note: &str,
    ) -> TimelineRow {
        TimelineRow {
            id,
            uuid: format!("t{id}"),
            note: Some(note.into()),
            start_at: d(start),
            end_at: d(end),
            start_at_utc: tz.map(|_| d(start)),
            end_at_utc: tz.map(|_| d(end)),
            timezone: tz.map(str::to_owned),
            billable: None,
            project_id: project,
            project_uuid: format!("p{project}"),
            project_name: format!("P{project}"),
            project_billable: true,
            taken_date: d("2026-10-05T00:00:00Z"),
            user_id: user,
            user_name: format!("User {user}"),
            user_username: format!("u{user}@x.id"),
        }
    }

    #[test]
    fn merges_close_entries_and_formats_in_zone() {
        let rows = vec![
            row(
                1,
                1,
                7,
                "2026-10-05T02:00:00Z",
                "2026-10-05T03:00:00Z",
                Some("Asia/Jakarta"),
                "a",
            ),
            row(
                2,
                1,
                7,
                "2026-10-05T03:00:30Z",
                "2026-10-05T04:00:00Z",
                Some("Asia/Jakarta"),
                "b",
            ),
            row(
                3,
                1,
                8,
                "2026-10-05T04:30:00Z",
                "2026-10-05T05:00:00Z",
                Some("Asia/Jakarta"),
                "null",
            ),
        ];
        let apps = vec![
            AppRow {
                tracking_timeline_id: 1,
                name: " Chrome ".into(),
                seconds: 600.4,
            },
            AppRow {
                tracking_timeline_id: 2,
                name: "Chrome".into(),
                seconds: 100.0,
            },
            AppRow {
                tracking_timeline_id: 3,
                name: "".into(),
                seconds: 50.0,
            },
            AppRow {
                tracking_timeline_id: 99,
                name: "X".into(),
                seconds: 50.0,
            },
        ];
        let out = build_work_report_employees(&rows, &apps);
        assert_eq!(out.len(), 1);
        let day = &out[0].days[0];
        assert_eq!(day.entries.len(), 2);
        assert_eq!(day.entries[0].start.as_deref(), Some("09:00"));
        assert_eq!(day.entries[0].end.as_deref(), Some("11:00"));
        assert_eq!(day.entries[0].notes, "a | b");
        assert_eq!(day.entries[0].seconds, 3600 + 3570);
        assert_eq!(day.entries[1].notes, "-");
        assert_eq!(day.apps.len(), 1);
        assert_eq!(day.apps[0].seconds, 700);
        assert_eq!(out[0].email.as_deref(), Some("u1@x.id"));
    }

    #[test]
    fn legacy_rows_read_wall_clock() {
        let rows = vec![row(
            1,
            2,
            1,
            "2026-10-05T09:00:00Z",
            "2026-10-05T10:00:00Z",
            None,
            "x",
        )];
        let out = build_work_report_employees(&rows, &[]);
        assert_eq!(out[0].days[0].entries[0].start.as_deref(), Some("09:00"));
    }

    #[test]
    fn employees_sorted_by_seconds_then_name() {
        let rows = vec![
            row(
                1,
                1,
                1,
                "2026-10-05T01:00:00Z",
                "2026-10-05T01:10:00Z",
                Some("UTC"),
                "x",
            ),
            row(
                2,
                2,
                1,
                "2026-10-05T01:00:00Z",
                "2026-10-05T02:00:00Z",
                Some("UTC"),
                "x",
            ),
        ];
        let out = build_work_report_employees(&rows, &[]);
        assert_eq!(out.iter().map(|e| e.id).collect::<Vec<_>>(), vec![2, 1]);
    }
}
