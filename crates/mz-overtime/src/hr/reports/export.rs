//! Ekspor laporan lembur ke XLSX. Node: `src/overtime/overtime-report.export.ts`
//! (`write-excel-file`; di sini `rust_xlsxwriter`). Fungsi murni: hanya membaca argumen,
//! sehingga baris berkas identik dengan keluaran service untuk layar.

use chrono::{Datelike, Utc};
use rust_xlsxwriter::{ExcelDateTime, Format, Workbook, Worksheet, XlsxError};

use super::entity::{OvertimeReport, ReportMeta, WeeklyExcessReport};
use crate::zone::{find_zone, parse_date_key};

pub const SHEET_DAILY: &str = "Rincian lembur";
pub const SHEET_WEEKLY: &str = "Melebihi jam kerja mingguan";

/// Label status pengajuan untuk ekspor (xlsx & pdf); status tak dikenal dikembalikan apa adanya.
pub fn status_label(status: &str) -> &str {
    match status {
        "PENDING_APPROVAL" => "Menunggu persetujuan",
        "PENDING_EMPLOYEE" => "Menunggu karyawan",
        "APPROVED" => "Disetujui",
        "REJECTED" => "Ditolak",
        "CANCELLED" => "Dibatalkan",
        "COMPLETED" => "Selesai",
        other => other,
    }
}

/// Label jenis hari; jenis tak dikenal dikembalikan apa adanya.
pub fn day_type_label(day_type: &str) -> &str {
    match day_type {
        "WORKDAY" => "Hari kerja",
        "HOLIDAY" => "Hari libur",
        other => other,
    }
}

fn day_type_text(day_type: Option<&str>) -> &str {
    day_type.map_or("-", day_type_label)
}

/// Menit -> `H j M m` (`None` -> `-`).
pub fn format_duration(minutes: Option<i64>) -> String {
    match minutes {
        None => "-".to_owned(),
        Some(m) => {
            let total = m.max(0);
            format!("{} j {} m", total / 60, total % 60)
        }
    }
}

pub fn report_filename(from: &str, to: &str, ext: &str) -> String {
    format!("laporan-lembur-{from}_{to}.{ext}")
}

const MONTH_ABBR: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "Mei", "Jun", "Jul", "Agu", "Sep", "Okt", "Nov", "Des",
];

/// `2026-10-01` -> `1 Okt 2026`; masukan tak valid dikembalikan apa adanya.
pub fn format_date_id(key: &str) -> String {
    let b = key.as_bytes();
    let shape_ok = b.len() == 10
        && b.iter().enumerate().all(|(i, c)| match i {
            4 | 7 => *c == b'-',
            _ => c.is_ascii_digit(),
        });
    if !shape_ok {
        return key.to_owned();
    }
    let parts = (key.get(0..4), key.get(5..7), key.get(8..10));
    let (Some(y), Some(m), Some(d)) = parts else {
        return key.to_owned();
    };
    let month = m
        .parse::<usize>()
        .ok()
        .and_then(|n| n.checked_sub(1))
        .and_then(|i| MONTH_ABBR.get(i));
    match (month, d.parse::<u32>()) {
        (Some(month), Ok(day)) => format!("{day} {month} {y}"),
        _ => key.to_owned(),
    }
}

/// Waktu cetak di zona company; zona tak dikenal jatuh ke UTC.
pub fn format_print_time(at: chrono::DateTime<Utc>, time_zone: &str) -> String {
    match find_zone(time_zone) {
        Some(tz) => format!(
            "{} ({time_zone})",
            at.with_timezone(&tz).format("%d/%m/%Y %H:%M")
        ),
        None => format!("{} (UTC)", at.format("%d/%m/%Y %H:%M")),
    }
}

