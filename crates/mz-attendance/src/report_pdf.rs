//! Port `src/mobile-clock/attendance-pdf.generator.ts` (`AttendancePdfGenerator`, pdfkit A4
//! landscape, margin 36): judul + lencana periode, empat kartu ringkasan, tabel absensi, nomor
//! halaman. Waktu dalam UTC.

use chrono::{DateTime, Utc};

use crate::attendance::ExportData;
use crate::pdf::{Align, Font, Pdf, Rgb};

const PAGE_W: f64 = 841.89;
const PAGE_H: f64 = 595.28;
const MARGIN: f64 = 36.0;
const CONTENT_W: f64 = PAGE_W - MARGIN * 2.0;

fn c(hex: &str) -> Rgb {
    Rgb::hex(hex)
}

const PRIMARY: &str = "#1a56db";
const SECONDARY: &str = "#6b7280";
const SUCCESS: &str = "#059669";
const WARNING: &str = "#d97706";
const DANGER: &str = "#dc2626";
const DARK: &str = "#1f2937";
const LIGHT: &str = "#f3f4f6";
const LIGHTER: &str = "#f9fafb";
const WHITE: &str = "#ffffff";
const BORDER: &str = "#e5e7eb";
const ROW_EVEN: &str = "#f9fafb";
const ROW_ODD: &str = "#ffffff";

/// `formatTimestamp` PDF: `YYYY-MM-DD HH:mm UTC`.
pub fn format_timestamp(ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(ms)
        .map(|d| d.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_default()
}

/// `formatDuration` PDF: `Hh Mm` / `Mm`, `-` bila null.
pub fn format_duration(seconds: Option<i64>) -> String {
    let Some(s) = seconds else {
        return "-".to_owned();
    };
    let h = s.div_euclid(3600);
    let m = s.rem_euclid(3600).div_euclid(60);
    if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m")
    }
}

struct Gen {
    doc: Pdf,
    y: f64,
}

impl Gen {
    /// `checkPageBreak(needed)`.
    fn check_page_break(&mut self, needed: f64) {
        if self.y + needed > PAGE_H - 50.0 {
            self.doc.add_page();
            self.y = MARGIN;
        }
    }

    fn header(&mut self, data: &ExportData) {
        let start = self.y;
        self.doc.text_box(
            MARGIN,
            start,
            PAGE_W - MARGIN - MARGIN,
            "ATTENDANCE REPORT",
            Font::Bold,
            20.0,
            c(PRIMARY),
            Align::Left,
        );
        let badge_x = PAGE_W - MARGIN - 200.0;
        self.doc
            .fill_round_rect(badge_x, start, 200.0, 44.0, 5.0, c(LIGHT));
        self.doc.text(
            badge_x + 10.0,
            start + 8.0,
            "PERIOD",
            Font::Regular,
            8.0,
            c(SECONDARY),
        );
        self.doc.text_box(
            badge_x + 10.0,
            start + 22.0,
            PAGE_W - (badge_x + 10.0) - MARGIN,
            &format!("{}  to  {}", data.start_date, data.end_date),
            Font::Bold,
            10.0,
            c(DARK),
            Align::Left,
        );
        self.y = start + 55.0;
        self.doc
            .line(MARGIN, self.y, PAGE_W - MARGIN, self.y, c(BORDER), 0.5);
        self.y += 14.0;
    }

    fn summary(&mut self, data: &ExportData) {
        self.check_page_break(70.0);
        let start = self.y;
        let card_w = (CONTENT_W - 30.0) / 4.0;
        let s = &data.summary;
        let cards = [
            ("Total Present", s.total_present.to_string(), PRIMARY),
            ("Total Late", s.total_late.to_string(), WARNING),
            ("Total Absent", s.total_absent.to_string(), DANGER),
            (
                "Avg Work Duration",
                format_duration(s.average_work_seconds),
                SUCCESS,
            ),
        ];
        for (i, (label, value, color)) in cards.iter().enumerate() {
            #[allow(clippy::cast_precision_loss)]
            let x = MARGIN + i as f64 * (card_w + 10.0);
            self.doc
                .fill_round_rect(x, start, card_w, 52.0, 5.0, c(LIGHTER));
            self.doc.fill_rect(x, start, 4.0, 52.0, c(color));
            self.doc.text(
                x + 12.0,
                start + 9.0,
                label,
                Font::Regular,
                8.0,
                c(SECONDARY),
            );
            self.doc
                .text(x + 12.0, start + 24.0, value, Font::Bold, 16.0, c(DARK));
        }
        self.y = start + 64.0;
    }

    fn table_header(&mut self) {
        let mut x = MARGIN;
        self.doc
            .fill_rect(x, self.y, CONTENT_W, HEADER_H, c(PRIMARY));
        for (label, w) in COLS {
            self.doc.text_box(
                x + 4.0,
                self.y + 7.0,
                w - 6.0,
                label,
                Font::Bold,
                FONT_SIZE,
                c(WHITE),
                Align::Left,
            );
            x += w;
        }
        self.y += HEADER_H;
    }

