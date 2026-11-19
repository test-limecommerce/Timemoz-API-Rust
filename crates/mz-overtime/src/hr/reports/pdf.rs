//! PDF "Laporan Lembur" (FX-6.3). Node: `src/overtime/overtime-report-pdf.generator.ts`.
//!
//! Halaman ringkasan (sampul, KPI, grafik, tabel per karyawan, tabel kelebihan mingguan)
//! lalu satu halaman rincian per karyawan yang punya baris. Fungsi murni atas data
//! `exportData()` — baris PDF = baris layar. Penulis PDF dipakai ulang dari
//! `mz_screenshots::reports::pdf`; tata letak tidak piksel-identik dengan pdfkit.

use std::collections::BTreeMap;

use mz_screenshots::reports::pdf::doc::{Align, Font, text_width};
use mz_screenshots::reports::pdf::format::theme::{
    AMBER, CYAN, DARK, FAINT, GREEN, M, MUTED, PRIMARY, PRIMARY_SOFT, SURFACE, SURFACE2, TEXT,
    VIOLET, WHITE,
};
use mz_screenshots::reports::pdf::format::{clean_text, enumerate_days};

use super::entity::{
    EmployeeRef, OvertimeReport, ReportDay, ReportEmployee, ReportMeta, WeeklyExcessReport,
    WeeklyExcessWeek,
};
use super::export::{day_type_label, status_label};
use crate::calendar::WEEKLY_LIMIT_SEC;
use crate::math::week_range_of;
use crate::zone::parse_date_key;
use mz_screenshots::reports::pdf::canvas::{
    self, CW, Canvas, CanvasOptions, ChartBucket, ChartOptions, ChartPart, Column, CoverOptions,
    FooterOptions, KpiCard, Locale, TextStyle,
};
use mz_screenshots::reports::pdf::format::ReportFormat;

/// `reportFormat('id')`.
const FMT: ReportFormat = ReportFormat(Locale::Id);

const RED: &str = "#dc2626";
const AMBER_SOFT: &str = "#fef3c7";
const AMBER_TEXT: &str = "#92400e";
const RED_SOFT: &str = "#fee2e2";
const RED_TEXT: &str = "#b91c1c";
/// Tanda baris belum final (pengganti `*` di PDF lama).
const NOT_FINAL_DOT: &str = "\u{2022}";
const MAX_NOTE_CHARS: usize = 500;
/// Tinggi plot grafik ringkasan (lebih pendek dari laporan tracking agar ringkasan muat 1 halaman).
const CHART_H: f64 = 84.0;
/// Tinggi baris rincian harian tanpa baris detail (tanggal dua baris).
const ROW_H: f64 = 27.0;
const FILTER_ALL: &str = "Semua";
/// Catatan hukum cadangan bila `legalNote` kosong (`LEGAL_NOTE`).
pub const LEGAL_NOTE: &str = "Batas lembur PP 35/2021: 4 jam/hari dan 18 jam/minggu di hari kerja; 12 jam (pola 5 hari) atau 11 jam (pola 6 hari) di hari libur. Sistem hanya memberi peringatan; kepatuhan menjadi tanggung jawab perusahaan.";

fn status_color(status: &str) -> &'static str {
    match status {
        "COMPLETED" => GREEN,
        "APPROVED" => PRIMARY,
        "PENDING_APPROVAL" | "PENDING_EMPLOYEE" => AMBER,
        "REJECTED" => RED,
        _ => FAINT,
    }
}

/// Data PDF: konteks cetak (`exportData().meta`) + kedua bagian laporan.
pub struct PdfInput<'a> {
    pub meta: &'a ReportMeta,
    pub daily: &'a OvertimeReport,
    pub weekly: &'a WeeklyExcessReport,
}

struct Chip {
    text: String,
    bg: &'static str,
    fg: &'static str,
}

struct DayRowLayout<'d> {
    day: &'d ReportDay,
    note: Option<String>,
    note_h: f64,
    chip_lines: Vec<Vec<Chip>>,
    h: f64,
}

#[allow(clippy::cast_precision_loss)]
fn minutes_to_sec(minutes: Option<i64>) -> f64 {
    minutes.unwrap_or(0) as f64 * 60.0
}

#[allow(clippy::cast_precision_loss)]
fn sec(v: i64) -> f64 {
    v as f64
}

fn clean(value: &str) -> String {
    clean_text(Some(value))
}

struct Generator<'a> {
    c: Canvas,
    input: &'a PdfInput<'a>,
    period_days: Vec<String>,
}

/// Membuat PDF laporan lembur.
pub fn generate(input: &PdfInput<'_>) -> Vec<u8> {
    let from = &input.daily.period.from;
    let to = &input.daily.period.to;
    let legal_note = if input.daily.legal_note.is_empty() {
        LEGAL_NOTE
    } else {
        &input.daily.legal_note
    };
    // Footer tiga baris; catatan hukum bisa dibungkus lebih dari satu baris.
    let legal_h = canvas::height_of(&clean(legal_note), CW, Font::Regular, 7.0, 0.0);
    let reserve = 74.0_f64.max((58.0 + legal_h).ceil());
    let canvas = Canvas::with_options(
        &format!("Laporan Lembur {from} - {to}"),
        &clean(&input.meta.company_name),
        "Timemoz",
        CanvasOptions {
            locale: Locale::Id,
            bottom_reserve: reserve,
        },
    );
    let mut g = Generator {
        c: canvas,
        input,
        period_days: enumerate_days(from, to),
    };
    g.render();
    g.c.draw_footers(&FooterOptions {
        company_name: &input.meta.company_name,
        company_fallback: Some("Perusahaan"),
        report_title: "Laporan Lembur",
        confidential_label: Some("Rahasia"),
        generated_by_label: Some("Dibuat oleh Timemoz \u{b7} www.timemoz.com"),
        page_label: Some(|i, n| format!("Halaman {i} dari {n}")),
        extra_line: Some(legal_note),
    });
    g.c.doc.finish()
}