/// Satu sel lembar. `Empty` = sel kosong (bukan 0, supaya SUM/AVERAGE tidak keliru).
#[derive(Debug, Clone, PartialEq)]
enum Cell {
    Empty,
    Text {
        value: String,
        bold: bool,
    },
    Number {
        value: f64,
        bold: bool,
        format: Option<&'static str>,
    },
    Date {
        key: String,
        bold: bool,
    },
}

const DATE_FORMAT: &str = "dd/mm/yyyy";
const DECIMAL_FORMAT: &str = "0.00";

fn text(value: impl Into<String>, bold: bool) -> Cell {
    Cell::Text {
        value: value.into(),
        bold,
    }
}

#[allow(clippy::cast_precision_loss)]
fn num(value: Option<i64>, bold: bool, format: Option<&'static str>) -> Cell {
    match value {
        None => Cell::Empty,
        Some(v) => Cell::Number {
            value: v as f64,
            bold,
            format,
        },
    }
}

/// Menit -> jam desimal (`None` tetap `None`).
#[allow(clippy::cast_precision_loss)]
fn hours_cell(minutes: Option<i64>, bold: bool) -> Cell {
    match minutes {
        None => Cell::Empty,
        Some(m) => Cell::Number {
            value: m as f64 / 60.0,
            bold,
            format: Some(DECIMAL_FORMAT),
        },
    }
}

/// Tanggal kalender `YYYY-MM-DD` -> sel tanggal (tanpa pergeseran zona); tak valid -> teks.
fn date_cell(key: &str, bold: bool) -> Cell {
    if parse_date_key(key).is_some() {
        Cell::Date {
            key: key.to_owned(),
            bold,
        }
    } else {
        text(key, bold)
    }
}

fn text_row(values: &[&str], bold: bool) -> Vec<Cell> {
    values.iter().map(|v| text(*v, bold)).collect()
}

/// Baris judul di atas setiap lembar: perusahaan, periode, waktu cetak, lalu satu baris kosong.
fn header_rows(from: &str, to: &str, meta: &ReportMeta) -> Vec<Vec<Cell>> {
    vec![
        vec![text(format!("Perusahaan: {}", meta.company_name), true)],
        vec![text(
            format!(
                "Periode: {} \u{2013} {}",
                format_date_id(from),
                format_date_id(to)
            ),
            false,
        )],
        vec![text(
            format!(
                "Dicetak: {}",
                format_print_time(meta.generated_at, &meta.time_zone)
            ),
            false,
        )],
        Vec::new(),
    ]
}

/// "Melebihi batas": harian = disetujui di atas batas harian; mingguan = lewat 18 jam/minggu.
fn exceeds_text(exceeds_limit: bool, exceeds_daily_limit: bool) -> &'static str {
    if exceeds_daily_limit {
        "Ya (disetujui)"
    } else if exceeds_limit {
        "Ya (mingguan)"
    } else {
        "Tidak"
    }
}

fn build_daily_sheet(daily: &OvertimeReport, meta: &ReportMeta) -> Vec<Vec<Cell>> {
    let mut sheet = header_rows(&daily.period.from, &daily.period.to, meta);
    sheet.push(text_row(
        &[
            "Karyawan",
            "Tanggal",
            "Jenis hari",
            "Durasi diajukan",
            "Tercatat",
            "Lembur",
            "Status",
            "Melebihi batas",
            "Final",
            "Durasi (menit)",
            "Tercatat (menit)",
            "Lembur (menit)",
            "Lembur (jam desimal)",
        ],
        true,
    ));
    for emp in &daily.employees {
        for d in &emp.days {
            sheet.push(vec![
                text(&emp.employee.name, false),
                date_cell(&d.date, false),
                text(day_type_text(d.day_type.as_deref()), false),
                text(format_duration(Some(d.approved_minutes)), false),
                text(format_duration(d.worked_minutes), false),
                text(format_duration(d.overtime_minutes), false),
                text(status_label(&d.status), false),
                text(exceeds_text(d.exceeds_limit, d.exceeds_daily_limit), false),
                text(if d.is_final { "Ya" } else { "Belum final" }, false),
                num(Some(d.approved_minutes), false, None),
                num(d.worked_minutes, false, None),
                num(d.overtime_minutes, false, None),
                hours_cell(d.overtime_minutes, false),
            ]);
        }
    }
    sheet.push(Vec::new());
    for emp in &daily.employees {
        sheet.push(vec![
            text(format!("Total {}", emp.employee.name), true),
            text("", true),
            text("", true),
            text("", true),
            text("", true),
            text(format_duration(Some(emp.total_overtime_minutes)), true),
            text("", true),
            text("", true),
            text(if emp.not_final { "Belum final" } else { "Ya" }, true),
            Cell::Empty,
            Cell::Empty,
            num(Some(emp.total_overtime_minutes), true, None),
            hours_cell(Some(emp.total_overtime_minutes), true),
        ]);
    }
    sheet.push(Vec::new());
    sheet.push(vec![text(&daily.legal_note, false)]);
    sheet
}

