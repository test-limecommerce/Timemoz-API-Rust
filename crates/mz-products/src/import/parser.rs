//! Port `products/product-import.parser.ts`: xlsx → header + baris teks. Murni (tanpa DB).
//! `exceljs` diganti `calamine` (lihat deviasi: pesan galat file rusak berbeda teks).

use std::collections::HashMap;
use std::io::Cursor;

use calamine::{Data, Reader, Xlsx};

use super::columns::{IMPORT_REQUIRED_COLUMNS, MAX_IMPORT_ROWS, legacy_uom_rate_columns};

/// `ImportParseError` — `code` masuk ke baris job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportParseError {
    pub code: &'static str,
    pub message: String,
}

impl std::fmt::Display for ImportParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ImportParseError {}

fn invalid(message: String) -> ImportParseError {
    ImportParseError {
        code: "IMPORT_INVALID_FILE",
        message,
    }
}

/// `ParsedImportRow`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedImportRow {
    /// Nomor baris spreadsheet (header = 1).
    pub row_number: usize,
    /// Teks sel, sejajar dengan indeks header; sel kosong = `""`.
    pub values: Vec<String>,
}

/// `ParsedImportWorkbook`. `headers` = (header, indeks kolom) urut kemunculan pertama.
#[derive(Debug, Clone, Default)]
pub struct ParsedImportWorkbook {
    pub headers: Vec<(String, usize)>,
    index: HashMap<String, usize>,
    pub rows: Vec<ParsedImportRow>,
}

impl ParsedImportWorkbook {
    /// `headerIndex.get(column)`.
    pub fn index_of(&self, column: &str) -> Option<usize> {
        self.index.get(column).copied()
    }

    pub fn has(&self, column: &str) -> bool {
        self.index.contains_key(column)
    }

    /// Dipakai uji row processor.
    pub fn from_headers(headers: &[&str]) -> Self {
        let mut wb = Self::default();
        for (i, h) in headers.iter().enumerate() {
            wb.push_header((*h).to_owned(), i);
        }
        wb
    }

    fn push_header(&mut self, header: String, col: usize) {
        if header.is_empty() || self.index.contains_key(&header) {
            return;
        }
        self.index.insert(header.clone(), col);
        self.headers.push((header, col));
    }
}

/// `cellToRaw(cell)`: tanggal → `YYYY-MM-DD`, bool → `true`/`false`, angka → `String(n)`,
/// teks di-trim.
fn cell_to_raw(cell: Option<&Data>) -> String {
    match cell {
        None | Some(Data::Empty) => String::new(),
        Some(Data::String(s)) => mz_core::jsfmt::trim(s).to_owned(),
        Some(Data::Float(f)) => mz_core::jsfmt::num_string(*f),
        Some(Data::Int(i)) => i.to_string(),
        Some(Data::Bool(b)) => if *b { "true" } else { "false" }.to_owned(),
        Some(Data::DateTime(dt)) => {
            let (y, m, d, ..) = dt.to_ymd_hms_milli();
            format!("{y:04}-{m:02}-{d:02}")
        }
        Some(Data::DateTimeIso(s)) => s.chars().take(10).collect(),
        Some(Data::DurationIso(s)) => mz_core::jsfmt::trim(s).to_owned(),
        Some(Data::Error(e)) => e.to_string(),
    }
}

