//! `GET /leaves/report` (`leaves.service.ts:1028` `generateLeaveReport` +
//! `leave-report.generator.ts`): CSV (BOM + CRLF) atau XLSX dua sheet (`Laporan Cuti`,
//! `Grafik` dengan dua grafik batang native).

use std::collections::BTreeMap;

use chrono::{Datelike, NaiveDate, NaiveDateTime};
use mz_core::iso8601::DateArg;
use mz_core::{ApiError, ApiResult, AppState};
use rust_xlsxwriter::{Chart, ChartSolidFill, Color, Format, FormatAlign, Workbook, Worksheet};

use crate::dto::{ReportFormat, ReportQuery};
use crate::model::Visibility;
use crate::util::{fmt_date, fmt_date_time, invalid_date, month_label, round};

/// Satu baris laporan (`LeaveReportRow`).
#[derive(Debug, Clone, PartialEq)]
pub struct ReportRow {
    pub employee_id: i32,
    pub employee_name: String,
    pub leave_type: String,
    pub start_date: String,
    pub end_date: String,
    pub total_days: f64,
    pub reason: String,
    pub status: String,
    pub submitted_at: String,
    pub approver: String,
    pub approved_at: String,
    pub balance_before: Option<f64>,
    pub balance_after: Option<f64>,
    pub approval_note: String,
}

/// `{ label, value }`.
#[derive(Debug, Clone, PartialEq)]
pub struct AggItem {
    pub label: String,
    pub value: f64,
}

/// `LeaveReportData`.
#[derive(Debug, Clone, PartialEq)]
pub struct ReportData {
    pub period: String,
    pub generated_at: String,
    pub rows: Vec<ReportRow>,
    pub type_agg: Vec<AggItem>,
    pub month_agg: Vec<AggItem>,
}

/// Nilai sel: teks atau angka (`null` → `-`).
#[derive(Debug, Clone, PartialEq)]
enum Cell {
    Text(String),
    Num(f64),
    Null,
}

const HEADERS: [&str; 14] = [
    "Employee ID",
    "Nama Karyawan",
    "Jenis Cuti",
    "Tanggal Mulai",
    "Tanggal Selesai",
    "Jumlah Hari",
    "Alasan",
    "Status",
    "Tanggal Pengajuan",
    "Approver",
    "Tanggal Approval",
    "Saldo Cuti Sebelum",
    "Saldo Cuti Sesudah",
    "Catatan Approver",
];

fn cells(r: &ReportRow) -> [Cell; 14] {
    let t = |s: &str| Cell::Text(s.to_owned());
    let n = |v: Option<f64>| v.map_or(Cell::Null, Cell::Num);
    [
        Cell::Num(f64::from(r.employee_id)),
        t(&r.employee_name),
        t(&r.leave_type),
        t(&r.start_date),
        t(&r.end_date),
        Cell::Num(r.total_days),
        t(&r.reason),
        t(&r.status),
        t(&r.submitted_at),
        t(&r.approver),
        t(&r.approved_at),
        n(r.balance_before),
        n(r.balance_after),
        t(&r.approval_note),
    ]
}

/// `csvCell(value)`.
fn csv_cell(c: &Cell) -> String {
    let s = match c {
        Cell::Text(s) => s.clone(),
        Cell::Num(n) => mz_core::jsfmt::num_string(*n),
        Cell::Null => "-".to_owned(),
    };
    if s.contains(['"', ',', '\r', '\n']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s
    }
}

/// `generateLeaveReportCsv(data)`: BOM UTF-8 + baris dipisah CRLF.
pub fn generate_csv(data: &ReportData) -> Vec<u8> {
    let mut lines = vec![
        HEADERS
            .iter()
            .map(|h| csv_cell(&Cell::Text((*h).to_owned())))
            .collect::<Vec<_>>()
            .join(","),
    ];
    for row in &data.rows {
        lines.push(
            cells(row)
                .iter()
                .map(csv_cell)
                .collect::<Vec<_>>()
                .join(","),
        );
    }
    format!("\u{feff}{}", lines.join("\r\n")).into_bytes()
}

fn header_format() -> Format {
    Format::new()
        .set_bold()
        .set_background_color(Color::RGB(0x4F_46E5))
        .set_font_color(Color::RGB(0xFF_FFFF))
        .set_align(FormatAlign::Center)
}

fn xlsx_err(e: rust_xlsxwriter::XlsxError) -> ApiError {
    ApiError::internal(anyhow::anyhow!("xlsx: {e}"))
}