fn build_weekly_sheet(
    weekly: &WeeklyExcessReport,
    from: &str,
    to: &str,
    meta: &ReportMeta,
) -> Vec<Vec<Cell>> {
    let mut sheet = header_rows(from, to, meta);
    sheet.push(text_row(
        &[
            "Bulan",
            "Karyawan",
            "Awal minggu",
            "Akhir minggu",
            "Tercatat",
            "Normal",
            "Lembur",
            "Kelebihan",
            "Final",
            "Tercatat (menit)",
            "Normal (menit)",
            "Lembur (menit)",
            "Kelebihan (menit)",
        ],
        true,
    ));
    for m in &weekly.months {
        for emp in &m.employees {
            for w in &emp.weeks {
                sheet.push(vec![
                    text(&m.month, false),
                    text(&emp.employee.name, false),
                    date_cell(&w.week_start, false),
                    date_cell(&w.week_end, false),
                    text(format_duration(Some(w.worked_minutes)), false),
                    text(format_duration(Some(w.normal_minutes)), false),
                    text(format_duration(Some(w.overtime_minutes)), false),
                    text(format_duration(Some(w.excess_minutes)), false),
                    text(if w.not_final { "Belum final" } else { "Ya" }, false),
                    num(Some(w.worked_minutes), false, None),
                    num(Some(w.normal_minutes), false, None),
                    num(Some(w.overtime_minutes), false, None),
                    num(Some(w.excess_minutes), false, None),
                ]);
            }
            // Sama dengan PDF: total kelebihan sebulan per karyawan.
            sheet.push(vec![
                text(&m.month, true),
                text(format!("Total {}", emp.employee.name), true),
                text("", true),
                text("", true),
                text("", true),
                text("", true),
                text("", true),
                text(format_duration(Some(emp.total_excess_minutes)), true),
                text("", true),
                Cell::Empty,
                Cell::Empty,
                Cell::Empty,
                num(Some(emp.total_excess_minutes), true, None),
            ]);
        }
    }
    sheet
}

/// Format sel; dibuat per sel (murah) supaya kombinasi tebal/format tidak perlu cache.
fn format_of(bold: bool, num_format: Option<&str>) -> Format {
    let mut f = Format::new();
    if bold {
        f = f.set_bold();
    }
    if let Some(nf) = num_format {
        f = f.set_num_format(nf);
    }
    f
}

