//! Port `src/mobile-clock/attendance-excel.generator.ts` (`write-excel-file`): sheet "Summary"
//! dan "Attendance". Waktu dalam UTC.

use chrono::{DateTime, Utc};
use rust_xlsxwriter::{Color, Format, Workbook, Worksheet, XlsxError};

use crate::attendance::ExportData;

/// `formatTimestamp` Excel: `YYYY-MM-DD HH:mm:ss UTC`.
pub fn format_timestamp(ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(ms)
        .map(|d| d.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_default()
}

/// `formatDuration` Excel: `Hh Mm Ss` / `Mm Ss` / `Ss`, `-` bila null.
pub fn format_duration(seconds: Option<i64>) -> String {
    let Some(s) = seconds else {
        return "-".to_owned();
    };
    let h = s.div_euclid(3600);
    let m = s.rem_euclid(3600).div_euclid(60);
    let sec = s.rem_euclid(60);
    if h > 0 {
        format!("{h}h {m}m {sec}s")
    } else if m > 0 {
        format!("{m}m {sec}s")
    } else {
        format!("{sec}s")
    }
}

fn cell(text: &str) -> String {
    text.chars().take(32_767).collect()
}

fn num_or_dash(ws: &mut Worksheet, row: u32, col: u16, v: Option<f64>) -> Result<(), XlsxError> {
    match v {
        Some(n) => ws.write_number(row, col, n).map(|_| ()),
        None => ws.write_string(row, col, "-").map(|_| ()),
    }
}

const HEADERS: [&str; 20] = [
    "No",
    "Employee Name",
    "Username",
    "Clock In",
    "Clock In Geofence",
    "Clock In Lat",
    "Clock In Lng",
    "Clock In Accuracy (m)",
    "Clock In Mock",
    "Clock In Verification",
    "Clock Out",
    "Clock Out Geofence",
    "Clock Out Lat",
    "Clock Out Lng",
    "Clock Out Accuracy (m)",
    "Clock Out Mock",
    "Clock Out Verification",
    "Duration",
    "Status",
    "Note",
];

/// `generateAttendanceExcel(data)`.
pub fn build(data: &ExportData) -> Result<Vec<u8>, XlsxError> {
    let bold = Format::new().set_bold();
    let header = Format::new()
        .set_bold()
        .set_background_color(Color::RGB(0x001a_56db))
        .set_font_color(Color::RGB(0x00ff_ffff));
    let mut wb = Workbook::new();

    let ws = wb.add_worksheet().set_name("Summary")?;
    ws.merge_range(0, 0, 0, 1, "ATTENDANCE REPORT SUMMARY", &bold)?;
    let s = &data.summary;
    let lines: [(&str, Option<f64>, Option<String>); 6] = [
        (
            "Period",
            None,
            Some(format!("{} to {}", data.start_date, data.end_date)),
        ),
        ("Total Present", Some(s.total_present as f64), None),
        ("Total Late", Some(s.total_late as f64), None),
        ("Total Absent", Some(s.total_absent as f64), None),
        (
            "Average Work Duration",
            None,
            Some(format_duration(s.average_work_seconds)),
        ),
        ("Total Records", Some(data.rows.len() as f64), None),
    ];
    for (i, (label, n, text)) in lines.into_iter().enumerate() {
        let row = u32::try_from(i + 2).unwrap_or(u32::MAX);
        ws.write_string_with_format(row, 0, label, &bold)?;
        match (n, text) {
            (Some(n), _) => {
                ws.write_number(row, 1, n)?;
            }
            (None, Some(t)) => {
                ws.write_string(row, 1, cell(&t))?;
            }
            (None, None) => {}
        }
    }

    let ws = wb.add_worksheet().set_name("Attendance")?;
    for (c, h) in HEADERS.iter().enumerate() {
        ws.write_string_with_format(0, u16::try_from(c).unwrap_or(0), *h, &header)?;
    }
    for (i, r) in data.rows.iter().enumerate() {
        let row = u32::try_from(i + 1).unwrap_or(u32::MAX);
        let yes_no = |b: bool| if b { "Yes" } else { "No" };
        ws.write_number(row, 0, (i + 1) as f64)?;
        ws.write_string(row, 1, cell(&r.user_name))?;
        ws.write_string(row, 2, cell(&r.username))?;
        ws.write_string(row, 3, format_timestamp(r.clock_in.at_ms))?;
        ws.write_string(
            row,
            4,
            cell(r.clock_in.geofence_name.as_deref().unwrap_or("-")),
        )?;
        num_or_dash(ws, row, 5, r.clock_in.lat)?;
        num_or_dash(ws, row, 6, r.clock_in.lng)?;
        num_or_dash(ws, row, 7, r.clock_in.accuracy)?;
        ws.write_string(row, 8, yes_no(r.clock_in.is_mock))?;
        ws.write_string(row, 9, r.clock_in.verification.as_deref().unwrap_or("-"))?;
        let out = r.clock_out.as_ref();
        ws.write_string(
            row,
            10,
            out.map_or_else(|| "-".to_owned(), |o| format_timestamp(o.at_ms)),
        )?;
        ws.write_string(
            row,
            11,
            cell(out.and_then(|o| o.geofence_name.as_deref()).unwrap_or("-")),
        )?;
        num_or_dash(ws, row, 12, out.and_then(|o| o.lat))?;
        num_or_dash(ws, row, 13, out.and_then(|o| o.lng))?;
        num_or_dash(ws, row, 14, out.and_then(|o| o.accuracy))?;
        ws.write_string(row, 15, out.map_or("-", |o| yes_no(o.is_mock)))?;
        ws.write_string(
            row,
            16,
            out.and_then(|o| o.verification.as_deref()).unwrap_or("-"),
        )?;
        ws.write_string(row, 17, format_duration(r.duration_seconds))?;
        ws.write_string(row, 18, &r.status)?;
        ws.write_string(row, 19, cell(r.note.as_deref().unwrap_or("-")))?;
    }

    wb.save_to_buffer()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attendance::{ExportRow, Punch, Summary};

    #[test]
    fn formats() {
        assert_eq!(format_duration(None), "-");
        assert_eq!(format_duration(Some(5)), "5s");
        assert_eq!(format_duration(Some(65)), "1m 5s");
        assert_eq!(format_duration(Some(3725)), "1h 2m 5s");
        assert_eq!(
            format_timestamp(1_749_430_800_000),
            "2025-06-09 01:00:00 UTC"
        );
    }

    #[test]
    fn builds_workbook() {
        let data = ExportData {
            start_date: "2026-06-01".into(),
            end_date: "2026-06-30".into(),
            summary: Summary {
                total_present: 1,
                total_late: 0,
                total_absent: 2,
                average_work_seconds: None,
            },
            rows: vec![ExportRow {
                user_name: "Budi".into(),
                username: "budi".into(),
                clock_in: Punch {
                    at_ms: 1_749_430_800_000,
                    lat: Some(-6.2),
                    lng: None,
                    accuracy: Some(10.0),
                    is_mock: false,
                    verification: Some("verified".into()),
                    geofence_name: None,
                },
                clock_out: None,
                duration_seconds: None,
                late_seconds: Some(60),
                status: "active".into(),
                note: None,
            }],
        };
        let bytes = build(&data).unwrap_or_default();
        assert!(bytes.starts_with(b"PK"));
    }
}