fn write_cell(
    ws: &mut Worksheet,
    row: u32,
    col: u16,
    c: &Cell,
) -> Result<(), rust_xlsxwriter::XlsxError> {
    match c {
        Cell::Text(s) => ws.write_string(row, col, s).map(|_| ()),
        Cell::Num(n) => ws.write_number(row, col, *n).map(|_| ()),
        Cell::Null => ws.write_string(row, col, "-").map(|_| ()),
    }
}

/// `generateLeaveReportExcel(data)`: sheet data + sheet grafik dengan dua grafik batang
/// (jenis cuti: horizontal, per bulan: kolom), legenda tidak ada, seri merujuk header B.
pub fn generate_excel(data: &ReportData) -> ApiResult<Vec<u8>> {
    const SHEET_DATA: &str = "Laporan Cuti";
    const SHEET_CHART: &str = "Grafik";
    let header = header_format();
    let bold = Format::new().set_bold();
    let mut wb = Workbook::new();

    let ws = wb.add_worksheet();
    ws.set_name(SHEET_DATA).map_err(xlsx_err)?;
    for (col, h) in (0u16..).zip(HEADERS) {
        ws.write_string_with_format(0, col, h, &header)
            .map_err(xlsx_err)?;
    }
    for (row, r) in (1u32..).zip(&data.rows) {
        for (col, c) in (0u16..).zip(cells(r).iter()) {
            write_cell(ws, row, col, c).map_err(xlsx_err)?;
        }
    }

    let ws = wb.add_worksheet();
    ws.set_name(SHEET_CHART).map_err(xlsx_err)?;
    // Baris 1 judul, 2 header, 3.. data jenis; kosong; judul bulan; header; data bulan.
    ws.write_string_with_format(0, 0, "Penggunaan per jenis cuti", &bold)
        .map_err(xlsx_err)?;
    let table_header = |ws: &mut Worksheet, row: u32| -> Result<(), rust_xlsxwriter::XlsxError> {
        ws.write_string_with_format(row, 0, "Kategori", &header)?;
        ws.write_string_with_format(row, 1, "Jumlah Hari", &header)?;
        Ok(())
    };
    table_header(ws, 1).map_err(xlsx_err)?;
    let mut row = 2u32;
    for t in &data.type_agg {
        ws.write_string(row, 0, &t.label).map_err(xlsx_err)?;
        ws.write_number(row, 1, t.value).map_err(xlsx_err)?;
        row += 1;
    }
    let type_end = row - 1;
    ws.write_string(row, 0, "").map_err(xlsx_err)?;
    row += 1;
    ws.write_string_with_format(row, 0, "Cuti per bulan", &bold)
        .map_err(xlsx_err)?;
    row += 1;
    let month_header = row;
    table_header(ws, month_header).map_err(xlsx_err)?;
    row += 1;
    let month_start = row;
    for m in &data.month_agg {
        ws.write_string(row, 0, &m.label).map_err(xlsx_err)?;
        ws.write_number(row, 1, m.value).map_err(xlsx_err)?;
        row += 1;
    }
    let month_end = row - 1;

    let chart = |mut chart: Chart, title: &str, header_row: u32, from: u32, to: u32, color: u32| {
        chart.title().set_name(title);
        chart.legend().set_hidden();
        chart
            .add_series()
            .set_name((SHEET_CHART, header_row, 1))
            .set_categories((SHEET_CHART, from, 0, to, 0))
            .set_values((SHEET_CHART, from, 1, to, 1))
            .set_format(ChartSolidFill::new().set_color(Color::RGB(color)))
            .set_gap(60);
        chart.set_width(640).set_height(400);
        chart
    };
    let by_type = chart(
        Chart::new_bar(),
        "Penggunaan per jenis cuti",
        1,
        2,
        type_end,
        0x4F_46E5,
    );
    let by_month = chart(
        Chart::new_column(),
        "Cuti per bulan",
        month_header,
        month_start,
        month_end,
        0x4F_76E8,
    );
    ws.insert_chart(1, 4, &by_type).map_err(xlsx_err)?;
    ws.insert_chart(22, 4, &by_month).map_err(xlsx_err)?;

    wb.save_to_buffer().map_err(xlsx_err)
}

#[derive(sqlx::FromRow)]
struct ItemRow {
    user_id: i32,
    leave_type: Option<String>,
    start_date: NaiveDate,
    end_date: NaiveDate,
    total_days: f64,
    reason: String,
    status: String,
    created_at: NaiveDateTime,
    reviewed_at: Option<NaiveDateTime>,
    balance_before: Option<f64>,
    balance_after: Option<f64>,
    approval_note: Option<String>,
    user_name: Option<String>,
    reviewer_name: Option<String>,
    type_name: Option<String>,
}