fn write_sheet(ws: &mut Worksheet, rows: &[Vec<Cell>]) -> Result<(), XlsxError> {
    for (r, row) in rows.iter().enumerate() {
        let r = u32::try_from(r).unwrap_or(u32::MAX);
        for (c, cell) in row.iter().enumerate() {
            let c = u16::try_from(c).unwrap_or(u16::MAX);
            match cell {
                Cell::Empty => {}
                Cell::Text { value, bold } => {
                    let value: String = value.chars().take(32_767).collect();
                    if *bold {
                        ws.write_string_with_format(r, c, value, &format_of(true, None))?;
                    } else {
                        ws.write_string(r, c, value)?;
                    }
                }
                Cell::Number {
                    value,
                    bold,
                    format,
                } => {
                    if *bold || format.is_some() {
                        ws.write_number_with_format(r, c, *value, &format_of(*bold, *format))?;
                    } else {
                        ws.write_number(r, c, *value)?;
                    }
                }
                Cell::Date { key, bold } => {
                    let Some(d) = parse_date_key(key) else {
                        continue;
                    };
                    let year = u16::try_from(d.year()).unwrap_or(1900);
                    let month = u8::try_from(d.month()).unwrap_or(1);
                    let day = u8::try_from(d.day()).unwrap_or(1);
                    let dt = ExcelDateTime::from_ymd(year, month, day)?;
                    ws.write_datetime_with_format(r, c, &dt, &format_of(*bold, Some(DATE_FORMAT)))?;
                }
            }
        }
    }
    Ok(())
}

