//! Port `src/reports/work-report-pdf.generator.ts`: PDF "Work Hours & Activity Report" —
//! ringkasan tim, breakdown project, lalu per karyawan timesheet harian dan log pemakaian aplikasi.
//! Tata letak mengikuti pdfkit semirip mungkin; yang wajib sama adalah isi dan urutannya.

use std::collections::HashMap;

use chrono::{DateTime, Utc};

use super::canvas::{
    CW, Canvas, ChartOptions, Column, CoverOptions, FooterOptions, KpiCard, TextStyle,
};
use super::doc::{Align, Font};
use super::format::theme::{
    AMBER, BORDER, CYAN, DARK, FAINT, GREEN, M, MUTED, NAVY, PRIMARY, PRIMARY_SOFT, PROJECT_COLORS,
    SURFACE, SURFACE2, TEXT, VIOLET, WHITE,
};
use super::format::{
    chart_buckets, clean_text, enumerate_days, f_date, f_day, f_dur, f_generated_at, pct,
};

#[derive(Debug, Clone)]
pub struct Entry {
    /// Jam lokal "HH:mm".
    pub start: Option<String>,
    pub end: Option<String>,
    pub project: String,
    pub notes: String,
    pub seconds: i64,
}

#[derive(Debug, Clone)]
pub struct AppUsage {
    pub name: String,
    pub seconds: i64,
}

#[derive(Debug, Clone)]
pub struct Day {
    /// "YYYY-MM-DD"
    pub date: String,
    pub seconds: i64,
    pub entries: Vec<Entry>,
    pub apps: Vec<AppUsage>,
}

#[derive(Debug, Clone)]
pub struct Employee {
    pub id: i32,
    pub name: String,
    pub email: Option<String>,
    pub seconds: i64,
    pub days: Vec<Day>,
}

#[derive(Debug, Clone, Copy)]
pub struct Sections {
    pub summary: bool,
    pub projects: bool,
    pub employees: bool,
}

impl Default for Sections {
    fn default() -> Self {
        Self {
            summary: true,
            projects: true,
            employees: true,
        }
    }
}

pub struct ReportData {
    pub company_name: String,
    pub start_date: String,
    pub end_date: String,
    pub generated_at: DateTime<Utc>,
    pub generated_by: String,
    pub time_zone: Option<String>,
    pub employees: Vec<Employee>,
    pub sections: Sections,
}

/// Batas panjang notes per baris supaya satu baris tak pernah melebihi satu halaman.
const MAX_NOTE_CHARS: usize = 400;
/// Baris tabel aplikasi per karyawan; sisanya diringkas jadi "Other".
const TOP_APPS_PER_EMPLOYEE: usize = 5;

struct ProjectStat {
    name: String,
    seconds: i64,
    members: Vec<String>,
    color: &'static str,
}

struct EmployeeStat<'a> {
    employee: &'a Employee,
    active_days: usize,
    app_totals: Vec<AppUsage>,
    app_seconds: i64,
    projects: Vec<(String, i64)>,
}

#[allow(clippy::cast_precision_loss)]
fn secs(v: i64) -> f64 {
    v as f64
}

/// Tambah ke daftar berurutan-kemunculan (`Map` JS).
fn add_ordered(list: &mut Vec<(String, i64)>, key: &str, v: i64) {
    if let Some(e) = list.iter_mut().find(|(k, _)| k == key) {
        e.1 += v;
    } else {
        list.push((key.to_owned(), v));
    }
}

fn employee_stat(e: &Employee) -> EmployeeStat<'_> {
    let mut apps: Vec<(String, i64)> = Vec::new();
    let mut projects: Vec<(String, i64)> = Vec::new();
    for d in &e.days {
        for a in &d.apps {
            add_ordered(&mut apps, &a.name, a.seconds);
        }
        for en in &d.entries {
            add_ordered(&mut projects, &en.project, en.seconds);
        }
    }
    apps.sort_by_key(|a| std::cmp::Reverse(a.1));
    projects.sort_by_key(|p| std::cmp::Reverse(p.1));
    let app_totals: Vec<AppUsage> = apps
        .into_iter()
        .map(|(name, seconds)| AppUsage { name, seconds })
        .collect();
    EmployeeStat {
        employee: e,
        active_days: e.days.iter().filter(|d| d.seconds > 0).count(),
        app_seconds: app_totals.iter().map(|a| a.seconds).sum(),
        app_totals,
        projects,
    }
}

fn project_stats(employees: &[&Employee]) -> Vec<ProjectStat> {
    let mut list: Vec<(String, i64, Vec<String>)> = Vec::new();
    for e in employees {
        for d in &e.days {
            for en in &d.entries {
                if let Some(p) = list.iter_mut().find(|p| p.0 == en.project) {
                    p.1 += en.seconds;
                    if !p.2.contains(&e.name) {
                        p.2.push(e.name.clone());
                    }
                } else {
                    list.push((en.project.clone(), en.seconds, vec![e.name.clone()]));
                }
            }
        }
    }
    list.sort_by_key(|p| std::cmp::Reverse(p.1));
    list.into_iter()
        .enumerate()
        .map(|(i, (name, seconds, members))| ProjectStat {
            name,
            seconds,
            members,
            color: PROJECT_COLORS
                .get(i % PROJECT_COLORS.len())
                .copied()
                .unwrap_or(PRIMARY),
        })
        .collect()
}