/// `parseImportWorkbook(buffer, maxRows)`.
pub fn parse_import_workbook(
    buffer: &[u8],
    max_rows: usize,
) -> Result<ParsedImportWorkbook, ImportParseError> {
    let mut workbook: Xlsx<_> = Xlsx::new(Cursor::new(buffer))
        .map_err(|e| invalid(format!("File is not a readable xlsx workbook: {e}")))?;
    let range = match workbook.worksheet_range_at(0) {
        None => return Err(invalid("Workbook has no worksheet".into())),
        Some(Err(e)) => {
            return Err(invalid(format!(
                "File is not a readable xlsx workbook: {e}"
            )));
        }
        Some(Ok(r)) => r,
    };
    // Posisi absolut 0-based: header SELALU baris 1 spreadsheet.
    let (last_row, last_col) = range.end().unwrap_or((0, 0));
    let column_count = if range.is_empty() {
        0
    } else {
        last_col as usize + 1
    };
    let get = |row: u32, col: usize| {
        u32::try_from(col)
            .ok()
            .and_then(|c| range.get_value((row, c)))
    };
    if column_count == 0 {
        return Err(invalid("Workbook has no header row".into()));
    }
    let mut wb = ParsedImportWorkbook::default();
    for col in 0..column_count {
        wb.push_header(cell_to_raw(get(0, col)), col);
    }
    let missing: Vec<&str> = IMPORT_REQUIRED_COLUMNS
        .iter()
        .copied()
        .filter(|c| !wb.has(c))
        .collect();
    if !missing.is_empty() {
        return Err(invalid(format!(
            "Missing required column header(s): {}",
            missing.join(", ")
        )));
    }
    let legacy: Vec<String> = legacy_uom_rate_columns()
        .into_iter()
        .filter(|c| wb.has(c))
        .collect();
    if !legacy.is_empty() {
        return Err(invalid(format!(
            "Column(s) {} are no longer supported — download a new template and use uomNOperator/uomNFactor",
            legacy.join(", ")
        )));
    }
    for row in 1..=last_row {
        let values: Vec<String> = (0..column_count)
            .map(|c| cell_to_raw(get(row, c)))
            .collect();
        if values.iter().all(String::is_empty) {
            continue;
        }
        wb.rows.push(ParsedImportRow {
            row_number: row as usize + 1,
            values,
        });
        if wb.rows.len() > max_rows {
            return Err(ImportParseError {
                code: "IMPORT_TOO_MANY_ROWS",
                message: format!("File has more than {max_rows} data rows"),
            });
        }
    }
    Ok(wb)
}

