//! Tata letak PDF timesheet (`timesheets/timesheet-pdf.generator.ts`) dan invoice
//! (`timesheet-invoices/timesheet-invoice-pdf.generator.ts`) di atas [`crate::pdf`].
//! Isi dan urutan sama dengan pdfkit; tata letak mengikuti koordinat yang sama.

use chrono::NaiveDate;

use crate::calc::{format_date_short, format_duration, money2};
use crate::jsfmt;
use crate::pdf::{Align, Font, PAGE_H, PAGE_W, Pdf, Rgb};

struct Colors;

impl Colors {
    fn primary() -> Rgb {
        Rgb::hex("#1a56db")
    }
    fn secondary() -> Rgb {
        Rgb::hex("#6b7280")
    }
    fn dark() -> Rgb {
        Rgb::hex("#1f2937")
    }
    fn light() -> Rgb {
        Rgb::hex("#f3f4f6")
    }
    fn lighter() -> Rgb {
        Rgb::hex("#f9fafb")
    }
    fn white() -> Rgb {
        Rgb::hex("#ffffff")
    }
    fn border() -> Rgb {
        Rgb::hex("#e5e7eb")
    }
    fn success() -> Rgb {
        Rgb::hex("#059669")
    }
    fn danger() -> Rgb {
        Rgb::hex("#dc2626")
    }
    fn warning() -> Rgb {
        Rgb::hex("#d97706")
    }
}

// ───────────────────────── timesheet ─────────────────────────

#[derive(Debug, Clone)]
pub struct DayProject {
    pub project_name: String,
    pub total_seconds: i64,
}

#[derive(Debug, Clone)]
pub struct DayEntry {
    pub date: NaiveDate,
    pub total_seconds: i64,
    pub total_hours: f64,
    pub projects: Vec<DayProject>,
}

#[derive(Debug, Clone)]
pub struct TimesheetPdfData {
    pub employee_name: String,
    pub employee_email: String,
    pub period_start: NaiveDate,
    pub period_end: NaiveDate,
    pub pay_period_type: String,
    pub status: String,
    pub total_seconds: i64,
    pub pay_rate: Option<f64>,
    pub rate_type: Option<String>,
    pub total_amount: Option<f64>,
    pub currency: Option<String>,
    pub submitted_at: Option<NaiveDate>,
    pub approved_by_name: Option<String>,
    pub rejected_reason: Option<String>,
    pub notes: Option<String>,
    pub billable_seconds: Option<i64>,
    pub non_billable_seconds: Option<i64>,
    pub daily: Vec<DayEntry>,
}

const MARGIN: f64 = 40.0;
const CONTENT_W: f64 = PAGE_W - MARGIN * 2.0;

struct Sheet {
    pdf: Pdf,
    y: f64,
}

impl Sheet {
    fn check_page_break(&mut self, needed: f64) {
        if self.y + needed > PAGE_H - 80.0 {
            self.pdf.add_page();
            self.y = MARGIN;
        }
    }

    fn rule(&mut self, y: f64) {
        self.pdf
            .line(MARGIN, y, PAGE_W - MARGIN, y, Colors::border(), 1.0);
    }
}

pub fn timesheet_pdf(data: &TimesheetPdfData) -> Vec<u8> {
    let mut s = Sheet {
        pdf: Pdf::new(),
        y: MARGIN,
    };
    draw_header(&mut s, data);
    draw_meta(&mut s, data);
    draw_daily_table(&mut s, data);
    draw_summary(&mut s, data);
    draw_footer(&mut s);
    s.pdf.finish()
}