    fn table(&mut self, data: &ExportData) {
        self.check_page_break(HEADER_H + ROW_H);
        self.table_header();
        for (idx, row) in data.rows.iter().enumerate() {
            if self.y + ROW_H > PAGE_H - 50.0 {
                self.doc.add_page();
                self.y = MARGIN;
                self.table_header();
            }
            let bg = if idx % 2 == 0 { ROW_EVEN } else { ROW_ODD };
            self.doc.fill_rect(MARGIN, self.y, CONTENT_W, ROW_H, c(bg));
            let cells = [
                (idx + 1).to_string(),
                row.user_name.clone(),
                format_timestamp(row.clock_in.at_ms),
                row.clock_in
                    .geofence_name
                    .clone()
                    .unwrap_or_else(|| "-".to_owned()),
                row.clock_out
                    .as_ref()
                    .map_or_else(|| "-".to_owned(), |o| format_timestamp(o.at_ms)),
                row.clock_out
                    .as_ref()
                    .and_then(|o| o.geofence_name.clone())
                    .unwrap_or_else(|| "-".to_owned()),
                format_duration(row.duration_seconds),
                format_duration(row.late_seconds),
                row.status.clone(),
                row.note.clone().unwrap_or_else(|| "-".to_owned()),
            ];
            let mut x = MARGIN;
            let cell_y = self.y + 6.0;
            for ((_, w), value) in COLS.iter().zip(cells.iter()) {
                self.doc.text_box(
                    x + 4.0,
                    cell_y,
                    w - 6.0,
                    value,
                    Font::Regular,
                    FONT_SIZE,
                    c(DARK),
                    Align::Left,
                );
                x += w;
            }
            self.doc.line(
                MARGIN,
                self.y + ROW_H,
                MARGIN + CONTENT_W,
                self.y + ROW_H,
                c(BORDER),
                0.3,
            );
            self.y += ROW_H;
        }
        if data.rows.is_empty() {
            self.check_page_break(ROW_H);
            self.doc.text_box(
                MARGIN + 4.0,
                self.y + 6.0,
                PAGE_W - (MARGIN + 4.0) - MARGIN,
                "No attendance records found for the selected period.",
                Font::Regular,
                FONT_SIZE,
                c(SECONDARY),
                Align::Left,
            );
            self.y += ROW_H;
        }
    }

    fn page_numbers(&mut self) {
        let count = self.doc.page_count();
        for i in 0..count {
            self.doc.switch_to_page(i);
            self.doc.text_box(
                MARGIN,
                PAGE_H - MARGIN - 14.0,
                CONTENT_W,
                &format!("Page {} of {count}", i + 1),
                Font::Regular,
                8.0,
                c(SECONDARY),
                Align::Right,
            );
        }
    }
}

const ROW_H: f64 = 20.0;
const HEADER_H: f64 = 22.0;
const FONT_SIZE: f64 = 7.5;
const COLS: [(&str, f64); 10] = [
    ("#", 24.0),
    ("Employee", 95.0),
    ("Clock In", 105.0),
    ("In Geofence", 90.0),
    ("Clock Out", 105.0),
    ("Out Geofence", 90.0),
    ("Duration", 55.0),
    ("Late By", 55.0),
    ("Status", 62.0),
    ("Note", 88.0),
];

/// `AttendancePdfGenerator.generate(data)`.
pub fn build(data: &ExportData) -> Vec<u8> {
    let mut g = Gen {
        doc: Pdf::new(PAGE_W, PAGE_H),
        y: MARGIN,
    };
    g.header(data);
    g.summary(data);
    g.table(data);
    g.page_numbers();
    g.doc.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attendance::{ExportRow, Punch, Summary};

    fn row(i: usize) -> ExportRow {
        ExportRow {
            user_name: format!("User {i}"),
            username: format!("u{i}"),
            clock_in: Punch {
                at_ms: 1_749_430_800_000,
                lat: None,
                lng: None,
                accuracy: None,
                is_mock: false,
                verification: None,
                geofence_name: Some("Kantor".into()),
            },
            clock_out: None,
            duration_seconds: Some(3700),
            late_seconds: None,
            status: "closed".into(),
            note: None,
        }
    }

    #[test]
    fn formats() {
        assert_eq!(format_duration(Some(3700)), "1h 1m");
        assert_eq!(format_duration(Some(59)), "0m");
        assert_eq!(format_timestamp(1_749_430_800_000), "2025-06-09 01:00 UTC");
    }

    #[test]
    fn paginates_and_numbers_pages() {
        let data = ExportData {
            start_date: "2026-06-01".into(),
            end_date: "2026-06-30".into(),
            summary: Summary {
                total_present: 40,
                total_late: 1,
                total_absent: 0,
                average_work_seconds: Some(3600),
            },
            rows: (0..40).map(row).collect(),
        };
        let s = String::from_utf8_lossy(&build(&data)).into_owned();
        // Halaman 1: header tabel y=169, 17 baris; halaman 2: 23 baris sisa.
        assert!(s.contains("/Count 2"), "{}", &s[..200.min(s.len())]);
        assert!(s.contains("(Page 2 of 2)"));
        assert!(s.contains("(ATTENDANCE REPORT)"));
        assert!(s.contains("(2026-06-01  to  2026-06-30)"));
        let empty = ExportData {
            rows: Vec::new(),
            ..data
        };
        let s = String::from_utf8_lossy(&build(&empty)).into_owned();
        assert!(s.contains("No attendance records found for the selected period."));
        assert!(s.contains("/Count 1"));
    }
}