/// (nama, detik, jumlah pengguna) urut detik menurun.
fn team_apps(stats: &[EmployeeStat<'_>]) -> Vec<(String, i64, usize)> {
    let mut list: Vec<(String, i64, usize)> = Vec::new();
    for s in stats {
        for a in &s.app_totals {
            if let Some(e) = list.iter_mut().find(|e| e.0 == a.name) {
                e.1 += a.seconds;
                e.2 += 1;
            } else {
                list.push((a.name.clone(), a.seconds, 1));
            }
        }
    }
    list.sort_by_key(|a| std::cmp::Reverse(a.1));
    list
}

fn plural(n: usize, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

pub struct WorkReportPdf<'a> {
    data: &'a ReportData,
    pdf: Canvas,
    period_days: Vec<String>,
    project_color: HashMap<String, &'static str>,
}

/// `WorkReportPdfGenerator.generate`: bytes PDF.
pub fn generate(data: &ReportData) -> Vec<u8> {
    let title = format!(
        "Work Hours & Activity Report {} - {}",
        data.start_date, data.end_date
    );
    let mut g = WorkReportPdf {
        data,
        pdf: Canvas::new(&title, &clean_text(Some(&data.company_name)), "Timemoz"),
        period_days: enumerate_days(&data.start_date, &data.end_date),
        project_color: HashMap::new(),
    };
    g.render();
    g.pdf.draw_footers(&FooterOptions::new(
        &data.company_name,
        "Work Hours & Activity Report",
    ));
    g.pdf.doc.finish()
}

impl WorkReportPdf<'_> {
    fn render(&mut self) {
        let employees: Vec<&Employee> = self
            .data
            .employees
            .iter()
            .filter(|e| e.seconds > 0 || !e.days.is_empty())
            .collect();
        let stats: Vec<EmployeeStat<'_>> = employees.iter().map(|e| employee_stat(e)).collect();
        let projects = project_stats(&employees);
        for p in &projects {
            self.project_color.insert(p.name.clone(), p.color);
        }
        let apps = team_apps(&stats);
        let total_seconds: i64 = employees.iter().map(|e| e.seconds).sum();

        self.draw_cover_header(employees.len(), projects.len());

        if employees.is_empty() {
            self.pdf.empty_state(
                "No time tracked in this period",
                "Try a different date range or check the selected employees and projects.",
            );
            return;
        }

        let sections = self.data.sections;
        if sections.summary {
            self.draw_executive_summary(&stats, &projects, total_seconds);
            self.draw_daily_chart(&employees);
        }
        if sections.employees {
            self.draw_team_summary(&stats);
        }
        if sections.projects {
            self.draw_project_breakdown(&projects, total_seconds);
        }
        if sections.employees && !apps.is_empty() {
            self.draw_top_apps(&apps);
        }
        if sections.employees {
            for s in &stats {
                self.draw_employee_timesheet(s);
                if s.app_seconds > 0 {
                    self.draw_employee_activity(s);
                }
            }
        }
    }

    fn draw_cover_header(&mut self, employee_count: usize, project_count: usize) {
        let days = self.period_days.len();
        let meta = [
            (
                "Generated on",
                f_generated_at(self.data.generated_at, self.data.time_zone.as_deref()),
            ),
            ("Generated by", {
                let g = clean_text(Some(&self.data.generated_by));
                if g.is_empty() { "-".to_owned() } else { g }
            }),
            (
                "Team",
                format!(
                    "{} · {}",
                    plural(employee_count, "employee"),
                    plural(project_count, "project")
                ),
            ),
        ];
        self.pdf.cover_header(&CoverOptions {
            title: "WORK HOURS & ACTIVITY REPORT",
            company_name: &self.data.company_name,
            company_fallback: None,
            period_label: None,
            period_from: &f_date(&self.data.start_date),
            period_to: &f_date(&self.data.end_date),
            period_hint: &plural(days, "day"),
            meta: &meta,
        });
    }

    fn draw_executive_summary(
        &mut self,
        stats: &[EmployeeStat<'_>],
        projects: &[ProjectStat],
        total_seconds: i64,
    ) {
        let person_days: usize = stats.iter().map(|s| s.active_days).sum();
        self.pdf.section_title(
            "Executive Summary",
            Some("Key figures for the whole team during the reporting period."),
        );
        #[allow(clippy::cast_precision_loss)]
        let avg = if person_days > 0 {
            secs(total_seconds) / person_days as f64
        } else {
            0.0
        };
        self.pdf.kpi_cards(&[
            KpiCard {
                label: "Total hours tracked".into(),
                value: f_dur(secs(total_seconds)),
                hint: Some(format!(
                    "{} with activity",
                    plural(person_days, "person-day")
                )),
                color: PRIMARY,
            },
            KpiCard {
                label: "Avg per employee / day".into(),
                value: f_dur(avg),
                hint: Some("Across days with tracked time".into()),
                color: CYAN,
            },
            KpiCard {
                label: "Team members".into(),
                value: stats.len().to_string(),
                hint: Some("With tracked time".into()),
                color: VIOLET,
            },
            KpiCard {
                label: "Projects".into(),
                value: projects.len().to_string(),
                hint: Some(match projects.first() {
                    Some(p) => format!("Top: {}", clean_text(Some(&p.name))),
                    None => "No projects".into(),
                }),
                color: AMBER,
            },
        ]);
    }

    fn draw_daily_chart(&mut self, employees: &[&Employee]) {
        let mut per_day: HashMap<&str, i64> = HashMap::new();
        for e in employees {
            for d in &e.days {
                *per_day.entry(d.date.as_str()).or_insert(0) += d.seconds;
            }
        }
        let (buckets, weekly) = chart_buckets(&self.period_days, |d| {
            secs(per_day.get(d).copied().unwrap_or(0))
        });
        self.pdf.section_title(
            if weekly {
                "Weekly Team Hours"
            } else {
                "Daily Team Hours"
            },
            Some(if weekly {
                "Total tracked hours of all employees per week (weeks start on Monday)."
            } else {
                "Total tracked hours of all employees per day."
            }),
        );
        self.pdf.bar_chart(&buckets, ChartOptions::default());
    }

    fn name_with_meta(&mut self, name: Option<&str>, meta: Option<String>, x: f64, width: f64) {
        let Some(name) = name else {
            self.pdf
                .txt("-", x, self.pdf.y + 12.0, TextStyle::new(8.5, FAINT));
            return;
        };
        let n = self
            .pdf
            .fit(&clean_text(Some(name)), width, Font::Bold, 7.8);
        self.pdf
            .txt(&n, x, self.pdf.y + 7.0, TextStyle::new(7.8, TEXT).bold());
        if let Some(m) = meta {
            self.pdf
                .txt(&m, x, self.pdf.y + 18.0, TextStyle::new(7.0, FAINT));
        }
    }

    fn draw_team_summary(&mut self, stats: &[EmployeeStat<'_>]) {
        let has_apps = stats.iter().any(|s| s.app_seconds > 0);
        self.pdf.section_title(
            "Team Summary",
            Some(if has_apps {
                "Hours, active days and most used application per employee."
            } else {
                "Hours, active days and projects per employee."
            }),
        );
        let cols = [
            Column::new("Employee", 146.0, Align::Left),
            Column::new("Days", 40.0, Align::Center),
            Column::new("Total", 58.0, Align::Right),
            Column::new("Avg / day", 56.0, Align::Right),
            if has_apps {
                Column::new("Top application", 98.0, Align::Left)
            } else {
                Column::new("Projects", 98.0, Align::Center)
            },
            Column::new("Main project", CW - 398.0, Align::Left),
        ];
        self.pdf.ensure(22.0 + 32.0);
        self.pdf.table_header(&cols);

        for (i, s) in stats.iter().enumerate() {
            let e = s.employee;
            let rh = 32.0;
            if self.pdf.ensure(rh) {
                self.pdf.table_header(&cols);
            }
            if i % 2 == 1 {
                self.pdf.doc.rect(M, self.pdf.y, CW, rh, SURFACE);
            }
            let mut x = M;
            self.pdf.avatar(
                &e.name,
                x + 20.0,
                self.pdf.y + rh / 2.0,
                10.0,
                PRIMARY_SOFT,
                PRIMARY,
                7.5,
            );
            let n = self.pdf.fit(
                &clean_text(Some(&e.name)),
                cols[0].w - 42.0,
                Font::Bold,
                8.5,
            );
            self.pdf.txt(
                &n,
                x + 36.0,
                self.pdf.y + 7.0,
                TextStyle::new(8.5, DARK).bold(),
            );
            let em = clean_text(e.email.as_deref());
            let em = self.pdf.fit(
                if em.is_empty() { "-" } else { &em },
                cols[0].w - 42.0,
                Font::Regular,
                7.5,
            );
            self.pdf
                .txt(&em, x + 36.0, self.pdf.y + 18.0, TextStyle::new(7.5, MUTED));
            x += cols[0].w;
            self.pdf.txt(
                &s.active_days.to_string(),
                x,
                self.pdf.y + 12.0,
                TextStyle::new(8.5, TEXT).width(cols[1].w, Align::Center),
            );
            x += cols[1].w;
            self.pdf.txt(
                &f_dur(secs(e.seconds)),
                x,
                self.pdf.y + 12.0,
                TextStyle::new(8.5, DARK)
                    .bold()
                    .width(cols[2].w - 8.0, Align::Right),
            );
            x += cols[2].w;
            #[allow(clippy::cast_precision_loss)]
            let avg = if s.active_days > 0 {
                secs(e.seconds) / s.active_days as f64
            } else {
                0.0
            };
            self.pdf.txt(
                &f_dur(avg),
                x,
                self.pdf.y + 12.0,
                TextStyle::new(8.5, TEXT).width(cols[3].w - 8.0, Align::Right),
            );
            x += cols[3].w;
            if has_apps {
                let top = s.app_totals.first();
                let meta = top.map(|a| {
                    format!(
                        "{} · {}",
                        f_dur(secs(a.seconds)),
                        pct(secs(a.seconds) / secs(s.app_seconds))
                    )
                });
                self.name_with_meta(
                    top.map(|a| a.name.as_str()),
                    meta,
                    x + 8.0,
                    cols[4].w - 14.0,
                );
            } else {
                self.pdf.txt(
                    &s.projects.len().to_string(),
                    x,
                    self.pdf.y + 12.0,
                    TextStyle::new(8.5, TEXT).width(cols[4].w, Align::Center),
                );
            }
            x += cols[4].w;
            let main = s.projects.first();
            let denom = if e.seconds == 0 { 1.0 } else { secs(e.seconds) };
            let meta =
                main.map(|(_, v)| format!("{} · {}", f_dur(secs(*v)), pct(secs(*v) / denom)));
            self.name_with_meta(
                main.map(|(n, _)| n.as_str()),
                meta,
                x + 8.0,
                cols[5].w - 14.0,
            );
            self.pdf.y += rh;
        }
        self.pdf.hr_default();
        self.pdf.y += 18.0;
    }

    fn draw_project_breakdown(&mut self, projects: &[ProjectStat], total_seconds: i64) {
        let Some(first) = projects.first() else {
            return;
        };
        self.pdf
            .section_title("Project Breakdown", Some("Where the team spent its time."));
        let cols = [
            Column::new("Project", 200.0, Align::Left),
            Column::new("Team members", 140.0, Align::Left),
            Column::new("Hours", 65.0, Align::Right),
            Column::new("Share of total", CW - 405.0, Align::Left),
        ];
        self.pdf.ensure(22.0 + 22.0);
        self.pdf.table_header(&cols);
        let top = if first.seconds == 0 {
            1.0
        } else {
            secs(first.seconds)
        };
        let total = if total_seconds == 0 {
            1.0
        } else {
            secs(total_seconds)
        };

        for (i, p) in projects.iter().enumerate() {
            let rh = 22.0;
            if self.pdf.ensure(rh) {
                self.pdf.table_header(&cols);
            }
            if i % 2 == 1 {
                self.pdf.doc.rect(M, self.pdf.y, CW, rh, SURFACE);
            }
            self.pdf
                .doc
                .circle(M + 12.0, self.pdf.y + rh / 2.0, 3.5, p.color);
            let n = self
                .pdf
                .fit(&clean_text(Some(&p.name)), 172.0, Font::Bold, 8.5);
            self.pdf.txt(
                &n,
                M + 22.0,
                self.pdf.y + 7.0,
                TextStyle::new(8.5, DARK).bold(),
            );
            let first_names: Vec<String> = p
                .members
                .iter()
                .map(|n| {
                    clean_text(Some(n))
                        .split(' ')
                        .next()
                        .unwrap_or("")
                        .to_owned()
                })
                .collect();
            let members = if first_names.len() > 3 {
                format!(
                    "{} +{}",
                    first_names
                        .iter()
                        .take(3)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", "),
                    first_names.len() - 3
                )
            } else {
                first_names.join(", ")
            };
            let members = self.pdf.fit(&members, 128.0, Font::Regular, 8.0);
            self.pdf.txt(
                &members,
                M + 206.0,
                self.pdf.y + 7.5,
                TextStyle::new(8.0, MUTED),
            );
            self.pdf.txt(
                &f_dur(secs(p.seconds)),
                M + 340.0,
                self.pdf.y + 7.0,
                TextStyle::new(8.5, DARK).bold().width(57.0, Align::Right),
            );
            self.pdf.bar(
                M + 415.0,
                self.pdf.y + 8.0,
                60.0,
                6.0,
                secs(p.seconds) / top,
                p.color,
            );
            self.pdf.txt(
                &pct(secs(p.seconds) / total),
                M + 482.0,
                self.pdf.y + 7.0,
                TextStyle::new(8.0, TEXT),
            );
            self.pdf.y += rh;
        }
        self.pdf.hr_default();
        self.pdf.y += 18.0;
    }

    fn draw_top_apps(&mut self, apps: &[(String, i64, usize)]) {
        self.pdf.section_title(
            "Top Applications",
            Some("Applications the team spent the most time in, from the Timemoz desktop app."),
        );
        let cols = [
            Column::new("#", 24.0, Align::Center),
            Column::new("Application", 170.0, Align::Left),
            Column::new("Used by", 70.0, Align::Center),
            Column::new("Usage", CW - 24.0 - 170.0 - 70.0 - 70.0 - 50.0, Align::Left),
            Column::new("Duration", 70.0, Align::Right),
            Column::new("Share", 50.0, Align::Right),
        ];
        self.pdf.ensure(22.0 + 20.0);
        self.pdf.table_header(&cols);
        let total = {
            let t: i64 = apps.iter().map(|a| a.1).sum();
            if t == 0 { 1.0 } else { secs(t) }
        };
        let top = apps
            .first()
            .map_or(1.0, |a| if a.1 == 0 { 1.0 } else { secs(a.1) });

        for (i, (name, seconds, users)) in apps.iter().take(10).enumerate() {
            let rh = 20.0;
            if self.pdf.ensure(rh) {
                self.pdf.table_header(&cols);
            }
            if i % 2 == 1 {
                self.pdf.doc.rect(M, self.pdf.y, CW, rh, SURFACE);
            }
            let mut x = M;
            self.pdf.txt(
                &(i + 1).to_string(),
                x,
                self.pdf.y + 6.5,
                TextStyle::new(8.0, FAINT).width(cols[0].w, Align::Center),
            );
            x += cols[0].w;
            let n = self
                .pdf
                .fit(&clean_text(Some(name)), cols[1].w - 12.0, Font::Bold, 8.5);
            self.pdf.txt(
                &n,
                x + 6.0,
                self.pdf.y + 6.0,
                TextStyle::new(8.5, DARK).bold(),
            );
            x += cols[1].w;
            self.pdf.txt(
                &format!("{users} {}", if *users > 1 { "people" } else { "person" }),
                x,
                self.pdf.y + 6.5,
                TextStyle::new(8.0, MUTED).width(cols[2].w, Align::Center),
            );
            x += cols[2].w;
            self.pdf.bar(
                x + 6.0,
                self.pdf.y + 8.0,
                cols[3].w - 12.0,
                5.0,
                secs(*seconds) / top,
                PRIMARY,
            );
            x += cols[3].w;
            self.pdf.txt(
                &f_dur(secs(*seconds)),
                x,
                self.pdf.y + 6.0,
                TextStyle::new(8.5, DARK)
                    .bold()
                    .width(cols[4].w - 6.0, Align::Right),
            );
            x += cols[4].w;
            self.pdf.txt(
                &pct(secs(*seconds) / total),
                x,
                self.pdf.y + 6.0,
                TextStyle::new(8.5, MUTED).width(cols[5].w - 6.0, Align::Right),
            );
            self.pdf.y += rh;
        }
        self.pdf.hr_default();
        self.pdf.y += 18.0;
    }

    fn draw_employee_header(&mut self, e: &Employee, label: &str) {
        let period = format!(
            "{} - {}",
            f_date(&self.data.start_date),
            f_date(&self.data.end_date)
        );
        self.pdf
            .person_header(&e.name, e.email.as_deref(), label, &period);
    }

    fn draw_employee_timesheet(&mut self, s: &EmployeeStat<'_>) {
        let e = s.employee;
        self.pdf.new_page();
        self.draw_employee_header(e, "Timesheet");
        #[allow(clippy::cast_precision_loss)]
        let avg = if s.active_days > 0 {
            secs(e.seconds) / s.active_days as f64
        } else {
            0.0
        };
        self.pdf.kpi_cards(&[
            KpiCard {
                label: "Total hours".into(),
                value: f_dur(secs(e.seconds)),
                hint: Some(plural(s.projects.len(), "project")),
                color: PRIMARY,
            },
            KpiCard {
                label: "Active days".into(),
                value: format!("{}/{}", s.active_days, self.period_days.len()),
                hint: Some("Days with tracked time".into()),
                color: CYAN,
            },
            KpiCard {
                label: "Average per day".into(),
                value: f_dur(avg),
                hint: Some("Per active day".into()),
                color: VIOLET,
            },
            match s.app_totals.first() {
                Some(a) => KpiCard {
                    label: "Applications used".into(),
                    value: s.app_totals.len().to_string(),
                    hint: Some(format!("Top: {}", clean_text(Some(&a.name)))),
                    color: GREEN,
                },
                None => KpiCard {
                    label: "Applications used".into(),
                    value: "-".into(),
                    hint: Some("No desktop activity".into()),
                    color: GREEN,
                },
            },
        ]);

        if self.period_days.len() <= 7 {
            self.draw_week_at_a_glance(e);
        }

        self.pdf.section_title(
            "Daily Timesheet",
            Some("Time entries recorded by the employee, grouped per day."),
        );
        let cols = [
            Column::new("Date", 76.0, Align::Left),
            Column::new("Time", 62.0, Align::Left),
            Column::new("Project", 142.0, Align::Left),
            Column::new("Notes", CW - 76.0 - 62.0 - 142.0 - 52.0, Align::Left),
            Column::new("Duration", 52.0, Align::Right),
        ];
        let (col_date, col_time, col_project, col_notes, col_duration) =
            (&cols[0], &cols[1], &cols[2], &cols[3], &cols[4]);
        self.pdf.ensure(22.0 + 24.0);
        self.pdf.table_header(&cols);

        let days: Vec<&Day> = e.days.iter().filter(|d| !d.entries.is_empty()).collect();
        for (di, d) in days.iter().enumerate() {
            let min_h: f64 = if d.entries.len() == 1 { 30.0 } else { 22.0 };
            struct Row<'e> {
                entry: &'e Entry,
                note: String,
                project: String,
                h: f64,
            }
            let rows: Vec<Row<'_>> = d
                .entries
                .iter()
                .map(|entry| {
                    let notes = {
                        let n = clean_text(Some(&entry.notes));
                        if n.is_empty() { "-".to_owned() } else { n }
                    };
                    let note = if notes.chars().count() > MAX_NOTE_CHARS {
                        format!(
                            "{}...",
                            notes
                                .chars()
                                .take(MAX_NOTE_CHARS)
                                .collect::<String>()
                                .trim_end()
                        )
                    } else {
                        notes
                    };
                    let project = clean_text(Some(&entry.project));
                    let note_h =
                        self.pdf
                            .height_of(&note, col_notes.w - 16.0, Font::Regular, 8.0, 0.0);
                    let proj_h =
                        self.pdf
                            .height_of(&project, col_project.w - 22.0, Font::Bold, 7.8, 0.0);
                    Row {
                        entry,
                        note,
                        project,
                        h: min_h.max(note_h.max(proj_h) + 12.0),
                    }
                })
                .collect();

            // Usahakan satu hari utuh di satu halaman.
            let group_h: f64 = rows.iter().map(|r| r.h).sum();
            if group_h <= self.pdf.bottom() - M - 22.0 && self.pdf.y + group_h > self.pdf.bottom() {
                self.pdf.new_page();
                self.pdf.table_header(&cols);
            }

            let mut segment_top = self.pdf.y;
            let mut continued = false;
            let shade = di % 2 == 1;
            let day_label = f_day(&d.date);
            let total_label = format!("{} total", f_dur(secs(d.seconds)));

            for (k, r) in rows.iter().enumerate() {
                if self.pdf.y + r.h > self.pdf.bottom() {
                    self.draw_date_label(&day_label, &total_label, segment_top, continued);
                    self.pdf.new_page();
                    self.pdf.table_header(&cols);
                    segment_top = self.pdf.y;
                    continued = true;
                }
                if shade {
                    self.pdf.doc.rect(M, self.pdf.y, CW, r.h, SURFACE);
                }
                let mut x = M + col_date.w;
                let time = match (&r.entry.start, &r.entry.end) {
                    (Some(a), Some(b)) => format!("{a} - {b}"),
                    _ => "-".to_owned(),
                };
                self.pdf
                    .txt(&time, x + 8.0, self.pdf.y + 8.0, TextStyle::new(8.0, TEXT));
                x += col_time.w;
                let color = self
                    .project_color
                    .get(&r.entry.project)
                    .copied()
                    .unwrap_or(MUTED);
                self.pdf.doc.circle(x + 11.0, self.pdf.y + 10.5, 3.0, color);
                self.pdf.txt(
                    &r.project,
                    x + 18.0,
                    self.pdf.y + 8.0,
                    TextStyle::new(7.8, DARK)
                        .bold()
                        .width(col_project.w - 22.0, Align::Left),
                );
                x += col_project.w;
                self.pdf.txt(
                    &r.note,
                    x + 8.0,
                    self.pdf.y + 8.0,
                    TextStyle::new(8.0, TEXT).width(col_notes.w - 16.0, Align::Left),
                );
                x += col_notes.w;
                self.pdf.txt(
                    &f_dur(secs(r.entry.seconds)),
                    x,
                    self.pdf.y + 8.0,
                    TextStyle::new(8.0, DARK)
                        .bold()
                        .width(col_duration.w - 8.0, Align::Right),
                );
                self.pdf.y += r.h;
                if k + 1 < rows.len() {
                    self.pdf.hr(0.4, M + col_date.w, CW - col_date.w);
                }
            }
            self.draw_date_label(&day_label, &total_label, segment_top, continued);
            self.pdf.hr(0.6, M, CW);
        }

        self.pdf
            .total_bar("TOTAL FOR PERIOD", &f_dur(secs(e.seconds)));
        self.draw_project_split(s);

        if s.app_seconds == 0 && self.pdf.y + 56.0 <= self.pdf.bottom() {
            self.pdf.doc.rrect(M, self.pdf.y, CW, 44.0, 6.0, SURFACE);
            self.pdf.txt(
                "No application activity recorded",
                M + 14.0,
                self.pdf.y + 10.0,
                TextStyle::new(8.5, DARK).bold(),
            );
            self.pdf.txt(
                "Application usage is captured when the employee tracks time with the Timemoz desktop app.",
                M + 14.0,
                self.pdf.y + 24.0,
                TextStyle::new(8.0, MUTED),
            );
            self.pdf.y += 56.0;
        }
    }

    /// Label tanggal digambar setelah baris-barisnya (shading baris tidak menimpa).
    fn draw_date_label(&mut self, day: &str, total: &str, segment_top: f64, continued: bool) {
        self.pdf.txt(
            day,
            M + 8.0,
            segment_top + 8.0,
            TextStyle::new(8.0, DARK).bold(),
        );
        self.pdf.txt(
            if continued { "(continued)" } else { total },
            M + 8.0,
            segment_top + 19.0,
            TextStyle::new(7.5, if continued { FAINT } else { PRIMARY }),
        );
    }

    fn draw_week_at_a_glance(&mut self, e: &Employee) {
        let by_date: HashMap<&str, i64> = e
            .days
            .iter()
            .map(|d| (d.date.as_str(), d.seconds))
            .collect();
        let max_day = e.days.iter().map(|d| d.seconds).max().unwrap_or(1).max(1);
        let gap = 8.0;
        #[allow(clippy::cast_precision_loss)]
        let n = self.period_days.len() as f64;
        let gw = (CW - gap * (n - 1.0)) / n;
        self.pdf.ensure(58.0);
        let days = self.period_days.clone();
        for (i, date) in days.iter().enumerate() {
            #[allow(clippy::cast_precision_loss)]
            let x = M + i as f64 * (gw + gap);
            let seconds = by_date.get(date.as_str()).copied().unwrap_or(0);
            self.pdf.doc.rrect(
                x,
                self.pdf.y,
                gw,
                44.0,
                6.0,
                if seconds > 0 { SURFACE } else { SURFACE2 },
            );
            let label = self.pdf.fit(&f_day(date), gw - 16.0, Font::Bold, 7.5);
            self.pdf.txt(
                &label,
                x + 8.0,
                self.pdf.y + 8.0,
                TextStyle::new(7.5, MUTED).bold(),
            );
            if seconds > 0 {
                self.pdf.txt(
                    &f_dur(secs(seconds)),
                    x + 8.0,
                    self.pdf.y + 19.0,
                    TextStyle::new(10.0, DARK).bold(),
                );
                self.pdf.bar(
                    x + 8.0,
                    self.pdf.y + 34.0,
                    gw - 16.0,
                    4.0,
                    secs(seconds) / secs(max_day),
                    PRIMARY,
                );
            } else {
                self.pdf.txt(
                    "No activity",
                    x + 8.0,
                    self.pdf.y + 21.0,
                    TextStyle::new(9.0, FAINT).bold(),
                );
            }
        }
        self.pdf.y += 58.0;
    }

    fn draw_project_split(&mut self, s: &EmployeeStat<'_>) {
        if s.projects.is_empty() {
            return;
        }
        let total = if s.employee.seconds == 0 {
            1.0
        } else {
            secs(s.employee.seconds)
        };
        self.pdf.ensure(56.0);
        self.pdf.txt(
            "TIME BY PROJECT",
            M,
            self.pdf.y,
            TextStyle::new(7.0, MUTED).bold(),
        );
        self.pdf.y += 12.0;
        self.pdf.doc.rrect(M, self.pdf.y, CW, 10.0, 5.0, SURFACE2);
        let mut px = M;
        for (name, v) in &s.projects {
            let w = secs(*v) / total * CW;
            let color = self.project_color.get(name).copied().unwrap_or(MUTED);
            self.pdf.doc.rect(px, self.pdf.y, w, 10.0, color);
            px += w;
        }
        self.pdf.y += 18.0;

        let mut lx = M;
        for (name, v) in &s.projects {
            let label = format!(
                "{}  ",
                self.pdf
                    .fit(&clean_text(Some(name)), 200.0, Font::Bold, 8.0)
            );
            let value = format!("{} · {}", f_dur(secs(*v)), pct(secs(*v) / total));
            let lw = super::doc::text_width(&label, Font::Bold, 8.0);
            let vw = super::doc::text_width(&value, Font::Regular, 8.0);
            if lx > M && lx + 12.0 + lw + vw > M + CW {
                lx = M;
                self.pdf.y += 14.0;
                self.pdf.ensure(14.0);
            }
            let color = self.project_color.get(name).copied().unwrap_or(MUTED);
            self.pdf
                .doc
                .rrect(lx, self.pdf.y + 1.0, 8.0, 8.0, 2.0, color);
            self.pdf.txt(
                &label,
                lx + 12.0,
                self.pdf.y + 1.0,
                TextStyle::new(8.0, DARK).bold(),
            );
            self.pdf.txt(
                &value,
                lx + 12.0 + lw,
                self.pdf.y + 1.0,
                TextStyle::new(8.0, MUTED),
            );
            lx += 12.0 + lw + vw + 22.0;
        }
        self.pdf.y += 24.0;
    }

    fn draw_employee_activity(&mut self, s: &EmployeeStat<'_>) {
        let e = s.employee;
        self.pdf.new_page();
        self.draw_employee_header(e, "Application Activity Log");

        let app_days: Vec<&Day> = e.days.iter().filter(|d| !d.apps.is_empty()).collect();
        #[allow(clippy::cast_precision_loss)]
        let avg_apps = if app_days.is_empty() {
            0.0
        } else {
            app_days.iter().map(|d| d.apps.len()).sum::<usize>() as f64 / app_days.len() as f64
        };
        let total = secs(s.app_seconds);
        let Some(top_app) = s.app_totals.first() else {
            return;
        };
        self.pdf.kpi_cards(&[
            KpiCard {
                label: "Total active app time".into(),
                value: f_dur(total),
                hint: Some(format!("{} with activity", plural(app_days.len(), "day"))),
                color: PRIMARY,
            },
            KpiCard {
                label: "Applications used".into(),
                value: s.app_totals.len().to_string(),
                hint: Some(format!(
                    "About {} per day",
                    super::format::js_round(avg_apps)
                )),
                color: VIOLET,
            },
            KpiCard {
                label: "Most used".into(),
                value: pct(secs(top_app.seconds) / total),
                hint: Some(clean_text(Some(&top_app.name))),
                color: CYAN,
            },
        ]);

        self.pdf.section_title(
            "Application Usage",
            Some("Total time each application was active during the period."),
        );
        let cols = [
            Column::new("#", 24.0, Align::Center),
            Column::new("Application", 170.0, Align::Left),
            Column::new("Usage", CW - 24.0 - 170.0 - 62.0 - 46.0, Align::Left),
            Column::new("Duration", 62.0, Align::Right),
            Column::new("Share", 46.0, Align::Right),
        ];
        self.pdf.table_header(&cols);
        let shown: Vec<&AppUsage> = s.app_totals.iter().take(TOP_APPS_PER_EMPLOYEE).collect();
        let top_seconds = shown
            .first()
            .map_or(1.0, |a| if a.seconds == 0 { 1.0 } else { secs(a.seconds) });
        for (i, a) in shown.iter().enumerate() {
            let rh = 18.0;
            if self.pdf.ensure(rh) {
                self.pdf.table_header(&cols);
            }
            if i % 2 == 1 {
                self.pdf.doc.rect(M, self.pdf.y, CW, rh, SURFACE);
            }
            let mut x = M;
            self.pdf.txt(
                &(i + 1).to_string(),
                x,
                self.pdf.y + 6.0,
                TextStyle::new(8.0, FAINT).width(cols[0].w, Align::Center),
            );
            x += cols[0].w;
            let n = self.pdf.fit(
                &clean_text(Some(&a.name)),
                cols[1].w - 14.0,
                Font::Bold,
                8.5,
            );
            self.pdf.txt(
                &n,
                x + 6.0,
                self.pdf.y + 5.5,
                TextStyle::new(8.5, DARK).bold(),
            );
            x += cols[1].w;
            self.pdf.bar(
                x + 6.0,
                self.pdf.y + 7.0,
                cols[2].w - 12.0,
                5.0,
                secs(a.seconds) / top_seconds,
                PRIMARY,
            );
            x += cols[2].w;
            self.pdf.txt(
                &f_dur(secs(a.seconds)),
                x,
                self.pdf.y + 5.5,
                TextStyle::new(8.5, DARK)
                    .bold()
                    .width(cols[3].w - 6.0, Align::Right),
            );
            x += cols[3].w;
            self.pdf.txt(
                &pct(secs(a.seconds) / total),
                x,
                self.pdf.y + 5.5,
                TextStyle::new(8.5, MUTED).width(cols[4].w - 6.0, Align::Right),
            );
            self.pdf.y += rh;
        }
        let rest: Vec<&AppUsage> = s.app_totals.iter().skip(TOP_APPS_PER_EMPLOYEE).collect();
        if !rest.is_empty() {
            let rest_seconds: i64 = rest.iter().map(|a| a.seconds).sum();
            self.pdf.ensure(19.0);
            let names = rest
                .iter()
                .map(|a| clean_text(Some(&a.name)))
                .collect::<Vec<_>>()
                .join(", ");
            let text = self.pdf.fit(
                &format!("Other ({} apps): {names}", rest.len()),
                380.0,
                Font::Regular,
                7.5,
            );
            self.pdf.txt(
                &text,
                M + 30.0,
                self.pdf.y + 5.5,
                TextStyle::new(7.5, MUTED),
            );
            self.pdf.txt(
                &f_dur(secs(rest_seconds)),
                M + CW - 108.0,
                self.pdf.y + 5.5,
                TextStyle::new(8.0, MUTED).width(54.0, Align::Right),
            );
            self.pdf.txt(
                &pct(secs(rest_seconds) / total),
                M + CW - 46.0,
                self.pdf.y + 5.5,
                TextStyle::new(8.0, MUTED).width(40.0, Align::Right),
            );
            self.pdf.y += 19.0;
        }
        self.pdf.hr_default();
        self.pdf.y += 14.0;

        self.draw_daily_activity_cards(s, &app_days);
    }

    fn draw_daily_activity_cards(&mut self, s: &EmployeeStat<'_>, app_days: &[&Day]) {
        self.pdf.section_title(
            "Daily Activity Log",
            Some("Applications used each day and how long each one was active."),
        );
        let gap = 10.0;
        let cw = (CW - gap) / 2.0;
        let rows = 6usize;
        #[allow(clippy::cast_precision_loss)]
        let card_h = 30.0 + rows as f64 * 13.0 + 8.0;

        // Kartu highlight menempati slot terakhir di grid dua kolom.
        let slots = app_days.len() + 1;
        for i in 0..slots {
            let col = i % 2;
            if col == 0 {
                self.pdf.ensure(card_h);
            }
            #[allow(clippy::cast_precision_loss)]
            let x = M + col as f64 * (cw + gap);
            if let Some(d) = app_days.get(i) {
                self.draw_day_card(d, x, cw, card_h, rows);
            } else {
                self.draw_highlights(s, x, cw, card_h);
            }
            if col == 1 || i == slots - 1 {
                self.pdf.y += card_h + gap;
            }
        }
    }

    fn draw_day_card(&mut self, d: &Day, x: f64, cw: f64, card_h: f64, rows: usize) {
        let y = self.pdf.y;
        self.pdf
            .doc
            .rrect_stroke(x, y, cw, card_h, 6.0, WHITE, BORDER);
        self.pdf.doc.rrect(x, y, cw, 26.0, 6.0, SURFACE);
        self.pdf.doc.rect(x, y + 20.0, cw, 6.0, SURFACE);

        let day_label = f_day(&d.date);
        let app_seconds: i64 = d.apps.iter().map(|a| a.seconds).sum();
        self.pdf.txt(
            &day_label,
            x + 10.0,
            y + 9.0,
            TextStyle::new(8.5, DARK).bold(),
        );
        let lw = super::doc::text_width(&day_label, Font::Bold, 8.5);
        self.pdf.txt(
            &format!("·  {}", f_dur(secs(app_seconds))),
            x + 14.0 + lw,
            y + 9.0,
            TextStyle::new(8.5, PRIMARY).bold(),
        );
        self.pdf.txt(
            &plural(d.apps.len(), "app"),
            x + cw - 70.0,
            y + 9.5,
            TextStyle::new(7.5, MUTED).width(60.0, Align::Right),
        );

        let mut ry = y + 32.0;
        let has_other = d.apps.len() > rows;
        let shown_n = if has_other { rows - 1 } else { rows };
        let top = d
            .apps
            .first()
            .map_or(1.0, |a| if a.seconds == 0 { 1.0 } else { secs(a.seconds) });
        for a in d.apps.iter().take(shown_n) {
            self.pdf.doc.circle(x + 14.0, ry + 4.0, 2.6, PRIMARY);
            let n = self
                .pdf
                .fit(&clean_text(Some(&a.name)), 110.0, Font::Regular, 8.0);
            self.pdf.txt(&n, x + 22.0, ry, TextStyle::new(8.0, DARK));
            self.pdf.bar(
                x + 138.0,
                ry + 1.5,
                cw - 200.0,
                5.0,
                secs(a.seconds) / top,
                PRIMARY,
            );
            self.pdf.txt(
                &f_dur(secs(a.seconds)),
                x + cw - 56.0,
                ry,
                TextStyle::new(8.0, DARK).bold().width(46.0, Align::Right),
            );
            ry += 13.0;
        }
        if has_other {
            let rest: Vec<&AppUsage> = d.apps.iter().skip(rows - 1).collect();
            let seconds: i64 = rest.iter().map(|a| a.seconds).sum();
            self.pdf.doc.circle(x + 14.0, ry + 4.0, 2.6, FAINT);
            self.pdf.txt(
                &format!("Other ({} apps)", rest.len()),
                x + 22.0,
                ry,
                TextStyle::new(8.0, MUTED),
            );
            self.pdf.txt(
                &f_dur(secs(seconds)),
                x + cw - 56.0,
                ry,
                TextStyle::new(8.0, MUTED).width(46.0, Align::Right),
            );
        }
    }

    fn draw_highlights(&mut self, s: &EmployeeStat<'_>, x: f64, cw: f64, card_h: f64) {
        let total = secs(s.app_seconds);
        let Some(first) = s.app_totals.first() else {
            return;
        };
        let top3: Vec<&AppUsage> = s.app_totals.iter().take(3).collect();
        let longest = s
            .employee
            .days
            .iter()
            .fold(None::<&Day>, |best, d| match best {
                Some(b) if d.seconds <= b.seconds => Some(b),
                _ => Some(d),
            });
        let mut notes = vec![format!(
            "Most used: {}, {} ({} of app time).",
            clean_text(Some(&first.name)),
            f_dur(secs(first.seconds)),
            pct(secs(first.seconds) / total)
        )];
        if let Some(l) = longest {
            notes.push(format!(
                "Longest day: {} with {} tracked.",
                f_day(&l.date),
                f_dur(secs(l.seconds))
            ));
        }
        if top3.len() > 1 {
            let covered: i64 = top3.iter().map(|a| a.seconds).sum();
            notes.push(format!(
                "Top {} apps cover {} of app time.",
                top3.len(),
                pct(secs(covered) / total)
            ));
        }

        let y = self.pdf.y;
        self.pdf.doc.rrect(x, y, cw, card_h, 6.0, PRIMARY_SOFT);
        self.pdf.txt(
            "Highlights",
            x + 12.0,
            y + 10.0,
            TextStyle::new(8.5, NAVY).bold(),
        );
        let mut hy = y + 28.0;
        for n in notes {
            let h = self.pdf.height_of(&n, cw - 34.0, Font::Regular, 7.8, 1.0);
            if hy + h > y + card_h - 4.0 {
                continue;
            }
            self.pdf.doc.circle(x + 15.0, hy + 3.5, 1.6, NAVY);
            self.pdf.txt(
                &n,
                x + 22.0,
                hy,
                TextStyle::new(7.8, TEXT)
                    .width(cw - 34.0, Align::Left)
                    .gap(1.0),
            );
            hy += h + 6.0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn sample() -> ReportData {
        let day = |date: &str, entries: Vec<(&str, &str, i64)>, apps: Vec<(&str, i64)>| Day {
            date: date.into(),
            seconds: entries.iter().map(|e| e.2).sum(),
            entries: entries
                .into_iter()
                .map(|(p, n, s)| Entry {
                    start: Some("09:00".into()),
                    end: Some("10:00".into()),
                    project: p.into(),
                    notes: n.into(),
                    seconds: s,
                })
                .collect(),
            apps: apps
                .into_iter()
                .map(|(n, s)| AppUsage {
                    name: n.into(),
                    seconds: s,
                })
                .collect(),
        };
        let a = Employee {
            id: 1,
            name: "Maria Wijaya".into(),
            email: Some("maria@example.com".into()),
            seconds: 7200 + 1800,
            days: vec![
                day(
                    "2026-10-05",
                    vec![("Alpha", "Fix bug (urgent) é", 7200)],
                    vec![("Chrome", 3600), ("Code", 1200)],
                ),
                day("2026-10-06", vec![("Beta", "-", 1800)], vec![]),
            ],
        };
        let b = Employee {
            id: 2,
            name: "Budi".into(),
            email: None,
            seconds: 600,
            days: vec![day("2026-10-05", vec![("Alpha", "x", 600)], vec![])],
        };
        ReportData {
            company_name: "Acme Corp".into(),
            start_date: "2026-10-05".into(),
            end_date: "2026-10-07".into(),
            generated_at: Utc
                .with_ymd_and_hms(2026, 10, 8, 1, 0, 0)
                .single()
                .unwrap_or_default(),
            generated_by: "Admin".into(),
            time_zone: Some("Asia/Jakarta".into()),
            employees: vec![a, b],
            sections: Sections::default(),
        }
    }

    #[test]
    fn renders_pdf_with_all_sections() {
        let pdf = generate(&sample());
        assert!(pdf.starts_with(b"%PDF-1.3") && pdf.ends_with(b"%%EOF\n"));
        let text = String::from_utf8_lossy(&pdf);
        assert!(text.contains("/Count "));
        assert!(text.contains("Work Hours & Activity Report 2026-10-05 - 2026-10-07"));
    }

    #[test]
    fn empty_report_still_renders() {
        let mut d = sample();
        d.employees.clear();
        assert!(generate(&d).starts_with(b"%PDF"));
        d.sections = Sections {
            summary: false,
            projects: false,
            employees: false,
        };
        assert!(generate(&d).starts_with(b"%PDF"));
    }

    #[test]
    fn many_days_paginate() {
        let mut d = sample();
        d.start_date = "2026-01-01".into();
        d.end_date = "2026-03-31".into();
        let proto = d.employees[0].days[0].clone();
        d.employees[0].days = (0..60)
            .map(|i| Day {
                date: format!("2026-02-{:02}", (i % 28) + 1),
                ..proto.clone()
            })
            .collect();
        let pdf = generate(&d);
        let text = String::from_utf8_lossy(&pdf);
        let pages: usize = text
            .split("/Count ")
            .nth(1)
            .and_then(|r| r.split(' ').next())
            .and_then(|n| n.parse().ok())
            .unwrap_or(0);
        assert!(pages > 3, "harus beberapa halaman, dapat {pages}");
    }

    #[test]
    fn stats_sorting() {
        let d = sample();
        let s = employee_stat(&d.employees[0]);
        assert_eq!(s.active_days, 2);
        assert_eq!(s.app_totals[0].name, "Chrome");
        assert_eq!(s.projects[0], ("Alpha".to_owned(), 7200));
        let refs: Vec<&Employee> = d.employees.iter().collect();
        let p = project_stats(&refs);
        assert_eq!(p[0].name, "Alpha");
        assert_eq!(p[0].members, vec!["Maria Wijaya", "Budi"]);
        assert_eq!(p[1].color, PROJECT_COLORS[1]);
    }
}