fn draw_header(s: &mut Sheet, d: &TimesheetPdfData) {
    let y0 = s.y;
    s.pdf.text(
        MARGIN,
        y0,
        "Timesheet Report",
        Font::Bold,
        22.0,
        Colors::primary(),
    );
    s.pdf.text(
        MARGIN,
        y0 + 30.0,
        &d.employee_name,
        Font::Regular,
        13.0,
        Colors::dark(),
    );
    s.pdf.text(
        MARGIN,
        y0 + 48.0,
        &d.employee_email,
        Font::Regular,
        10.0,
        Colors::secondary(),
    );

    let bx = PAGE_W - MARGIN - 160.0;
    s.pdf
        .fill_round_rect(bx, y0, 160.0, 50.0, 6.0, Colors::light());
    s.pdf.text(
        bx + 10.0,
        y0 + 10.0,
        "PERIOD",
        Font::Regular,
        8.0,
        Colors::secondary(),
    );
    let period = format!(
        "{} \u{2013} {}",
        format_date_short(d.period_start),
        format_date_short(d.period_end)
    );
    s.pdf.text_box(
        bx + 10.0,
        y0 + 28.0,
        140.0,
        &period,
        Font::Bold,
        10.0,
        Colors::dark(),
        Align::Left,
    );

    s.y = y0 + 70.0;
    let y = s.y;
    s.rule(y);
    s.y += 15.0;
}

fn draw_meta(s: &mut Sheet, d: &TimesheetPdfData) {
    let y0 = s.y;
    let status_color = match d.status.as_str() {
        "APPROVED" => Colors::success(),
        "REJECTED" => Colors::danger(),
        _ => Colors::warning(),
    };
    s.pdf.text(
        MARGIN,
        y0,
        "Status:",
        Font::Regular,
        10.0,
        Colors::secondary(),
    );
    s.pdf
        .text(MARGIN + 45.0, y0, &d.status, Font::Bold, 10.0, status_color);
    s.pdf.text(
        MARGIN + 140.0,
        y0,
        "Pay Period:",
        Font::Regular,
        10.0,
        Colors::secondary(),
    );
    s.pdf.text(
        MARGIN + 200.0,
        y0,
        &d.pay_period_type,
        Font::Bold,
        10.0,
        Colors::dark(),
    );
    if let Some(sub) = d.submitted_at {
        s.pdf.text(
            MARGIN + 310.0,
            y0,
            "Submitted:",
            Font::Regular,
            10.0,
            Colors::secondary(),
        );
        s.pdf.text(
            MARGIN + 365.0,
            y0,
            &format_date_short(sub),
            Font::Bold,
            10.0,
            Colors::dark(),
        );
    }
    s.y = y0 + 25.0;
}

fn draw_daily_table(s: &mut Sheet, d: &TimesheetPdfData) {
    s.check_page_break(60.0);
    let col_date = MARGIN;
    let col_project = MARGIN + 100.0;
    let col_duration = MARGIN + 380.0;
    let col_hours = MARGIN + 460.0;

    s.pdf
        .fill_rect(MARGIN, s.y, CONTENT_W, 26.0, Colors::dark());
    for (x, label) in [
        (col_date + 8.0, "Date"),
        (col_project, "Project"),
        (col_duration, "Duration"),
        (col_hours, "Hours"),
    ] {
        s.pdf
            .text(x, s.y + 9.0, label, Font::Bold, 9.0, Colors::white());
    }
    s.y += 26.0;

    for (day_index, day) in d.daily.iter().enumerate() {
        let rows_needed = (day.projects.len() + 1) as f64;
        s.check_page_break(rows_needed * 22.0 + 10.0);
        let bg = if day_index % 2 == 0 {
            Colors::lighter()
        } else {
            Colors::white()
        };

        for (pi, project) in day.projects.iter().enumerate() {
            s.pdf.fill_rect(MARGIN, s.y, CONTENT_W, 22.0, bg);
            if pi == 0 {
                s.pdf.text_box(
                    col_date + 8.0,
                    s.y + 7.0,
                    90.0,
                    &format_date_short(day.date),
                    Font::Bold,
                    9.0,
                    Colors::dark(),
                    Align::Left,
                );
            }
            s.pdf.text_box(
                col_project,
                s.y + 7.0,
                270.0,
                &project.project_name,
                Font::Regular,
                9.0,
                Colors::dark(),
                Align::Left,
            );
            s.pdf.text(
                col_duration,
                s.y + 7.0,
                &format_duration(project.total_seconds),
                Font::Regular,
                9.0,
                Colors::dark(),
            );
            s.pdf.text(
                col_hours,
                s.y + 7.0,
                &jsfmt::num_string(crate::calc::hours2(project.total_seconds)),
                Font::Regular,
                9.0,
                Colors::secondary(),
            );
            s.y += 22.0;
        }

        s.pdf
            .fill_rect(MARGIN, s.y, CONTENT_W, 22.0, Colors::light());
        s.pdf.text(
            col_project,
            s.y + 7.0,
            "Daily Total",
            Font::Bold,
            9.0,
            Colors::secondary(),
        );
        s.pdf.text(
            col_duration,
            s.y + 7.0,
            &format_duration(day.total_seconds),
            Font::Bold,
            9.0,
            Colors::dark(),
        );
        s.pdf.text(
            col_hours,
            s.y + 7.0,
            &jsfmt::to_fixed(day.total_hours, 2),
            Font::Bold,
            9.0,
            Colors::dark(),
        );
        s.y += 22.0 + 2.0;
    }
    s.y += 10.0;
}