/// `toXlsx`: dua lembar (`Rincian lembur`, `Melebihi jam kerja mingguan`).
pub fn to_xlsx(
    daily: &OvertimeReport,
    weekly: &WeeklyExcessReport,
    meta: &ReportMeta,
) -> Result<Vec<u8>, XlsxError> {
    let mut wb = Workbook::new();
    let daily_rows = build_daily_sheet(daily, meta);
    write_sheet(wb.add_worksheet().set_name(SHEET_DAILY)?, &daily_rows)?;
    let weekly_rows = build_weekly_sheet(weekly, &daily.period.from, &daily.period.to, meta);
    write_sheet(wb.add_worksheet().set_name(SHEET_WEEKLY)?, &weekly_rows)?;
    wb.save_to_buffer()
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::Read;

    use chrono::TimeZone;

    use super::super::fixtures::{daily_of, meta, weekly_of, xlsx_daily, xlsx_weekly};
    use super::*;

    #[derive(Debug, Clone)]
    struct XCell {
        /// `s` (string bersama) atau `n` (angka).
        t: char,
        v: String,
        style: usize,
    }

    type Sheet = BTreeMap<(u32, u32), XCell>;

    fn entry(bytes: &[u8], name: &str) -> String {
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes.to_vec())).unwrap();
        let mut out = String::new();
        zip.by_name(name).unwrap().read_to_string(&mut out).unwrap();
        out
    }

    fn unescape(s: &str) -> String {
        s.replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
            .replace("&amp;", "&")
    }

    fn attr(tag: &str, name: &str) -> Option<String> {
        let key = format!(" {name}=\"");
        let start = tag.find(&key)? + key.len();
        let end = tag[start..].find('"')? + start;
        Some(tag[start..end].to_owned())
    }

    fn shared_strings(xml: &str) -> Vec<String> {
        xml.split("<si>")
            .skip(1)
            .map(|si| {
                let mut text = String::new();
                let mut rest = si;
                while let Some(open) = rest.find("<t") {
                    let after = &rest[open..];
                    let gt = after.find('>').unwrap();
                    let close = after.find("</t>").unwrap();
                    text.push_str(&unescape(&after[gt + 1..close]));
                    rest = &after[close + 4..];
                }
                text
            })
            .collect()
    }

    /// `B5` -> (baris 4, kolom 1), nol-basis.
    fn parse_ref(r: &str) -> (u32, u32) {
        let letters: String = r.chars().take_while(char::is_ascii_alphabetic).collect();
        let row: u32 = r[letters.len()..].parse().unwrap();
        let col = letters
            .bytes()
            .fold(0u32, |a, b| a * 26 + u32::from(b - b'A' + 1));
        (row - 1, col - 1)
    }

    fn read_sheet(bytes: &[u8], n: usize) -> (Sheet, Vec<String>) {
        let sst = shared_strings(&entry(bytes, "xl/sharedStrings.xml"));
        let xml = entry(bytes, &format!("xl/worksheets/sheet{n}.xml"));
        let mut sheet = Sheet::new();
        for part in xml.split("<c ").skip(1) {
            let tag_end = part.find('>').unwrap();
            let tag = format!(" {}", &part[..tag_end]);
            let tag = tag.as_str();
            if tag.ends_with('/') {
                continue; // sel kosong berformat
            }
            let r = parse_ref(&attr(tag, "r").unwrap());
            let t = if attr(tag, "t").as_deref() == Some("s") {
                's'
            } else {
                'n'
            };
            let v_start = part.find("<v>").unwrap() + 3;
            let v_end = part.find("</v>").unwrap();
            let raw = &part[v_start..v_end];
            let style = attr(tag, "s").and_then(|s| s.parse().ok()).unwrap_or(0);
            let v = if t == 's' {
                sst[raw.parse::<usize>().unwrap()].clone()
            } else {
                raw.to_owned()
            };
            sheet.insert(r, XCell { t, v, style });
        }
        (sheet, sst)
    }

    /// Kode format angka sel (`numFmtId` 2 = `0.00` bawaan).
    fn num_format(bytes: &[u8], style: usize) -> String {
        let xml = entry(bytes, "xl/styles.xml");
        let xfs_at = xml.find("<cellXfs").unwrap();
        let xf = xml[xfs_at..]
            .split("<xf ")
            .skip(1)
            .nth(style)
            .unwrap()
            .to_owned();
        let id = attr(&format!(" {xf}"), "numFmtId").unwrap();
        if id == "2" {
            return "0.00".into();
        }
        for part in xml.split("<numFmt ").skip(1) {
            let part = format!(" {part}");
            if attr(&part, "numFmtId").as_deref() == Some(&id) {
                return unescape(&attr(&part, "formatCode").unwrap());
            }
        }
        format!("#{id}")
    }

    fn text_at(sheet: &Sheet, r: u32, c: u32) -> Option<&str> {
        sheet.get(&(r, c)).map(|x| x.v.as_str())
    }

    #[test]
    fn duration_format() {
        for (m, expected) in [
            (Some(0), "0 j 0 m"),
            (Some(59), "0 j 59 m"),
            (Some(60), "1 j 0 m"),
            (Some(90), "1 j 30 m"),
            (Some(270), "4 j 30 m"),
            (None, "-"),
        ] {
            assert_eq!(format_duration(m), expected);
        }
    }

    #[test]
    fn filename() {
        assert_eq!(
            report_filename("2026-09-26", "2026-10-25", "xlsx"),
            "laporan-lembur-2026-09-26_2026-10-25.xlsx"
        );
        assert_eq!(
            report_filename("2026-09-26", "2026-10-25", "pdf"),
            "laporan-lembur-2026-09-26_2026-10-25.pdf"
        );
    }

    #[test]
    fn indonesian_date_and_print_time() {
        assert_eq!(format_date_id("2026-10-01"), "1 Okt 2026");
        assert_eq!(format_date_id("2026-08-31"), "31 Agu 2026");
        assert_eq!(format_date_id("2026-05-09"), "9 Mei 2026");
        assert_eq!(format_date_id("bukan-tanggal"), "bukan-tanggal");
        let at = chrono::Utc
            .with_ymd_and_hms(2026, 10, 25, 20, 30, 0)
            .single()
            .unwrap();
        assert_eq!(
            format_print_time(at, "Asia/Jakarta"),
            "26/10/2026 03:30 (Asia/Jakarta)"
        );
        assert_eq!(
            format_print_time(at, "Pacific/Auckland"),
            "26/10/2026 09:30 (Pacific/Auckland)"
        );
        assert_eq!(
            format_print_time(at, "Nowhere/Land"),
            "25/10/2026 20:30 (UTC)"
        );
    }

    #[test]
    fn two_sheets_with_company_period_and_print_header() {
        let bytes = to_xlsx(&xlsx_daily(), &xlsx_weekly(), &meta()).unwrap();
        let wb = entry(&bytes, "xl/workbook.xml");
        let first = wb.find("Rincian lembur").unwrap();
        let second = wb.find("Melebihi jam kerja mingguan").unwrap();
        assert!(first < second);
        for n in [1, 2] {
            let (sheet, _) = read_sheet(&bytes, n);
            assert_eq!(text_at(&sheet, 0, 0), Some("Perusahaan: PT Maju Jaya"));
            assert_eq!(
                text_at(&sheet, 1, 0),
                Some("Periode: 26 Sep 2026 \u{2013} 25 Okt 2026")
            );
            assert_eq!(
                text_at(&sheet, 2, 0),
                Some("Dicetak: 26/10/2026 03:30 (Asia/Jakarta)")
            );
        }
    }

    #[test]
    fn daily_sheet_columns_types_and_blank_cells() {
        let bytes = to_xlsx(&xlsx_daily(), &xlsx_weekly(), &meta()).unwrap();
        let (s, _) = read_sheet(&bytes, 1);
        let header: Vec<&str> = (0..13).filter_map(|c| text_at(&s, 4, c)).collect();
        assert_eq!(
            header,
            [
                "Karyawan",
                "Tanggal",
                "Jenis hari",
                "Durasi diajukan",
                "Tercatat",
                "Lembur",
                "Status",
                "Melebihi batas",
                "Final",
                "Durasi (menit)",
                "Tercatat (menit)",
                "Lembur (menit)",
                "Lembur (jam desimal)"
            ]
        );
        let names: Vec<&str> = (5..9).filter_map(|r| text_at(&s, r, 0)).collect();
        assert_eq!(
            names,
            ["Ani Wijaya", "Ani Wijaya", "Ani Wijaya", "Budi Santoso"]
        );
        // Tanggal: sel angka berformat tanggal (serial Excel), bukan teks.
        let date = s.get(&(5, 1)).unwrap();
        assert_eq!(date.t, 'n');
        assert_eq!(date.v, "46296"); // 2026-10-01
        assert_eq!(num_format(&bytes, date.style), "dd/mm/yyyy");
        let strs: Vec<&str> = (2..9).filter_map(|c| text_at(&s, 5, c)).collect();
        assert_eq!(
            strs,
            [
                "Hari kerja",
                "2 j 0 m",
                "1 j 30 m",
                "1 j 30 m",
                "Selesai",
                "Tidak",
                "Ya"
            ]
        );
        let nums: Vec<(char, &str)> = (9..13)
            .map(|c| {
                let x = s.get(&(5, c)).unwrap();
                (x.t, x.v.as_str())
            })
            .collect();
        assert_eq!(nums, [('n', "120"), ('n', "90"), ('n', "90"), ('n', "1.5")]);
        assert_eq!(num_format(&bytes, s.get(&(5, 12)).unwrap().style), "0.00");
        // Baris tanpa tercatat/lembur: sel kosong (bukan 0), durasi diajukan tetap angka.
        assert_eq!(text_at(&s, 7, 4), Some("-"));
        assert_eq!(text_at(&s, 7, 5), Some("-"));
        assert_eq!(text_at(&s, 7, 9), Some("300"));
        for c in 10..13 {
            assert!(!s.contains_key(&(7, c)), "kolom {c} harus kosong");
        }
        assert_eq!(text_at(&s, 7, 7), Some("Ya (disetujui)"));
        assert_eq!(text_at(&s, 6, 8), Some("Belum final"));
    }

    #[test]
    fn totals_per_employee_and_legal_note() {
        let bytes = to_xlsx(&xlsx_daily(), &xlsx_weekly(), &meta()).unwrap();
        let (s, sst) = read_sheet(&bytes, 1);
        let total_rows: Vec<u32> = s
            .iter()
            .filter(|((_, c), x)| *c == 0 && x.v.starts_with("Total "))
            .map(|((r, _), _)| *r)
            .collect();
        assert_eq!(total_rows.len(), 2);
        let (ani, budi) = (total_rows[0], total_rows[1]);
        assert_eq!(text_at(&s, ani, 0), Some("Total Ani Wijaya"));
        assert_eq!(text_at(&s, budi, 0), Some("Total Budi Santoso"));
        assert_eq!(text_at(&s, ani, 5), Some("4 j 30 m"));
        assert_eq!(s.get(&(ani, 11)).unwrap().t, 'n');
        assert_eq!(text_at(&s, ani, 11), Some("270"));
        assert_eq!(text_at(&s, ani, 12), Some("4.5"));
        assert_eq!(text_at(&s, budi, 11), Some("0"));
        assert!(sst.iter().any(|v| v == super::super::pdf::LEGAL_NOTE));
        // Σ kolom "Lembur (menit)" untuk baris harian saja = 90 + 180 + 0.
        let day_minutes: f64 = s
            .iter()
            .filter(|((r, c), _)| *c == 11 && (5..9).contains(r))
            .map(|(_, x)| x.v.parse::<f64>().unwrap())
            .sum();
        assert!((day_minutes - 270.0).abs() < f64::EPSILON);
    }

    #[test]
    fn weekly_sheet_dates_numbers_and_monthly_total() {
        let bytes = to_xlsx(&xlsx_daily(), &xlsx_weekly(), &meta()).unwrap();
        let (s, _) = read_sheet(&bytes, 2);
        assert_eq!(text_at(&s, 4, 0), Some("Bulan"));
        let header: Vec<&str> = (9..13).filter_map(|c| text_at(&s, 4, c)).collect();
        assert_eq!(
            header,
            [
                "Tercatat (menit)",
                "Normal (menit)",
                "Lembur (menit)",
                "Kelebihan (menit)"
            ]
        );
        assert_eq!(text_at(&s, 5, 0), Some("2026-10"));
        assert_eq!(text_at(&s, 5, 1), Some("Ani Wijaya"));
        assert_eq!(text_at(&s, 5, 2), Some("46293")); // 2026-09-28
        assert_eq!(text_at(&s, 5, 3), Some("46299")); // 2026-10-04
        assert_eq!(
            num_format(&bytes, s.get(&(5, 2)).unwrap().style),
            "dd/mm/yyyy"
        );
        let strs: Vec<&str> = (4..9).filter_map(|c| text_at(&s, 5, c)).collect();
        assert_eq!(
            strs,
            ["50 j 0 m", "40 j 0 m", "4 j 0 m", "6 j 0 m", "Belum final"]
        );
        let nums: Vec<&str> = (9..13).filter_map(|c| text_at(&s, 5, c)).collect();
        assert_eq!(nums, ["3000", "2400", "240", "360"]);
        assert_eq!(text_at(&s, 6, 1), Some("Total Ani Wijaya"));
        assert_eq!(text_at(&s, 6, 7), Some("6 j 0 m"));
        assert_eq!(text_at(&s, 6, 12), Some("360"));
    }

    #[test]
    fn empty_data_still_yields_valid_file() {
        let daily = daily_of(Vec::new(), "2026-09-26", "2026-10-25");
        let bytes = to_xlsx(&daily, &weekly_of(Vec::new()), &meta()).unwrap();
        let (s, _) = read_sheet(&bytes, 1);
        assert_eq!(text_at(&s, 0, 0), Some("Perusahaan: PT Maju Jaya"));
        assert_eq!(text_at(&s, 4, 0), Some("Karyawan"));
    }
}