/// Batas produksi.
pub fn parse_default(buffer: &[u8]) -> Result<ParsedImportWorkbook, ImportParseError> {
    parse_import_workbook(buffer, MAX_IMPORT_ROWS)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use rust_xlsxwriter::{ExcelDateTime, Format, Workbook};

    pub enum C {
        S(&'static str),
        N(f64),
        B(bool),
        D(u16, u8, u8),
    }

    pub fn build(headers: &[&str], rows: &[Vec<C>]) -> Vec<u8> {
        let mut wb = Workbook::new();
        let ws = wb.add_worksheet();
        ws.set_name("Products").unwrap();
        let date = Format::new().set_num_format("yyyy-mm-dd");
        for (c, h) in headers.iter().enumerate() {
            ws.write_string(0, c as u16, *h).unwrap();
        }
        for (r, row) in rows.iter().enumerate() {
            let r = r as u32 + 1;
            for (c, v) in row.iter().enumerate() {
                let c = c as u16;
                match v {
                    C::S("") => {}
                    C::S(s) => {
                        ws.write_string(r, c, *s).unwrap();
                    }
                    C::N(n) => {
                        ws.write_number(r, c, *n).unwrap();
                    }
                    C::B(b) => {
                        ws.write_boolean(r, c, *b).unwrap();
                    }
                    C::D(y, m, d) => {
                        let dt = ExcelDateTime::from_ymd(*y, *m, *d).unwrap();
                        ws.write_datetime_with_format(r, c, &dt, &date).unwrap();
                    }
                }
            }
        }
        wb.save_to_buffer().unwrap()
    }

    const FULL: &[&str] = &[
        "sku",
        "name",
        "categoryCode",
        "baseUomCode",
        "weight",
        "isActive",
    ];

    fn val<'a>(p: &'a ParsedImportWorkbook, row: usize, col: &str) -> &'a str {
        &p.rows[row].values[p.index_of(col).unwrap()]
    }

    #[test]
    fn header_dan_baris_kosong_dilewati() {
        let buf = build(
            FULL,
            &[
                vec![
                    C::S("SKU-1"),
                    C::S("Product 1"),
                    C::S("BEV"),
                    C::S("PCS"),
                    C::N(10.0),
                    C::B(true),
                ],
                vec![C::S(""), C::S(""), C::S(""), C::S(""), C::S(""), C::S("")],
                vec![
                    C::S("SKU-2"),
                    C::S("Product 2"),
                    C::S("BEV"),
                    C::S("PCS"),
                    C::N(20.0),
                    C::B(false),
                ],
            ],
        );
        let p = parse_import_workbook(&buf, 100).unwrap();
        for h in FULL {
            assert!(p.has(h));
        }
        assert_eq!(p.rows.len(), 2);
        assert_eq!(p.rows[0].row_number, 2);
        assert_eq!(val(&p, 0, "sku"), "SKU-1");
        assert_eq!(p.rows[1].row_number, 4);
        assert_eq!(val(&p, 1, "isActive"), "false");
    }

    #[test]
    fn bool_dan_angka_jadi_teks() {
        let buf = build(
            FULL,
            &[vec![
                C::S("SKU-1"),
                C::S("Product 1"),
                C::S("BEV"),
                C::S("PCS"),
                C::N(12.5),
                C::B(true),
            ]],
        );
        let p = parse_import_workbook(&buf, 100).unwrap();
        assert_eq!(val(&p, 0, "weight"), "12.5");
        assert_eq!(val(&p, 0, "isActive"), "true");
    }

    #[test]
    fn tanggal_jadi_yyyy_mm_dd() {
        let buf = build(
            &[
                "sku",
                "name",
                "categoryCode",
                "baseUomCode",
                "cf:launch_date",
            ],
            &[vec![
                C::S("SKU-1"),
                C::S("Product 1"),
                C::S("BEV"),
                C::S("PCS"),
                C::D(2026, 1, 15),
            ]],
        );
        let p = parse_import_workbook(&buf, 100).unwrap();
        assert_eq!(val(&p, 0, "cf:launch_date"), "2026-01-15");
    }

    #[test]
    fn header_wajib_hilang() {
        let buf = build(&["sku", "name"], &[vec![C::S("SKU-1"), C::S("Product 1")]]);
        let e = parse_import_workbook(&buf, 100).unwrap_err();
        assert_eq!(e.code, "IMPORT_INVALID_FILE");
        assert_eq!(
            e.message,
            "Missing required column header(s): categoryCode, baseUomCode"
        );
    }

    #[test]
    fn kolom_rate_lama_ditolak() {
        let buf = build(
            &[
                "sku",
                "name",
                "categoryCode",
                "baseUomCode",
                "uom2Code",
                "uom2Rate",
            ],
            &[vec![
                C::S("SKU-1"),
                C::S("Product 1"),
                C::S("BEV"),
                C::S("PCS"),
                C::S("BOX"),
                C::N(12.0),
            ]],
        );
        let e = parse_import_workbook(&buf, 100).unwrap_err();
        assert_eq!(e.code, "IMPORT_INVALID_FILE");
        assert!(e.message.contains("uom2Rate"));
    }

    #[test]
    fn terlalu_banyak_baris() {
        let rows: Vec<Vec<C>> = (0..5)
            .map(|_| vec![C::S("SKU"), C::S("P"), C::S("BEV"), C::S("PCS")])
            .collect();
        let buf = build(&["sku", "name", "categoryCode", "baseUomCode"], &rows);
        assert_eq!(
            parse_import_workbook(&buf, 3).unwrap_err().code,
            "IMPORT_TOO_MANY_ROWS"
        );
    }

    #[test]
    fn bukan_xlsx() {
        let e = parse_import_workbook(b"not an xlsx file", 100).unwrap_err();
        assert_eq!(e.code, "IMPORT_INVALID_FILE");
    }
}