fn draw_summary(s: &mut Sheet, d: &TimesheetPdfData) {
    s.check_page_break(120.0);
    let y = s.y;
    s.rule(y);
    s.y += 15.0;
    s.pdf
        .text(MARGIN, s.y, "SUMMARY", Font::Bold, 13.0, Colors::dark());
    s.y += 20.0;

    let cur = d.currency.clone().unwrap_or_default();
    let mut rows: Vec<(String, String)> = vec![
        (
            "Total Working Hours".into(),
            format_duration(d.total_seconds),
        ),
        (
            "Total Working Days".into(),
            format!("{} days", d.daily.len()),
        ),
    ];
    if let Some(b) = d.billable_seconds {
        rows.push(("Billable Hours".into(), format_duration(b)));
        rows.push((
            "Non-Billable Hours".into(),
            format_duration(d.non_billable_seconds.unwrap_or(0)),
        ));
    }
    if let (Some(rate), Some(rt)) = (d.pay_rate, d.rate_type.as_deref().filter(|r| !r.is_empty())) {
        rows.push((
            "Pay Rate".into(),
            format!("{cur} {} ({rt})", jsfmt::locale(rate)),
        ));
    }
    if let Some(a) = d.total_amount {
        rows.push(("Total Amount".into(), format!("{cur} {}", jsfmt::locale(a))));
    }
    if let Some(n) = d.approved_by_name.as_deref().filter(|n| !n.is_empty()) {
        rows.push(("Reviewed by".into(), n.to_owned()));
    }
    if let Some(n) = d.rejected_reason.as_deref().filter(|n| !n.is_empty()) {
        rows.push(("Reviewer's Note".into(), n.to_owned()));
    }
    if let Some(n) = d.notes.as_deref().filter(|n| !n.is_empty()) {
        rows.push(("Employee Notes".into(), n.to_owned()));
    }

    for (label, value) in rows {
        s.check_page_break(20.0);
        s.pdf.text_box(
            MARGIN,
            s.y,
            180.0,
            &label,
            Font::Regular,
            10.0,
            Colors::secondary(),
            Align::Left,
        );
        s.pdf.text_box(
            MARGIN + 190.0,
            s.y,
            CONTENT_W - 190.0,
            &value,
            Font::Bold,
            10.0,
            Colors::dark(),
            Align::Left,
        );
        s.y += 20.0;
    }
}

fn draw_footer(s: &mut Sheet) {
    let fy = PAGE_H - 80.0;
    s.rule(fy);
    s.pdf.text_box(
        MARGIN,
        fy + 12.0,
        CONTENT_W,
        "Generated by Timemoz | www.timemoz.com",
        Font::Regular,
        8.0,
        Colors::secondary(),
        Align::Center,
    );
}

// ───────────────────────── invoice ─────────────────────────

#[derive(Debug, Clone)]
pub struct InvoicePdfData {
    pub invoice_number: String,
    pub status: String,
    pub issued_at: Option<String>,
    pub company_name: String,
    pub employee_name: String,
    pub employee_email: String,
    pub period_start: String,
    pub period_end: String,
    pub pay_period_type: String,
    pub total_hours: f64,
    pub pay_rate: Option<f64>,
    pub rate_type: String,
    pub total_amount: f64,
    pub currency: String,
    pub notes: Option<String>,
}