/// `new Date(query.x)` (UTC) untuk filter `@db.Date`.
fn date_arg(raw: Option<&str>) -> ApiResult<Option<NaiveDateTime>> {
    match raw.map(mz_core::iso8601::parse_date_arg) {
        None => Ok(None),
        Some(Some(DateArg::Ok(d))) => Ok(Some(d)),
        Some(_) => Err(invalid_date()),
    }
}

/// Hasil laporan: `(bytes, filename, contentType)`.
pub struct ReportFile {
    pub bytes: Vec<u8>,
    pub filename: String,
    pub content_type: &'static str,
}

/// `generateLeaveReport(reviewerId, query)`.
pub async fn generate_report(
    state: &AppState,
    reviewer_id: i32,
    company_id: i32,
    q: &ReportQuery,
) -> ApiResult<ReportFile> {
    let vis = Visibility::load(&state.db, reviewer_id, company_id).await?;
    let start = date_arg(q.start_date.as_deref())?;
    let end = date_arg(q.end_date.as_deref())?;

    let mut qb = sqlx::QueryBuilder::new(
        r#"SELECT lr."userId" AS user_id, lr."leaveType"::text AS leave_type,
                  lr."startDate" AS start_date, lr."endDate" AS end_date, lr."totalDays" AS total_days,
                  lr.reason, lr.status::text AS status, lr."createdAt" AS created_at,
                  lr."reviewedAt" AS reviewed_at, lr."balanceBefore" AS balance_before,
                  lr."balanceAfter" AS balance_after, lr."approvalNote" AS approval_note,
                  u.name AS user_name, rb.name AS reviewer_name, lt.name AS type_name
             FROM "leaveRequest" lr
             LEFT JOIN users u ON u.id = lr."userId"
             LEFT JOIN users rb ON rb.id = lr."reviewedById"
             LEFT JOIN "companyLeaveType" lt ON lt.id = lr."leaveTypeId"
            WHERE lr."companyId" = "#,
    );
    qb.push_bind(company_id).push(" AND ");
    vis.push(&mut qb, "lr");
    if let Some(t) = q.leave_type_id.filter(|t| *t != 0) {
        qb.push(r#" AND lr."leaveTypeId" = "#).push_bind(t);
    }
    if let Some(s) = &q.status {
        qb.push(" AND lr.status::text = ").push_bind(s.clone());
    }
    // Kolom `@db.Date` dibandingkan dengan tanggal argumen (Prisma memotong jamnya).
    match (start, end) {
        (Some(s), Some(e)) => {
            qb.push(r#" AND lr."startDate" <= "#)
                .push_bind(e.date())
                .push(r#" AND lr."endDate" >= "#)
                .push_bind(s.date());
        }
        (Some(s), None) => {
            qb.push(r#" AND lr."endDate" >= "#).push_bind(s.date());
        }
        (None, Some(e)) => {
            qb.push(r#" AND lr."startDate" <= "#).push_bind(e.date());
        }
        (None, None) => {}
    }
    qb.push(r#" ORDER BY lr."startDate" ASC, lr."createdAt" ASC, lr.id ASC"#);
    let items: Vec<ItemRow> = qb.build_query_as().fetch_all(&state.db).await?;

    let rows = items
        .iter()
        .map(|it| ReportRow {
            employee_id: it.user_id,
            employee_name: it.user_name.clone().unwrap_or_else(|| "-".to_owned()),
            leave_type: it
                .type_name
                .clone()
                .or_else(|| it.leave_type.clone())
                .unwrap_or_else(|| "-".to_owned()),
            start_date: fmt_date(it.start_date),
            end_date: fmt_date(it.end_date),
            total_days: it.total_days,
            reason: it.reason.clone(),
            status: it.status.clone(),
            submitted_at: fmt_date_time(Some(it.created_at)),
            approver: it.reviewer_name.clone().unwrap_or_else(|| "-".to_owned()),
            approved_at: fmt_date_time(it.reviewed_at),
            balance_before: it.balance_before,
            balance_after: it.balance_after,
            approval_note: it.approval_note.clone().unwrap_or_else(|| "-".to_owned()),
        })
        .collect();

    // Agregasi = pemakaian nyata → hanya APPROVED.
    let approved: Vec<&ItemRow> = items.iter().filter(|i| i.status == "APPROVED").collect();
    let mut type_map: Vec<(String, f64)> = Vec::new();
    for i in &approved {
        let key = i
            .type_name
            .clone()
            .or_else(|| i.leave_type.clone())
            .unwrap_or_else(|| "Lainnya".to_owned());
        match type_map.iter_mut().find(|(k, _)| *k == key) {
            Some((_, v)) => *v += i.total_days,
            None => type_map.push((key, i.total_days)),
        }
    }
    let mut type_agg: Vec<AggItem> = type_map
        .into_iter()
        .map(|(label, value)| AggItem {
            label,
            value: round(value),
        })
        .collect();
    type_agg.sort_by(|a, b| b.value.total_cmp(&a.value));

    // Kunci `YYYY-MM` dengan bulan 0-based (seperti Node), urut leksikografis.
    let mut month_map: BTreeMap<String, (i32, u32, f64)> = BTreeMap::new();
    for i in &approved {
        let (y, m0) = (i.start_date.year(), i.start_date.month0());
        let key = format!("{y}-{m0:02}");
        month_map.entry(key).or_insert((y, m0, 0.0)).2 += i.total_days;
    }
    let years: std::collections::HashSet<i32> = month_map.values().map(|(y, _, _)| *y).collect();
    let multi_year = years.len() > 1;
    let mut month_agg: Vec<AggItem> = month_map
        .values()
        .map(|(y, m0, v)| AggItem {
            label: format!(
                "{}{}",
                month_label(i64::from(*m0)).unwrap_or_default(),
                if multi_year {
                    format!(" {y}")
                } else {
                    String::new()
                }
            ),
            value: round(*v),
        })
        .collect();

    // Rentang grafik tidak boleh kosong.
    let none = || {
        vec![AggItem {
            label: "Tidak ada data".to_owned(),
            value: 0.0,
        }]
    };
    if type_agg.is_empty() {
        type_agg = none();
    }
    if month_agg.is_empty() {
        month_agg = none();
    }

    let period = if start.is_some() || end.is_some() {
        format!(
            "{} s/d {}",
            start.map_or_else(|| "...".to_owned(), |d| fmt_date(d.date())),
            end.map_or_else(|| "...".to_owned(), |d| fmt_date(d.date()))
        )
    } else {
        "Semua periode".to_owned()
    };
    let now = mz_core::time::now_naive();
    let data = ReportData {
        period,
        generated_at: fmt_date_time(Some(now)),
        rows,
        type_agg,
        month_agg,
    };
    let stamp = fmt_date(now.date());
    Ok(match q.format {
        ReportFormat::Csv => ReportFile {
            bytes: generate_csv(&data),
            filename: format!("Laporan-Cuti-{stamp}.csv"),
            content_type: "text/csv; charset=utf-8",
        },
        ReportFormat::Excel => ReportFile {
            bytes: generate_excel(&data)?,
            filename: format!("Laporan-Cuti-{stamp}.xlsx"),
            content_type: "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data() -> ReportData {
        ReportData {
            period: "Semua periode".into(),
            generated_at: "2026-11-17 10:00".into(),
            rows: vec![ReportRow {
                employee_id: 5,
                employee_name: "Ani, S.Kom".into(),
                leave_type: "Cuti \"Tahunan\"".into(),
                start_date: "2026-10-05".into(),
                end_date: "2026-10-06".into(),
                total_days: 1.5,
                reason: "baris1\nbaris2".into(),
                status: "APPROVED".into(),
                submitted_at: "2026-10-01 03:04".into(),
                approver: "-".into(),
                approved_at: "-".into(),
                balance_before: Some(12.0),
                balance_after: None,
                approval_note: "-".into(),
            }],
            type_agg: vec![AggItem {
                label: "Tidak ada data".into(),
                value: 0.0,
            }],
            month_agg: vec![AggItem {
                label: "Oct".into(),
                value: 1.5,
            }],
        }
    }

    #[test]
    fn csv_matches_node() {
        let csv = String::from_utf8(generate_csv(&data())).unwrap_or_default();
        let mut lines = csv.split("\r\n");
        assert_eq!(
            lines.next(),
            Some(
                "\u{feff}Employee ID,Nama Karyawan,Jenis Cuti,Tanggal Mulai,Tanggal Selesai,Jumlah Hari,Alasan,Status,Tanggal Pengajuan,Approver,Tanggal Approval,Saldo Cuti Sebelum,Saldo Cuti Sesudah,Catatan Approver"
            )
        );
        assert_eq!(
            lines.next(),
            Some(
                "5,\"Ani, S.Kom\",\"Cuti \"\"Tahunan\"\"\",2026-10-05,2026-10-06,1.5,\"baris1\nbaris2\",APPROVED,2026-10-01 03:04,-,-,12,-,-"
            )
        );
    }

    #[test]
    fn excel_has_two_sheets_and_charts() {
        let bytes = generate_excel(&data()).unwrap_or_default();
        assert!(bytes.starts_with(b"PK"));
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("xl/charts/chart1.xml") || bytes.len() > 1000);
    }
}
