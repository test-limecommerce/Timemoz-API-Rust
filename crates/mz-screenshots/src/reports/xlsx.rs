//! Export Excel `GET /reports/export/excel` — tiga sheet (Detailed / Projects / Employees),
//! sama dengan `write-excel-file` di Node (semua sel berupa string, header tebal).

use rust_xlsxwriter::{Format, Workbook, XlsxError};

use super::service::ExportSource;

/// `formatExportDuration`: `Hh Mm Ss`.
pub fn format_export_duration(seconds: i64) -> String {
    let h = seconds / 3600;
    let m = (seconds % 3600) / 60;
    let s = seconds % 60;
    format!("{h}h {m}m {s}s")
}

/// Sel tulis dengan pembatas panjang string Excel (32767 karakter).
fn cell(text: &str) -> String {
    text.chars().take(32_767).collect()
}

pub fn build(data: &ExportSource) -> Result<Vec<u8>, XlsxError> {
    let bold = Format::new().set_bold();
    let mut wb = Workbook::new();

    let sheet = wb.add_worksheet().set_name("Detailed")?;
    for (c, h) in [
        "Date", "Employee", "Project", "Notes", "Duration", "Billable",
    ]
    .iter()
    .enumerate()
    {
        sheet.write_string_with_format(0, u16::try_from(c).unwrap_or(0), *h, &bold)?;
    }
    for (i, r) in data.detailed.iter().enumerate() {
        let row = u32::try_from(i + 1).unwrap_or(u32::MAX);
        sheet.write_string(row, 0, cell(&r.date))?;
        sheet.write_string(row, 1, cell(&r.employee))?;
        sheet.write_string(row, 2, cell(&r.project))?;
        sheet.write_string(row, 3, cell(&r.notes))?;
        sheet.write_string(row, 4, format_export_duration(r.seconds))?;
        sheet.write_string(row, 5, if r.billable { "Yes" } else { "No" })?;
    }

    let sheet = wb.add_worksheet().set_name("Projects")?;
    for (c, h) in ["Project", "Employee", "Duration"].iter().enumerate() {
        sheet.write_string_with_format(0, u16::try_from(c).unwrap_or(0), *h, &bold)?;
    }
    for (i, r) in data.projects.iter().enumerate() {
        let row = u32::try_from(i + 1).unwrap_or(u32::MAX);
        sheet.write_string(row, 0, cell(&r.project))?;
        sheet.write_string(row, 1, cell(&r.employee))?;
        sheet.write_string(row, 2, format_export_duration(r.seconds))?;
    }

    let sheet = wb.add_worksheet().set_name("Employees")?;
    for (c, h) in ["Employee", "Duration"].iter().enumerate() {
        sheet.write_string_with_format(0, u16::try_from(c).unwrap_or(0), *h, &bold)?;
    }
    for (i, r) in data.employees.iter().enumerate() {
        let row = u32::try_from(i + 1).unwrap_or(u32::MAX);
        sheet.write_string(row, 0, cell(&r.employee))?;
        sheet.write_string(row, 1, format_export_duration(r.seconds))?;
    }

    wb.save_to_buffer()
}

#[cfg(test)]
mod tests {
    use super::super::service::{DetailedRow, EmployeeRow, ProjectRow};
    use super::*;

    #[test]
    fn duration_format() {
        assert_eq!(format_export_duration(0), "0h 0m 0s");
        assert_eq!(format_export_duration(3725), "1h 2m 5s");
    }

    #[test]
    fn builds_a_zip_with_three_sheets() {
        let src = ExportSource {
            company_name: "Acme".into(),
            generated_by: "Admin".into(),
            start_date: "2026-01-01".into(),
            end_date: "2026-01-02".into(),
            detailed: vec![DetailedRow {
                date: "2026-01-01".into(),
                employee: "Budi".into(),
                project: "Alpha".into(),
                notes: "-".into(),
                seconds: 3600,
                billable: true,
            }],
            projects: vec![ProjectRow {
                project: "Alpha".into(),
                employee: "Budi".into(),
                seconds: 3600,
            }],
            employees: vec![EmployeeRow {
                employee: "Budi".into(),
                seconds: 3600,
            }],
            time_zone: None,
            timelines: Vec::new(),
        };
        let bytes = build(&src).unwrap_or_default();
        assert!(bytes.starts_with(b"PK"), "xlsx = zip");
        let text = String::from_utf8_lossy(&bytes);
        for sheet in [
            "xl/worksheets/sheet1.xml",
            "xl/worksheets/sheet2.xml",
            "xl/worksheets/sheet3.xml",
        ] {
            assert!(text.contains(sheet), "{sheet}");
        }
    }
}