fn status_color(status: &str) -> Rgb {
    match status {
        "PAID" => Colors::success(),
        "CANCELLED" => Colors::danger(),
        "ISSUED" => Colors::primary(),
        _ => Colors::warning(),
    }
}

fn pay_period_label(t: &str) -> String {
    match t {
        "WEEKLY" => "Weekly Pay Period".into(),
        "BIWEEKLY" => "Bi-Weekly Pay Period".into(),
        "MONTHLY" => "Monthly Pay Period".into(),
        other => other.to_owned(),
    }
}

/// `formatPayRate`: `!payRate` (null atau 0) = `N/A`.
pub fn format_pay_rate(d: &InvoicePdfData) -> String {
    match d.pay_rate.filter(|r| *r != 0.0) {
        None => "N/A".into(),
        Some(r) if d.rate_type == "HOURLY" => format!("{} {} / hour", d.currency, money2(r)),
        Some(r) => format!("Fixed Rate \u{2014} {} {}", d.currency, money2(r)),
    }
}

/// `formatCompensationDetail`.
pub fn format_compensation_detail(d: &InvoicePdfData) -> String {
    match d.pay_rate.filter(|r| *r != 0.0) {
        Some(r) if d.rate_type == "HOURLY" => format!(
            "{} hrs \u{00D7} {} {}/hr",
            jsfmt::to_fixed(d.total_hours, 2),
            d.currency,
            money2(r)
        ),
        _ => "Fixed Compensation".into(),
    }
}