impl Generator<'_> {
    fn render(&mut self) {
        let employees: Vec<&ReportEmployee> = self
            .input
            .daily
            .employees
            .iter()
            .filter(|e| !e.days.is_empty())
            .collect();
        let row_count: usize = employees.iter().map(|e| e.days.len()).sum();
        let has_weekly = self
            .input
            .weekly
            .months
            .iter()
            .any(|m| !m.employees.is_empty());

        self.draw_cover(employees.len(), row_count);

        if employees.is_empty() && !has_weekly {
            self.c.empty_state(
                "Tidak ada lembur pada periode ini",
                "Coba ubah periode atau filter karyawan/status.",
            );
            return;
        }

        let not_final = employees.iter().any(|e| e.days.iter().any(|d| !d.is_final))
            || self.input.weekly.months.iter().any(|m| {
                m.employees
                    .iter()
                    .any(|e| e.weeks.iter().any(|w| w.not_final))
            });
        if not_final {
            self.draw_not_final_box();
        }

        if employees.is_empty() {
            self.c.empty_state(
                "Tidak ada lembur pada periode ini",
                "Coba ubah periode atau filter karyawan/status.",
            );
        } else {
            self.draw_summary_kpis(&employees);
            self.draw_chart(&employees);
            self.draw_employee_summary(&employees);
        }
        self.draw_weekly_excess();

        for e in &employees {
            self.draw_employee_page(e);
        }
    }

    // ---- Halaman ringkasan ----

    fn draw_cover(&mut self, employee_count: usize, row_count: usize) {
        let m = self.input.meta;
        let from = &self.input.daily.period.from;
        let to = &self.input.daily.period.to;
        let generated_by = {
            let c = clean(&m.generated_by);
            if c.is_empty() { "-".to_owned() } else { c }
        };
        self.c.cover_header(&CoverOptions {
            title: "LAPORAN LEMBUR",
            company_name: &m.company_name,
            company_fallback: Some("Perusahaan"),
            period_label: Some("PERIODE LAPORAN"),
            period_from: &FMT.f_date(from),
            period_to: &FMT.f_date(to),
            period_hint: &format!("{} hari", self.period_days.len()),
            meta: &[
                (
                    "Dibuat pada",
                    FMT.f_generated_at(m.generated_at, Some(&m.time_zone)),
                ),
                ("Dibuat oleh", generated_by),
                (
                    "Cakupan",
                    format!("{employee_count} karyawan \u{b7} {row_count} pengajuan"),
                ),
            ],
        });

        if let Some(filter) = self.filter_line() {
            let shown = canvas::fit(&filter, CW, Font::Regular, 8.0);
            let y = self.c.y - 8.0;
            self.c.txt(&shown, M, y, TextStyle::new(8.0, MUTED));
            self.c.y += 10.0;
        }
    }

    /// "Filter: ..." bila ada filter aktif; `None` bila tidak ada.
    fn filter_line(&self) -> Option<String> {
        let f = &self.input.meta.filters;
        let active = !f.status.is_empty()
            || f.day_type.is_some()
            || f.exceeds_limit.is_some()
            || f.employee_count > 0;
        if !active {
            return None;
        }
        let status = if f.status.is_empty() {
            FILTER_ALL.to_owned()
        } else {
            f.status
                .iter()
                .map(|s| status_label(s))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let day_type = f
            .day_type
            .as_deref()
            .map_or(FILTER_ALL, day_type_label)
            .to_owned();
        let exceeds = match f.exceeds_limit {
            None => FILTER_ALL,
            Some(true) => "Ya",
            Some(false) => "Tidak",
        };
        let people = if f.employee_count > 0 {
            format!("{} dipilih", f.employee_count)
        } else {
            FILTER_ALL.to_owned()
        };
        Some(format!(
            "Filter: Status {status}  \u{b7}  Jenis hari {day_type}  \u{b7}  Melebihi batas {exceeds}  \u{b7}  Karyawan {people}"
        ))
    }

    fn draw_not_final_box(&mut self) {
        let h = 32.0;
        self.c.ensure(h + 10.0);
        let y = self.c.y;
        self.c.doc.rrect(M, y, CW, h, 6.0, AMBER_SOFT);
        self.c.doc.rect(M, y + 7.0, 3.0, h - 14.0, AMBER);
        self.c.txt(
            "Sebagian angka belum final (masih dalam masa pengajuan mundur).",
            M + 14.0,
            y + 6.5,
            TextStyle::new(8.5, AMBER_TEXT).bold(),
        );
        self.c.txt(
            &format!("Baris bertanda {NOT_FINAL_DOT} masih bisa berubah."),
            M + 14.0,
            y + 18.5,
            TextStyle::new(8.0, AMBER_TEXT),
        );
        self.c.y += h + 12.0;
    }

    fn draw_summary_kpis(&mut self, employees: &[&ReportEmployee]) {
        let days: Vec<&ReportDay> = employees.iter().flat_map(|e| e.days.iter()).collect();
        let total_sec: i64 = employees.iter().map(|e| e.total_overtime_seconds).sum();
        let completed = days.iter().filter(|d| d.status == "COMPLETED").count();
        let with_overtime = employees
            .iter()
            .filter(|e| e.total_overtime_seconds > 0)
            .count();
        let exceeded = days.iter().filter(|d| d.exceeds_limit).count();
        let excess_minutes: i64 = self
            .input
            .weekly
            .months
            .iter()
            .flat_map(|m| m.employees.iter())
            .map(|e| e.total_excess_minutes)
            .sum();

        self.c.section_title("Ringkasan", None);
        self.c.kpi_cards(&[
            KpiCard {
                label: "Total lembur terhitung".into(),
                value: FMT.f_dur(sec(total_sec)),
                hint: Some(format!("{completed} pengajuan selesai")),
                color: PRIMARY,
            },
            KpiCard {
                label: "Karyawan lembur".into(),
                value: with_overtime.to_string(),
                hint: Some(format!("dari {} karyawan", employees.len())),
                color: CYAN,
            },
            KpiCard {
                label: "Melebihi batas".into(),
                value: exceeded.to_string(),
                hint: Some("pengajuan".into()),
                color: if exceeded > 0 { RED } else { AMBER },
            },
            KpiCard {
                // "Kelebihan tanpa pengajuan" terpotong di lebar kartu; artinya dipecah ke hint.
                label: "Kelebihan jam kerja".into(),
                value: FMT.f_dur(minutes_to_sec(Some(excess_minutes))),
                hint: Some("tanpa pengajuan lembur".into()),
                color: VIOLET,
            },
        ]);
    }

    /// Lembur terhitung (COMPLETED) per tanggal; hari libur bertumpuk ungu.
    fn draw_chart(&mut self, employees: &[&ReportEmployee]) {
        let mut work: BTreeMap<&str, f64> = BTreeMap::new();
        let mut holiday: BTreeMap<&str, f64> = BTreeMap::new();
        let mut total = 0.0;
        for e in employees {
            for d in &e.days {
                let minutes = d.overtime_minutes.unwrap_or(0);
                if d.status != "COMPLETED" || minutes == 0 {
                    continue;
                }
                let target = if d.day_type.as_deref() == Some("HOLIDAY") {
                    &mut holiday
                } else {
                    &mut work
                };
                let s = minutes_to_sec(Some(minutes));
                *target.entry(d.date.as_str()).or_insert(0.0) += s;
                total += s;
            }
        }
        if total <= 0.0 {
            return;
        }

        let (a, weekly) =
            FMT.chart_buckets(&self.period_days, |d| work.get(d).copied().unwrap_or(0.0));
        let (b, _) = FMT.chart_buckets(&self.period_days, |d| {
            holiday.get(d).copied().unwrap_or(0.0)
        });
        let buckets: Vec<ChartBucket> = a
            .into_iter()
            .enumerate()
            .map(|(i, bucket)| {
                let holiday_sec = b.get(i).map_or(0.0, |x| x.seconds);
                ChartBucket {
                    label: bucket.label,
                    seconds: bucket.seconds + holiday_sec,
                    parts: Some(vec![
                        ChartPart {
                            seconds: bucket.seconds,
                            color: PRIMARY,
                        },
                        ChartPart {
                            seconds: holiday_sec,
                            color: VIOLET,
                        },
                    ]),
                }
            })
            .collect();

        if weekly {
            self.c.section_title(
                "Lembur Terhitung per Minggu",
                Some("Lembur pengajuan selesai semua karyawan per minggu (mulai Senin)."),
            );
        } else {
            self.c.section_title(
                "Lembur Terhitung per Hari",
                Some("Lembur pengajuan selesai semua karyawan per hari."),
            );
        }
        self.c.ensure(CHART_H + 30.0);
        self.draw_legend();
        self.c.bar_chart(
            &buckets,
            ChartOptions {
                axis_unit: "j",
                height: CHART_H,
                ..ChartOptions::default()
            },
        );
    }

    fn draw_legend(&mut self) {
        let y = self.c.y - 8.0;
        let mut x = M + CW;
        for (label, color) in [("Hari libur", VIOLET), ("Hari kerja", PRIMARY)] {
            x -= text_width(label, Font::Regular, 7.5);
            self.c.txt(label, x, y, TextStyle::new(7.5, MUTED));
            x -= 10.0;
            self.c.doc.rrect(x, y + 0.5, 7.0, 7.0, 1.5, color);
            x -= 14.0;
        }
    }

    fn draw_employee_summary(&mut self, employees: &[&ReportEmployee]) {
        self.c.section_title(
            "Ringkasan per Karyawan",
            Some("Diajukan = disetujui/selesai; tercatat & lembur hanya dari pengajuan selesai."),
        );
        let cols = [
            Column::new("Karyawan", 167.0, Align::Left),
            Column::new("Pengajuan", 62.0, Align::Center),
            Column::new("Diajukan", 66.0, Align::Right),
            Column::new("Tercatat", 66.0, Align::Right),
            Column::new("Lembur", 66.0, Align::Right),
            Column::new(
                "Melebihi batas",
                CW - 167.0 - 62.0 - 66.0 * 3.0,
                Align::Center,
            ),
        ];
        let rh = 30.0;
        self.c.ensure(22.0 + rh);
        self.c.table_header(&cols);

        for (i, e) in employees.iter().enumerate() {
            if self.c.ensure(rh) {
                self.c.table_header(&cols);
            }
            let y = self.c.y;
            if i % 2 == 1 {
                self.c.doc.rect(M, y, CW, rh, SURFACE);
            }
            let decided: Vec<&ReportDay> = e
                .days
                .iter()
                .filter(|d| d.status == "APPROVED" || d.status == "COMPLETED")
                .collect();
            let completed: Vec<&ReportDay> =
                e.days.iter().filter(|d| d.status == "COMPLETED").collect();
            let requested: i64 = decided.iter().map(|d| d.approved_minutes).sum();
            let worked: i64 = completed
                .iter()
                .map(|d| d.worked_minutes.unwrap_or(0))
                .sum();
            let exceeded = e.days.iter().filter(|d| d.exceeds_limit).count();

            let first_w = cols.first().map_or(167.0, |c| c.w);
            self.draw_person(M, &e.employee, e.not_final, first_w, y, rh);
            let mut x = M + first_w;
            let mut cell = |g: &mut Self, value: &str, col: &Column, bold: bool, color: &str| {
                let w = if col.align == Align::Center {
                    col.w
                } else {
                    col.w - 8.0
                };
                let mut st = TextStyle::new(8.5, color).width(w, col.align);
                if bold {
                    st = st.bold();
                }
                g.c.txt(value, x, y + 12.0, st);
                x += col.w;
            };
            let c = |i: usize| cols.get(i);
            if let (Some(c1), Some(c2), Some(c3), Some(c4), Some(c5)) =
                (c(1), c(2), c(3), c(4), c(5))
            {
                cell(self, &e.days.len().to_string(), c1, false, TEXT);
                let req = if decided.is_empty() {
                    "-".to_owned()
                } else {
                    FMT.f_dur(minutes_to_sec(Some(requested)))
                };
                cell(self, &req, c2, false, TEXT);
                let wk = if completed.is_empty() {
                    "-".to_owned()
                } else {
                    FMT.f_dur(minutes_to_sec(Some(worked)))
                };
                cell(self, &wk, c3, false, TEXT);
                cell(
                    self,
                    &FMT.f_dur(sec(e.total_overtime_seconds)),
                    c4,
                    true,
                    DARK,
                );
                cell(
                    self,
                    &exceeded.to_string(),
                    c5,
                    exceeded > 0,
                    if exceeded > 0 { RED } else { TEXT },
                );
            }
            self.c.y += rh;
        }
        self.c.hr_default();
        self.c.y += 14.0;
    }

    /// Avatar + nama (+ tanda belum final) + email di sel mulai `x0`, baris setinggi `rh`.
    fn draw_person(
        &mut self,
        x0: f64,
        person: &EmployeeRef,
        not_final: bool,
        w: f64,
        y: f64,
        rh: f64,
    ) {
        self.c.avatar(
            &person.name,
            x0 + 20.0,
            y + rh / 2.0,
            10.0,
            PRIMARY_SOFT,
            PRIMARY,
            7.5,
        );
        let name_w = w - 42.0 - if not_final { 10.0 } else { 0.0 };
        let cleaned = clean(&person.name);
        let shown = canvas::fit(
            if cleaned.is_empty() { "-" } else { &cleaned },
            name_w,
            Font::Bold,
            8.5,
        );
        self.c
            .txt(&shown, x0 + 36.0, y + 7.0, TextStyle::new(8.5, DARK).bold());
        if not_final {
            let nx = x0 + 36.0 + text_width(&shown, Font::Bold, 8.5) + 3.0;
            self.c.txt(
                NOT_FINAL_DOT,
                nx,
                y + 7.0,
                TextStyle::new(8.5, AMBER).bold(),
            );
        }
        let email = clean(&person.email);
        let email = canvas::fit(
            if email.is_empty() { "-" } else { &email },
            w - 42.0,
            Font::Regular,
            7.5,
        );
        self.c
            .txt(&email, x0 + 36.0, y + 18.0, TextStyle::new(7.5, MUTED));
    }

    /// Label minggu: `12\u{2013}18 Okt`, atau `28 Sep\u{2013}04 Okt` bila melewati bulan.
    fn week_label(start: &str, end: &str) -> String {
        if start.get(..7) == end.get(..7) {
            return format!(
                "{}\u{2013}{}",
                start.get(8..10).unwrap_or(""),
                FMT.f_day_short(end)
            );
        }
        format!("{}\u{2013}{}", FMT.f_day_short(start), FMT.f_day_short(end))
    }

    /// `2026-10` -> `Okt 2026`.
    fn month_label(month: &str) -> String {
        FMT.f_date(&format!("{month}-01")).chars().skip(3).collect()
    }

    fn draw_weekly_excess(&mut self) {
        let from = self.input.daily.period.from.clone();
        let to = self.input.daily.period.to.clone();
        self.c.section_title(
            "Melebihi Jam Kerja Mingguan (tanpa pengajuan)",
            Some("Minggu lintas bulan dihitung di bulan minggu itu berakhir. Hanya filter karyawan yang berlaku."),
        );
        let rows: Vec<(&str, &EmployeeRef, &WeeklyExcessWeek)> = self
            .input
            .weekly
            .months
            .iter()
            .flat_map(|m| {
                m.employees.iter().flat_map(move |e| {
                    e.weeks
                        .iter()
                        .map(move |w| (m.month.as_str(), &e.employee, w))
                })
            })
            .collect();
        if rows.is_empty() {
            self.c.ensure(56.0);
            let y = self.c.y;
            self.c.doc.rrect(M, y, CW, 40.0, 6.0, SURFACE);
            self.c.txt(
                "Tidak ada karyawan yang melebihi jam kerja mingguan pada periode ini.",
                M,
                y + 15.0,
                TextStyle::new(8.5, MUTED).width(CW, Align::Center),
            );
            self.c.y += 54.0;
            return;
        }

        let with_month = from.get(..7) != to.get(..7);
        let month_w = if with_month { 58.0 } else { 0.0 };
        let name_w = if with_month { 131.0 } else { 159.0 };
        let mut cols: Vec<Column> = Vec::new();
        if with_month {
            cols.push(Column::new("Bulan", month_w, Align::Left));
        }
        cols.push(Column::new("Karyawan", name_w, Align::Left));
        cols.push(Column::new("Minggu", 74.0, Align::Left));
        cols.push(Column::new("Tercatat", 56.0, Align::Right));
        cols.push(Column::new("Normal", 56.0, Align::Right));
        cols.push(Column::new("Lembur resmi", 72.0, Align::Right));
        let last_w = CW - month_w - name_w - 74.0 - 56.0 - 56.0 - 72.0;
        cols.push(Column::new("Kelebihan", last_w, Align::Right));
        let rh = 30.0;
        self.c.ensure(22.0 + rh);
        self.c.table_header(&cols);
        for (i, (month, employee, week)) in rows.iter().enumerate() {
            if self.c.ensure(rh) {
                self.c.table_header(&cols);
            }
            let y = self.c.y;
            if i % 2 == 1 {
                self.c.doc.rect(M, y, CW, rh, SURFACE);
            }
            let mut x = M;
            if with_month {
                self.c.txt(
                    &Self::month_label(month),
                    x + 6.0,
                    y + 12.0,
                    TextStyle::new(8.0, TEXT),
                );
                x += month_w;
            }
            self.draw_person(x, employee, week.not_final, name_w, y, rh);
            x += name_w;
            self.c.txt(
                &Self::week_label(&week.week_start, &week.week_end),
                x + 6.0,
                y + 12.0,
                TextStyle::new(8.0, TEXT),
            );
            x += 74.0;
            let cells = [
                (week.worked_minutes, 56.0, false),
                (week.normal_minutes, 56.0, false),
                (week.overtime_minutes, 72.0, false),
                (week.excess_minutes, last_w, true),
            ];
            for (minutes, w, bold) in cells {
                let mut st = TextStyle::new(8.5, if bold { DARK } else { TEXT })
                    .width(w - 8.0, Align::Right);
                if bold {
                    st = st.bold();
                }
                self.c
                    .txt(&FMT.f_dur(minutes_to_sec(Some(minutes))), x, y + 12.0, st);
                x += w;
            }
            self.c.y += rh;
        }
        self.c.hr_default();
        self.c.y += 14.0;
    }

    // ---- Halaman per karyawan ----

    fn draw_employee_page(&mut self, e: &ReportEmployee) {
        let from = &self.input.daily.period.from;
        let to = &self.input.daily.period.to;
        self.c.new_page();
        self.c.person_header(
            &e.employee.name,
            Some(&e.employee.email),
            "Rincian lembur",
            &format!("{} - {}", FMT.f_date(from), FMT.f_date(to)),
        );

        let completed = e.days.iter().filter(|d| d.status == "COMPLETED").count();
        let exceeded = e.days.iter().filter(|d| d.exceeds_limit).count();
        #[allow(clippy::cast_precision_loss)]
        let avg = if completed > 0 {
            FMT.f_dur(sec(e.total_overtime_seconds) / completed as f64)
        } else {
            "-".to_owned()
        };
        self.c.kpi_cards(&[
            KpiCard {
                label: "Lembur terhitung".into(),
                value: FMT.f_dur(sec(e.total_overtime_seconds)),
                hint: Some("Hanya pengajuan selesai".into()),
                color: PRIMARY,
            },
            KpiCard {
                label: "Pengajuan selesai".into(),
                value: format!("{completed} dari {}", e.days.len()),
                hint: Some("pengajuan".into()),
                color: CYAN,
            },
            KpiCard {
                label: "Rata-rata per selesai".into(),
                value: avg,
                hint: Some("per pengajuan selesai".into()),
                color: VIOLET,
            },
            KpiCard {
                label: "Melebihi batas".into(),
                value: exceeded.to_string(),
                hint: Some("pengajuan".into()),
                color: if exceeded > 0 { RED } else { AMBER },
            },
        ]);

        self.draw_weekly_quota(e);
        self.draw_daily_rows(e);
        self.c.total_bar(
            "TOTAL LEMBUR PERIODE",
            &FMT.f_dur(sec(e.total_overtime_seconds)),
        );
    }

    /// Minggu kerja company untuk satu tanggal.
    fn week_of(&self, date: &str) -> (String, String) {
        let range = parse_date_key(date)
            .and_then(|d| week_range_of(d, self.input.meta.workweek_start_day).ok());
        match range {
            Some((s, e)) => (
                s.format("%Y-%m-%d").to_string(),
                e.format("%Y-%m-%d").to_string(),
            ),
            None => (date.to_owned(), date.to_owned()),
        }
    }

    /// Jumlah lembur selesai hari kerja per minggu kerja company vs batas 18 jam.
    fn draw_weekly_quota(&mut self, e: &ReportEmployee) {
        let mut weeks: BTreeMap<String, (String, f64)> = BTreeMap::new();
        for d in &e.days {
            if d.day_type.as_deref() != Some("WORKDAY") {
                continue;
            }
            let (start, end) = self.week_of(&d.date);
            let acc = weeks.entry(start).or_insert((end, 0.0));
            if d.status == "COMPLETED" {
                acc.1 += minutes_to_sec(d.overtime_minutes);
            }
        }
        if weeks.is_empty() {
            return;
        }

        let limit = sec(WEEKLY_LIMIT_SEC);
        let limit_h = WEEKLY_LIMIT_SEC / 3600;
        self.c.section_title(
            &format!("Kuota Mingguan (hari kerja, batas {limit_h} jam)"),
            None,
        );
        let rh = 18.0;
        let label_w = 86.0;
        let value_w = 110.0;
        for (start, (end, total)) in &weeks {
            self.c.ensure(rh);
            let y = self.c.y;
            let ratio = total / limit;
            let color = if ratio > 1.0 { RED } else { PRIMARY };
            self.cell(
                &Self::week_label(start, end),
                M,
                y + 2.0,
                Font::Bold,
                8.0,
                DARK,
                None,
            );
            self.c.bar(
                M + label_w,
                y + 3.0,
                CW - label_w - value_w,
                8.0,
                ratio,
                color,
            );
            self.cell(
                &format!("{} / {limit_h}j", FMT.f_dur(*total)),
                0.0,
                y + 2.0,
                Font::Bold,
                8.0,
                if ratio > 1.0 { RED } else { DARK },
                Some(M + CW),
            );
            self.c.y += rh;
        }
        self.c.y += 12.0;
    }

    fn daily_cols(&self) -> Vec<Column> {
        let rest = CW - 84.0 - 120.0 - 56.0 * 3.0;
        vec![
            Column::new("Tanggal", 84.0, Align::Left),
            Column::new("Sumber", 120.0, Align::Left),
            Column::new("Diajukan", 56.0, Align::Right),
            Column::new("Tercatat", 56.0, Align::Right),
            Column::new("Lembur", 56.0, Align::Right),
            Column::new("Status", rest, Align::Left),
        ]
    }

    fn chips_of(d: &ReportDay) -> Vec<Chip> {
        let mut chips = Vec::new();
        let red = |text: &str| Chip {
            text: text.to_owned(),
            bg: RED_SOFT,
            fg: RED_TEXT,
        };
        if d.exceeds_daily_limit {
            let within_actual = d.status == "COMPLETED"
                && matches!(
                    (d.overtime_minutes, d.daily_limit_minutes),
                    (Some(o), Some(l)) if o <= l
                );
            chips.push(red(if within_actual {
                "Disetujui di atas batas (aktual dalam batas)"
            } else {
                "Disetujui di atas batas"
            }));
        }
        if d.exceeds_weekly_limit || (d.exceeds_limit && !d.exceeds_daily_limit) {
            chips.push(red("Melebihi 18 jam/minggu"));
        }
        if d.status == "COMPLETED" && d.overtime_minutes.unwrap_or(0) == 0 {
            chips.push(Chip {
                text: if d.worked_minutes.unwrap_or(0) != 0 {
                    "Belum melebihi jam normal".to_owned()
                } else {
                    "Tidak ada jam tercatat".to_owned()
                },
                bg: SURFACE2,
                fg: MUTED,
            });
        }
        if !d.is_final {
            chips.push(Chip {
                text: "Belum final".to_owned(),
                bg: AMBER_SOFT,
                fg: AMBER_TEXT,
            });
        }
        chips
    }

    fn chip_width(chip: &Chip) -> f64 {
        text_width(&chip.text, Font::Bold, 6.8) + 12.0
    }

    /// Teks satu baris; `right` = tepi kanan untuk rata kanan.
    #[allow(clippy::too_many_arguments)]
    fn cell(
        &mut self,
        value: &str,
        x: f64,
        y: f64,
        font: Font,
        size: f64,
        color: &str,
        right: Option<f64>,
    ) {
        let left = right.map_or(x, |r| r - text_width(value, font, size));
        let st = TextStyle {
            font,
            ..TextStyle::new(size, color)
        };
        self.c.txt(value, left, y, st);
    }

    /// Tinggi baris dihitung sekali per baris.
    fn layout_row<'d>(d: &'d ReportDay, detail_w: f64) -> DayRowLayout<'d> {
        let raw = clean(d.note.as_deref().unwrap_or(""));
        let note = if raw.is_empty() {
            None
        } else if raw.chars().count() > MAX_NOTE_CHARS {
            let cut: String = raw.chars().take(MAX_NOTE_CHARS).collect();
            Some(format!("Alasan: {}...", cut.trim_end()))
        } else {
            Some(format!("Alasan: {raw}"))
        };
        let mut note_h = 0.0;
        if let Some(n) = &note {
            note_h = if text_width(n, Font::Regular, 7.5) <= detail_w {
                9.0
            } else {
                canvas::height_of(n, detail_w, Font::Regular, 7.5, 0.0)
            };
        }
        let mut chip_lines: Vec<Vec<Chip>> = Vec::new();
        let mut line_w = 0.0;
        for chip in Self::chips_of(d) {
            let w = Self::chip_width(&chip) + 5.0;
            match chip_lines.last_mut() {
                Some(last) if line_w + w <= detail_w => {
                    last.push(chip);
                    line_w += w;
                }
                _ => {
                    chip_lines.push(vec![chip]);
                    line_w = w;
                }
            }
        }
        #[allow(clippy::cast_precision_loss)]
        let extra_h =
            if note.is_some() { note_h + 3.0 } else { 0.0 } + chip_lines.len() as f64 * 14.0;
        let h = if extra_h > 0.0 {
            ROW_H.max(18.0 + extra_h + 4.0)
        } else {
            ROW_H
        };
        DayRowLayout {
            day: d,
            note,
            note_h,
            chip_lines,
            h,
        }
    }

    fn draw_pill(&mut self, label: &str, color: &str, x: f64, y: f64, max_w: f64) {
        let shown = canvas::fit(label, max_w - 14.0, Font::Bold, 7.0);
        let w = max_w.min(text_width(&shown, Font::Bold, 7.0) + 14.0);
        self.c.doc.rrect(x, y, w, 14.0, 7.0, color);
        self.cell(&shown, x + 7.0, y + 4.0, Font::Bold, 7.0, WHITE, None);
    }

    fn draw_daily_rows(&mut self, e: &ReportEmployee) {
        let cols = self.daily_cols();
        let (c0, c1, c2, c3, c4, c5) = (
            cols.first().map_or(0.0, |c| c.w),
            cols.get(1).map_or(0.0, |c| c.w),
            cols.get(2).map_or(0.0, |c| c.w),
            cols.get(3).map_or(0.0, |c| c.w),
            cols.get(4).map_or(0.0, |c| c.w),
            cols.get(5).map_or(0.0, |c| c.w),
        );
        let detail_x = M + c0 + 8.0;
        let detail_w = CW - c0 - 16.0;
        let rows: Vec<DayRowLayout<'_>> = e
            .days
            .iter()
            .map(|d| Self::layout_row(d, detail_w))
            .collect();

        self.c.section_title(
            "Rincian Harian",
            Some("Satu baris per pengajuan; alasan tolak/batal dan penanda ada di bawahnya."),
        );
        self.c.ensure(22.0 + rows.first().map_or(ROW_H, |r| r.h));
        self.c.table_header(&cols);

        let x_source = M + c0;
        let x_requested = x_source + c1;
        let x_worked = x_requested + c2;
        let x_overtime = x_worked + c3;
        let x_status = x_overtime + c4;
        for (i, r) in rows.iter().enumerate() {
            if self.c.y + r.h > self.c.bottom() {
                self.c.new_page();
                self.c.table_header(&cols);
            }
            let d = r.day;
            let y = self.c.y;
            if i % 2 == 1 {
                self.c.doc.rect(M, y, CW, r.h, SURFACE);
            }

            // Tanggal + jenis hari.
            let day_label = FMT.f_day(&d.date);
            self.cell(&day_label, M + 8.0, y + 6.5, Font::Bold, 8.0, DARK, None);
            if !d.is_final {
                let nx = M + 8.0 + text_width(&day_label, Font::Bold, 8.0) + 3.0;
                self.cell(NOT_FINAL_DOT, nx, y + 7.0, Font::Bold, 8.0, AMBER, None);
            }
            match d.day_type.as_deref() {
                Some(t @ ("HOLIDAY" | "WORKDAY")) => {
                    let color = if t == "HOLIDAY" { VIOLET } else { PRIMARY };
                    self.c.doc.rect(M + 8.5, y + 18.6, 4.4, 4.4, color);
                    self.cell(
                        day_type_label(t),
                        M + 16.0,
                        y + 17.0,
                        Font::Regular,
                        7.5,
                        MUTED,
                        None,
                    );
                }
                _ => self.cell("-", M + 8.0, y + 17.0, Font::Regular, 7.5, FAINT, None),
            }

            // Sumber.
            let source = if d.kind == "ORDER" {
                let by = clean(&d.requested_by_name);
                format!("Perintah dari {}", if by.is_empty() { "-" } else { &by })
            } else {
                "Pengajuan sendiri".to_owned()
            };
            let source = canvas::fit(&source, c1 - 12.0, Font::Regular, 8.0);
            self.cell(
                &source,
                x_source + 8.0,
                y + 7.0,
                Font::Regular,
                8.0,
                TEXT,
                None,
            );

            // Durasi, rata kanan.
            for (minutes, right, bold) in [
                (Some(d.approved_minutes), x_worked, false),
                (d.worked_minutes, x_overtime, false),
                (d.overtime_minutes, x_status, true),
            ] {
                let (value, color) = match minutes {
                    None => ("-".to_owned(), FAINT),
                    Some(m) => (
                        FMT.f_dur(minutes_to_sec(Some(m))),
                        if bold { DARK } else { TEXT },
                    ),
                };
                let font = if bold { Font::Bold } else { Font::Regular };
                self.cell(&value, 0.0, y + 7.0, font, 8.0, color, Some(right - 8.0));
            }

            // Status pill.
            self.draw_pill(
                status_label(&d.status),
                status_color(&d.status),
                x_status + 8.0,
                y + 4.5,
                c5 - 16.0,
            );

            // Baris detail: alasan lalu chip.
            let mut dy = y + 18.0;
            if let Some(note) = &r.note {
                if r.note_h <= 9.0 {
                    self.cell(note, detail_x, dy, Font::Regular, 7.5, TEXT, None);
                } else {
                    self.c.txt(
                        note,
                        detail_x,
                        dy,
                        TextStyle::new(7.5, TEXT).width(detail_w, Align::Left),
                    );
                }
                dy += r.note_h + 3.0;
            }
            for line in &r.chip_lines {
                let mut cx = detail_x;
                for chip in line {
                    let w = Self::chip_width(chip).min(detail_w);
                    self.c.doc.rrect(cx, dy, w, 11.0, 5.5, chip.bg);
                    let shown = canvas::fit(&chip.text, w - 12.0, Font::Bold, 6.8);
                    self.cell(&shown, cx + 6.0, dy + 2.4, Font::Bold, 6.8, chip.fg, None);
                    cx += w + 5.0;
                }
                dy += 14.0;
            }

            self.c.y += r.h;
        }
        self.c.hr(0.6, M, CW);
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use std::io::Read;

    use mz_screenshots::reports::pdf::doc::PAGE_H;

    use super::super::entity::{
        EmployeeRef, ReportEmployee, WeeklyExcessEmployee, WeeklyExcessMonth, WeeklyExcessWeek,
    };
    use super::super::fixtures::{day, employee, meta, weekly_of};
    use super::*;

    fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
        hay.get(from..)?
            .windows(needle.len())
            .position(|w| w == needle)
            .map(|p| p + from)
    }

    /// Isi stream per halaman (inflate) sebagai teks latin-1.
    fn contents(pdf: &[u8]) -> Vec<String> {
        let mut out = Vec::new();
        let mut at = 0;
        while let Some(p) = find(pdf, b"/Filter /FlateDecode >>\nstream\n", at) {
            let start = p + b"/Filter /FlateDecode >>\nstream\n".len();
            let end = find(pdf, b"\nendstream", start).unwrap();
            let mut z = flate2::read::ZlibDecoder::new(&pdf[start..end]);
            let mut raw = Vec::new();
            z.read_to_end(&mut raw).unwrap();
            out.push(raw.iter().map(|b| *b as char).collect());
            at = end;
        }
        out
    }

    fn unescape_pdf(s: &str) -> String {
        let chars: Vec<char> = s.chars().collect();
        let mut out = String::new();
        let mut i = 0;
        while i < chars.len() {
            if chars[i] == '\\' && i + 1 < chars.len() {
                let n = chars[i + 1];
                if n.is_ascii_digit() {
                    let oct: String = chars[i + 1..(i + 4).min(chars.len())].iter().collect();
                    let b = u8::from_str_radix(&oct, 8).unwrap();
                    out.push(match b {
                        0x85 => '\u{2026}',
                        0x95 => '\u{2022}',
                        0x96 => '\u{2013}',
                        0x97 => '\u{2014}',
                        other => other as char,
                    });
                    i += 4;
                } else {
                    out.push(n);
                    i += 2;
                }
            } else {
                out.push(chars[i]);
                i += 1;
            }
        }
        out
    }

    /// Teks per halaman: satu baris per operator `Tj`.
    fn page_texts(pdf: &[u8]) -> Vec<String> {
        contents(pdf)
            .iter()
            .map(|c| {
                let mut out = String::new();
                for line in c.lines() {
                    if let Some(inner) = line
                        .strip_suffix(") Tj")
                        .and_then(|body| body.strip_prefix('('))
                    {
                        out.push_str(&unescape_pdf(inner));
                        out.push('\n');
                    }
                }
                out
            })
            .collect()
    }

    /// Baseline (dari bawah halaman) setiap operator teks per halaman.
    fn text_ys(pdf: &[u8]) -> Vec<Vec<f64>> {
        contents(pdf)
            .iter()
            .map(|c| {
                c.lines()
                    .filter(|l| l.ends_with(" Td"))
                    .filter_map(|l| l.split(' ').nth(1)?.parse::<f64>().ok())
                    .collect()
            })
            .collect()
    }

    fn page_count(pdf: &[u8]) -> usize {
        String::from_utf8_lossy(pdf)
            .matches("/Type /Page /Parent")
            .count()
    }

    fn squash(s: &str) -> String {
        s.split_whitespace().collect()
    }

    fn make(
        input_meta: &ReportMeta,
        employees: Vec<ReportEmployee>,
        months: Vec<WeeklyExcessMonth>,
        period: (&str, &str),
    ) -> Vec<u8> {
        let daily = super::super::fixtures::daily_of(employees, period.0, period.1);
        let weekly = weekly_of(months);
        generate(&PdfInput {
            meta: input_meta,
            daily: &daily,
            weekly: &weekly,
        })
    }

    fn pdf_meta() -> ReportMeta {
        ReportMeta {
            company_name: "PT QA Lembur".into(),
            generated_by: "Owner QA".into(),
            generated_at: chrono::TimeZone::with_ymd_and_hms(&chrono::Utc, 2026, 10, 26, 5, 24, 0)
                .single()
                .unwrap(),
            ..meta()
        }
    }

    fn pday(over: impl FnOnce(&mut ReportDay)) -> ReportDay {
        let mut d = ReportDay {
            date: "2026-10-20".into(),
            worked_minutes: Some(660),
            overtime_minutes: Some(120),
            ..day()
        };
        over(&mut d);
        d
    }

    fn two() -> Vec<ReportEmployee> {
        vec![
            employee(
                "Staff Citra",
                vec![
                    pday(|_| {}),
                    pday(|d| {
                        d.date = "2026-10-24".into();
                        d.day_type = Some("HOLIDAY".into());
                        d.status = "REJECTED".into();
                        d.worked_minutes = None;
                        d.overtime_minutes = None;
                        d.note = Some("Stock opname dijadwalkan ulang ke Senin".into());
                    }),
                ],
            ),
            employee(
                "Staff Dodi",
                vec![pday(|d| {
                    d.date = "2026-10-21".into();
                    d.kind = "ORDER".into();
                    d.requested_by_name = "Manager QA".into();
                    d.overtime_minutes = Some(90);
                })],
            ),
        ]
    }

    const OCT: (&str, &str) = ("2026-10-01", "2026-10-31");

    #[test]
    fn valid_pdf_with_cover_summary_and_page_numbers() {
        let pdf = make(&pdf_meta(), two(), vec![], OCT);
        assert!(pdf.starts_with(b"%PDF-"));
        assert!(pdf.ends_with(b"%%EOF\n"));
        let text = page_texts(&pdf).join("");
        for needle in [
            "LAPORAN LEMBUR",
            "PT QA Lembur",
            "PERIODE LAPORAN",
            "Ringkasan per Karyawan",
            "Halaman 1 dari",
            "Perintah dari Manager QA",
            "Alasan: Stock opname dijadwalkan ulang ke Senin",
            "26 Okt 2026, 12:24 WIB",
        ] {
            assert!(text.contains(needle), "hilang: {needle}");
        }
        assert!(String::from_utf8_lossy(&pdf).contains("Laporan Lembur 2026-10-01 - 2026-10-31"));
    }

    #[test]
    fn page_count_is_summary_plus_employees_with_rows() {
        let pdf = make(&pdf_meta(), two(), vec![], OCT);
        assert_eq!(page_count(&pdf), 3);
        let pages = page_texts(&pdf);
        assert!(pages[1].contains("Staff Citra"));
        assert!(pages[1].contains("RINCIAN LEMBUR"));
        assert!(pages[2].contains("Staff Dodi"));
    }

    #[test]
    fn employee_without_rows_gets_no_page() {
        let mut list = two();
        list.push(employee("Tanpa Baris", vec![]));
        let pdf = make(&pdf_meta(), list, vec![], OCT);
        assert_eq!(page_count(&pdf), 3);
        assert!(!page_texts(&pdf).join("").contains("Tanpa Baris"));
    }

    #[test]
    fn not_final_box_only_when_rows_not_final() {
        let mut list = two();
        list[0].days[0].is_final = false;
        list[0].not_final = true;
        let text = page_texts(&make(&pdf_meta(), list, vec![], OCT)).join("");
        assert!(text.contains("Sebagian angka belum final"));
        assert!(text.contains("Belum final"));
        assert!(text.contains('\u{2022}'));
        assert!(!text.contains('*'));
        let plain = page_texts(&make(&pdf_meta(), two(), vec![], OCT)).join("");
        assert!(!plain.contains("Sebagian angka belum final"));
    }

    #[test]
    fn null_day_type_renders_dash_never_enum_key() {
        let list = vec![employee(
            "Staff Citra",
            vec![pday(|d| {
                d.day_type = None;
                d.status = "PENDING_APPROVAL".into();
            })],
        )];
        let text = page_texts(&make(&pdf_meta(), list, vec![], OCT)).join("");
        assert!(text.lines().any(|l| l == "-"));
        for bad in ["null", "WORKDAY", "HOLIDAY", "dayType"] {
            assert!(!text.contains(bad), "tidak boleh ada: {bad}");
        }
    }

    #[test]
    fn dates_are_indonesian_without_iso_patterns() {
        let text = page_texts(&make(&pdf_meta(), two(), vec![], OCT)).join("");
        assert!(text.contains("01 Okt 2026 -"));
        assert!(text.contains("01 Okt 2026 - 31 Okt 2026"));
        assert!(text.contains("Sel, 20 Okt"));
        let bytes = text.as_bytes();
        let iso = bytes.windows(10).any(|w| {
            w.iter().enumerate().all(|(i, c)| match i {
                4 | 7 => *c == b'-',
                _ => c.is_ascii_digit(),
            })
        });
        assert!(!iso, "ada pola tanggal ISO di konten");
    }

    #[test]
    fn empty_data_is_single_page_with_empty_state() {
        let pdf = make(&pdf_meta(), vec![], vec![], OCT);
        assert_eq!(page_count(&pdf), 1);
        assert!(page_texts(&pdf)[0].contains("Tidak ada lembur pada periode ini"));
    }

    #[test]
    fn emoji_cjk_and_long_notes_stay_inside_pages() {
        let long: String = "Alasan panjang sekali "
            .repeat(30)
            .chars()
            .take(500)
            .collect();
        let days: Vec<ReportDay> = (0..40)
            .map(|i| {
                pday(|d| {
                    d.date = format!("2026-10-{:02}", (i % 31) + 1);
                    d.status = "REJECTED".into();
                    d.worked_minutes = None;
                    d.overtime_minutes = None;
                    d.note = Some(format!("{long} \u{1f680} \u{62d2}\u{7edd}"));
                    d.exceeds_daily_limit = i % 2 == 0;
                    d.exceeds_weekly_limit = i % 3 == 0;
                    d.is_final = i % 4 != 0;
                })
            })
            .collect();
        let pdf = make(
            &pdf_meta(),
            vec![employee("Budi \u{1f680} \u{738b} Santoso", days)],
            vec![],
            OCT,
        );
        let text = page_texts(&pdf).join("");
        assert!(text.contains("Budi   Santoso"));
        for dropped in ["\u{1f680}", "\u{738b}", "\u{62d2}", "\u{7edd}"] {
            assert!(!text.contains(dropped));
        }
        // Tak ada teks di celah antara footer dan batas bawah konten.
        let legal_h = canvas::height_of(LEGAL_NOTE, CW, Font::Regular, 7.0, 0.0);
        let reserve = 74.0_f64.max((58.0 + legal_h).ceil());
        for ys in text_ys(&pdf) {
            assert!(!ys.is_empty());
            for y in ys {
                assert!(y > 0.0 && y < PAGE_H);
                assert!(
                    y <= 31.0 + legal_h || y >= reserve - 6.0,
                    "teks di celah footer: y={y}"
                );
            }
        }
        assert!(page_count(&pdf) > 2);
    }

    #[test]
    fn large_report_finishes_quickly() {
        let start = chrono::NaiveDate::from_ymd_opt(2026, 7, 1).unwrap();
        let dates: Vec<String> = (0..92)
            .map(|i| {
                (start + chrono::Duration::days(i))
                    .format("%Y-%m-%d")
                    .to_string()
            })
            .collect();
        let employees: Vec<ReportEmployee> = (0..200)
            .map(|e| {
                employee(
                    &format!("Karyawan {e:03}"),
                    dates
                        .iter()
                        .enumerate()
                        .map(|(i, date)| {
                            pday(|d| {
                                d.request_id = format!("{e}-{i}");
                                d.date = date.clone();
                                d.day_type =
                                    Some(if i % 7 >= 5 { "HOLIDAY" } else { "WORKDAY" }.into());
                                if i % 5 == 0 {
                                    d.status = "REJECTED".into();
                                    d.note = Some("Tidak mendesak".into());
                                }
                                d.is_final = i < 85;
                            })
                        })
                        .collect(),
                )
            })
            .collect();
        let t = std::time::Instant::now();
        let pdf = make(&pdf_meta(), employees, vec![], (&dates[0], &dates[91]));
        assert!(pdf.starts_with(b"%PDF-"));
        assert!(
            t.elapsed() < std::time::Duration::from_secs(10),
            "{:?}",
            t.elapsed()
        );
    }

    #[test]
    fn legal_note_on_every_page() {
        let pdf = make(&pdf_meta(), two(), vec![], OCT);
        let pages = page_texts(&pdf);
        assert_eq!(pages.len(), page_count(&pdf));
        let all = squash(&pages.join(""));
        assert_eq!(all.matches(&squash(LEGAL_NOTE)).count(), pages.len());
        for p in &pages {
            assert!(squash(p).contains(&squash(LEGAL_NOTE)));
        }
    }

    #[test]
    fn filter_line_weekly_table_and_empty_box() {
        let plain = page_texts(&make(&pdf_meta(), two(), vec![], OCT)).join("");
        assert!(!plain.contains("Filter:"));
        assert!(
            plain.contains("Tidak ada karyawan yang melebihi jam kerja mingguan pada periode ini.")
        );

        let months = vec![WeeklyExcessMonth {
            month: "2026-10".into(),
            employees: vec![WeeklyExcessEmployee {
                employee: EmployeeRef {
                    id: "d".into(),
                    name: "Staff Dodi".into(),
                    email: "dodi@example.com".into(),
                },
                total_excess_minutes: 450,
                weeks: vec![WeeklyExcessWeek {
                    week_start: "2026-10-12".into(),
                    week_end: "2026-10-18".into(),
                    worked_minutes: 2850,
                    normal_minutes: 2400,
                    overtime_minutes: 0,
                    excess_minutes: 450,
                    not_final: false,
                }],
            }],
        }];
        let mut m = pdf_meta();
        m.filters.status = vec!["COMPLETED".into()];
        let text = page_texts(&make(&m, two(), months, OCT)).join("");
        assert!(text.contains("Filter: Status Selesai"));
        assert!(text.contains("12\u{2013}18 Okt"));
        assert!(text.contains("7j 30m"));
    }

    #[test]
    fn limit_chips_use_fx46_labels() {
        let list = vec![employee(
            "Staff Citra",
            vec![
                pday(|d| {
                    d.exceeds_daily_limit = true;
                    d.exceeds_limit = true;
                    d.approved_minutes = 300;
                    d.overtime_minutes = Some(200);
                }),
                pday(|d| {
                    d.date = "2026-10-21".into();
                    d.exceeds_daily_limit = true;
                    d.exceeds_limit = true;
                    d.approved_minutes = 300;
                    d.overtime_minutes = Some(280);
                }),
                pday(|d| {
                    d.date = "2026-10-22".into();
                    d.exceeds_weekly_limit = true;
                    d.exceeds_limit = true;
                }),
                pday(|d| {
                    d.date = "2026-10-23".into();
                    d.worked_minutes = Some(0);
                    d.overtime_minutes = Some(0);
                }),
            ],
        )];
        let text = page_texts(&make(&pdf_meta(), list, vec![], OCT)).join("");
        assert!(text.contains("Disetujui di atas batas (aktual dalam batas)"));
        assert!(text.lines().any(|l| l == "Disetujui di atas batas"));
        assert!(text.contains("Melebihi 18 jam/minggu"));
        assert!(text.contains("Tidak ada jam tercatat"));
    }
}