pub fn invoice_pdf(d: &InvoicePdfData) -> Vec<u8> {
    let mut p = Pdf::new();
    let w = PAGE_W;

    // Header: latar biru (primary #1a56db) dengan teks putih, sama dengan Node.
    p.fill_rect(0.0, 0.0, w, 80.0, Colors::primary());
    p.text_box(
        50.0,
        22.0,
        300.0,
        &d.company_name,
        Font::Regular,
        18.0,
        Colors::white(),
        Align::Left,
    );
    p.text_box(
        50.0,
        28.0,
        w - 100.0,
        "PAYROLL INVOICE",
        Font::Regular,
        11.0,
        Colors::white(),
        Align::Right,
    );

    // Meta
    let y = 100.0;
    p.text(
        50.0,
        y,
        "INVOICE NUMBER",
        Font::Regular,
        9.0,
        Colors::secondary(),
    );
    p.text_spaced(
        50.0,
        y + 14.0,
        &d.invoice_number,
        Font::Regular,
        11.0,
        Colors::dark(),
        0.5,
    );
    p.text(
        220.0,
        y,
        "ISSUE DATE",
        Font::Regular,
        9.0,
        Colors::secondary(),
    );
    p.text(
        220.0,
        y + 14.0,
        d.issued_at.as_deref().unwrap_or("-"),
        Font::Regular,
        11.0,
        Colors::dark(),
    );
    let bx = w - 150.0;
    p.fill_round_rect(bx, y, 100.0, 24.0, 4.0, status_color(&d.status));
    p.text_box(
        bx,
        y + 8.0,
        100.0,
        &d.status,
        Font::Regular,
        9.0,
        Colors::white(),
        Align::Center,
    );
    p.line(50.0, y + 42.0, w - 50.0, y + 42.0, Colors::border(), 0.5);

    // Bill to
    let y = 162.0;
    p.fill_rect(50.0, y, 240.0, 72.0, Colors::light());
    p.text(
        62.0,
        y + 10.0,
        "BILL TO",
        Font::Regular,
        8.0,
        Colors::secondary(),
    );
    p.text(
        62.0,
        y + 24.0,
        &d.employee_name,
        Font::Regular,
        12.0,
        Colors::dark(),
    );
    p.text(
        62.0,
        y + 42.0,
        &d.employee_email,
        Font::Regular,
        9.0,
        Colors::secondary(),
    );
    p.fill_rect(310.0, y, 235.0, 72.0, Colors::lighter());
    p.text(
        322.0,
        y + 10.0,
        "PAY PERIOD",
        Font::Regular,
        8.0,
        Colors::secondary(),
    );
    p.text_box(
        322.0,
        y + 24.0,
        210.0,
        &format!("{}  \u{2013}  {}", d.period_start, d.period_end),
        Font::Regular,
        10.0,
        Colors::dark(),
        Align::Left,
    );
    p.text(
        322.0,
        y + 42.0,
        &pay_period_label(&d.pay_period_type),
        Font::Regular,
        9.0,
        Colors::secondary(),
    );

    // Tabel
    let top = 260.0;
    let table_w = w - 100.0;
    let row_h = 32.0;
    let (c_desc, c_detail, c_amount) = (50.0, 270.0, w - 150.0);
    p.fill_rect(50.0, top, table_w, row_h, Colors::primary());
    p.text(
        c_desc + 8.0,
        top + 11.0,
        "Description",
        Font::Regular,
        9.0,
        Colors::white(),
    );
    p.text(
        c_detail,
        top + 11.0,
        "Detail",
        Font::Regular,
        9.0,
        Colors::white(),
    );
    p.text(
        c_amount,
        top + 11.0,
        "Amount",
        Font::Regular,
        9.0,
        Colors::white(),
    );

    let r1 = top + row_h;
    p.fill_rect(50.0, r1, table_w, row_h, Colors::light());
    p.text(
        c_desc + 8.0,
        r1 + 11.0,
        "Total Hours Worked",
        Font::Regular,
        9.0,
        Colors::dark(),
    );
    p.text(
        c_detail,
        r1 + 11.0,
        &format!("{} hours", jsfmt::to_fixed(d.total_hours, 2)),
        Font::Regular,
        9.0,
        Colors::dark(),
    );
    p.text(c_amount, r1 + 11.0, "-", Font::Regular, 9.0, Colors::dark());

    let r2 = r1 + row_h;
    p.fill_rect(50.0, r2, table_w, row_h, Colors::white());
    p.text(
        c_desc + 8.0,
        r2 + 11.0,
        "Pay Rate",
        Font::Regular,
        9.0,
        Colors::dark(),
    );
    p.text(
        c_detail,
        r2 + 11.0,
        &format_pay_rate(d),
        Font::Regular,
        9.0,
        Colors::dark(),
    );
    p.text(c_amount, r2 + 11.0, "-", Font::Regular, 9.0, Colors::dark());

    let r3 = r2 + row_h;
    p.fill_rect(50.0, r3, table_w, row_h, Colors::light());
    p.text(
        c_desc + 8.0,
        r3 + 11.0,
        "Gross Compensation",
        Font::Regular,
        9.0,
        Colors::dark(),
    );
    p.text(
        c_detail,
        r3 + 11.0,
        &format_compensation_detail(d),
        Font::Regular,
        9.0,
        Colors::dark(),
    );
    p.text(
        c_amount,
        r3 + 10.0,
        &format!("{} {}", d.currency, money2(d.total_amount)),
        Font::Regular,
        10.0,
        Colors::dark(),
    );
    p.line(
        50.0,
        r3 + row_h,
        w - 50.0,
        r3 + row_h,
        Colors::border(),
        0.5,
    );

    // Total
    let y = 390.0;
    let box_w = 200.0;
    let box_x = w - 50.0 - box_w;
    p.fill_rect(box_x, y, box_w, 40.0, Colors::primary());
    p.text(
        box_x + 12.0,
        y + 8.0,
        "TOTAL DUE",
        Font::Regular,
        9.0,
        Colors::white(),
    );
    p.text_box(
        box_x,
        y + 14.0,
        box_w - 12.0,
        &format!("{} {}", d.currency, money2(d.total_amount)),
        Font::Regular,
        13.0,
        Colors::white(),
        Align::Right,
    );

    // Footer
    let fy = PAGE_H - 90.0;
    p.line(50.0, fy, w - 50.0, fy, Colors::border(), 0.5);
    if let Some(n) = d.notes.as_deref().filter(|n| !n.is_empty()) {
        p.text_box(
            50.0,
            fy + 10.0,
            w - 100.0,
            &format!("Notes: {n}"),
            Font::Regular,
            8.0,
            Colors::secondary(),
            Align::Left,
        );
    }
    p.text_box(
        50.0,
        PAGE_H - 40.0,
        w - 100.0,
        "This is a system-generated payroll invoice.",
        Font::Regular,
        7.5,
        Colors::secondary(),
        Align::Center,
    );
    p.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap_or_default()
    }

    fn invoice(rate: Option<f64>, rate_type: &str) -> InvoicePdfData {
        InvoicePdfData {
            invoice_number: "INV-TS-202604-0001".into(),
            status: "ISSUED".into(),
            issued_at: Some("01 Apr 2026".into()),
            company_name: "Acme".into(),
            employee_name: "Budi".into(),
            employee_email: "budi@x.com".into(),
            period_start: "01 Apr 2026".into(),
            period_end: "30 Apr 2026".into(),
            pay_period_type: "MONTHLY".into(),
            total_hours: 12.5,
            pay_rate: rate,
            rate_type: rate_type.into(),
            total_amount: 3200.0,
            currency: "USD".into(),
            notes: None,
        }
    }

    #[test]
    fn invoice_texts() {
        assert_eq!(
            format_pay_rate(&invoice(Some(256.0), "HOURLY")),
            "USD 256.00 / hour"
        );
        assert_eq!(
            format_pay_rate(&invoice(Some(3200.0), "FIXED")),
            "Fixed Rate \u{2014} USD 3,200.00"
        );
        assert_eq!(format_pay_rate(&invoice(None, "HOURLY")), "N/A");
        assert_eq!(format_pay_rate(&invoice(Some(0.0), "HOURLY")), "N/A");
        assert_eq!(
            format_compensation_detail(&invoice(Some(256.0), "HOURLY")),
            "12.50 hrs \u{00D7} USD 256.00/hr"
        );
        assert_eq!(
            format_compensation_detail(&invoice(Some(256.0), "FIXED")),
            "Fixed Compensation"
        );
        assert_eq!(pay_period_label("BIWEEKLY"), "Bi-Weekly Pay Period");
        assert_eq!(pay_period_label("X"), "X");
    }

    #[test]
    fn invoice_pdf_is_pdf() {
        let bytes = invoice_pdf(&invoice(Some(256.0), "HOURLY"));
        assert!(bytes.starts_with(b"%PDF-1.3"));
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.contains("INV-TS-202604-0001"));
        assert!(s.contains("TOTAL DUE"));
        assert!(s.contains("USD 3,200.00"));
    }

    #[test]
    fn timesheet_pdf_paginates_and_lists_rows() {
        let daily: Vec<DayEntry> = (1..=28)
            .map(|day| DayEntry {
                date: d(2026, 2, day),
                total_seconds: 3600,
                total_hours: 1.0,
                projects: vec![
                    DayProject {
                        project_name: "Alpha".into(),
                        total_seconds: 1800,
                    },
                    DayProject {
                        project_name: "Beta".into(),
                        total_seconds: 1800,
                    },
                ],
            })
            .collect();
        let data = TimesheetPdfData {
            employee_name: "Budi".into(),
            employee_email: "b@x.com".into(),
            period_start: d(2026, 2, 1),
            period_end: d(2026, 2, 28),
            pay_period_type: "MONTHLY".into(),
            status: "APPROVED".into(),
            total_seconds: 100_800,
            pay_rate: Some(10.0),
            rate_type: Some("HOURLY".into()),
            total_amount: Some(280.0),
            currency: Some("USD".into()),
            submitted_at: Some(d(2026, 3, 1)),
            approved_by_name: Some("Ani".into()),
            rejected_reason: None,
            notes: Some("ok".into()),
            billable_seconds: Some(100_800),
            non_billable_seconds: Some(0),
            daily,
        };
        let bytes = timesheet_pdf(&data);
        let s = String::from_utf8_lossy(&bytes);
        assert!(!s.contains("/Count 1 "));
        assert!(s.contains("Timesheet Report"));
        assert!(s.contains("Total Working Days"));
        assert!(s.contains("28 days"));
        assert!(s.contains("Reviewed by"));
        assert!(s.contains("USD 10 \\(HOURLY\\)"));
    }
}
