//! Port `src/leave-holidays/holiday-import.service.ts` (bagian murni: unggahan, parse CSV,
//! klasifikasi baris) + `holiday-import-file.filter.ts` (pesan 413).

use std::collections::{HashMap, HashSet};

use axum::http::StatusCode;
use chrono::NaiveDate;
use mz_core::ApiError;
use serde::Serialize;
use serde_json::{Map, Value};

use crate::csv;

/// `MAX_HOLIDAY_IMPORT_BYTES`.
pub const MAX_IMPORT_BYTES: usize = 262_144;
/// `MAX_HOLIDAY_IMPORT_ROWS`.
pub const MAX_IMPORT_ROWS: usize = 366;

const REQUIRED_HEADERS: [&str; 4] = ["tanggal", "nama", "jenis", "potong_cuti"];

/// Windows/browser kirim vnd.ms-excel, curl kirim octet-stream; isi tetap divalidasi parser.
const ALLOWED_MIMETYPES: [&str; 5] = [
    "text/csv",
    "application/csv",
    "text/plain",
    "application/vnd.ms-excel",
    "application/octet-stream",
];

/// `Express.Multer.File` (`originalname`, `mimetype`, `buffer`).
#[derive(Debug, Clone)]
pub struct Upload {
    pub original_name: String,
    pub mime: String,
    pub bytes: Vec<u8>,
}

/// `ParsedHolidayRow`: `row` = nomor baris file (header = baris 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedRow {
    pub row: usize,
    pub raw: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToAdd {
    pub row: usize,
    pub date: String,
    pub name: String,
    #[serde(rename = "type")]
    pub ty: &'static str,
    pub deducts_leave: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Skipped {
    pub row: usize,
    pub date: String,
    pub name: String,
    pub existing_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Invalid {
    pub row: usize,
    pub raw: Map<String, Value>,
    pub errors: Vec<String>,
}

/// `HolidayClassification` (urutan kunci = urutan literal Node).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Classification {
    pub to_add: Vec<ToAdd>,
    pub skipped_existing: Vec<Skipped>,
    pub invalid: Vec<Invalid>,
    pub deducting_joint_leave_count: usize,
}

/// `invalidFile(message)`: 422 `{code:'IMPORT_INVALID_FILE', message}`.
pub fn invalid_file(message: impl Into<String>) -> ApiError {
    ApiError::coded(
        StatusCode::UNPROCESSABLE_ENTITY,
        "IMPORT_INVALID_FILE",
        message,
    )
}

/// `ImportFileExceptionFilter('256 KB')`: 413 `{code:'IMPORT_FILE_TOO_LARGE', message}`.
pub fn file_too_large() -> ApiError {
    ApiError::coded(
        StatusCode::PAYLOAD_TOO_LARGE,
        "IMPORT_FILE_TOO_LARGE",
        format!("File exceeds the {} KB limit", MAX_IMPORT_BYTES / 1024),
    )
}

/// `assertCsvFile(file)`.
pub fn assert_csv_file(file: Option<&Upload>) -> Result<(), ApiError> {
    let Some(file) = file.filter(|f| !f.bytes.is_empty()) else {
        return Err(invalid_file("No file uploaded"));
    };
    if !file.original_name.to_ascii_lowercase().ends_with(".csv") {
        return Err(invalid_file("File must have a .csv extension"));
    }
    if !ALLOWED_MIMETYPES.contains(&file.mime.to_lowercase().as_str()) {
        return Err(invalid_file(
            "File type is not supported, upload a CSV file",
        ));
    }
    Ok(())
}

/// `parse(buffer)`.
pub fn parse(buffer: &[u8]) -> Result<Vec<ParsedRow>, ApiError> {
    let parsed = csv::parse(buffer).map_err(invalid_file)?;
    let missing: Vec<&str> = REQUIRED_HEADERS
        .iter()
        .copied()
        .filter(|h| !parsed.headers.iter().any(|x| x == h))
        .collect();
    if !missing.is_empty() {
        return Err(invalid_file(format!(
            "Kolom wajib tidak ada: {}",
            missing.join(", ")
        )));
    }
    if parsed.records.len() > MAX_IMPORT_ROWS {
        return Err(invalid_file(format!(
            "Maksimal {MAX_IMPORT_ROWS} baris data per file"
        )));
    }
    Ok(parsed
        .records
        .into_iter()
        .map(|r| {
            let mut raw = Map::new();
            for (name, value) in parsed.headers.iter().zip(r.fields) {
                // `obj[name] = value`: nama kembar menimpa nilai, posisi kunci tetap.
                raw.insert(name.clone(), Value::String(value));
            }
            ParsedRow { row: r.line, raw }
        })
        .collect())
}

/// `DATE_SHAPE` + `isRealDate`: `YYYY-MM-DD` dan benar-benar tanggal kalender.
pub fn real_date(key: &str) -> Option<NaiveDate> {
    let b = key.as_bytes();
    let shape = b.len() == 10
        && b.iter().enumerate().all(|(i, c)| match i {
            4 | 7 => *c == b'-',
            _ => c.is_ascii_digit(),
        });
    if !shape {
        return None;
    }
    let y: i32 = key.get(0..4)?.parse().ok()?;
    let m: u32 = key.get(5..7)?.parse().ok()?;
    let d: u32 = key.get(8..10)?.parse().ok()?;
    NaiveDate::from_ymd_opt(y, m, d)
}

fn text<'a>(raw: &'a Map<String, Value>, key: &str) -> &'a str {
    raw.get(key)
        .and_then(Value::as_str)
        .map_or("", mz_core::jsfmt::trim)
}

/// `classifyHolidayRows(rows, existingActiveByDate)` — murni.
pub fn classify(
    rows: &[ParsedRow],
    existing_active_by_date: &HashMap<String, String>,
) -> Classification {
    let mut result = Classification::default();
    let mut first_row_by_date: HashMap<String, usize> = HashMap::new();

    for ParsedRow { row, raw } in rows {
        let mut errors: Vec<String> = Vec::new();

        let date = text(raw, "tanggal").to_owned();
        let date_ok = real_date(&date).is_some();
        if !date_ok {
            errors.push("tanggal tidak valid (format YYYY-MM-DD)".to_owned());
        }

        let name = text(raw, "nama").to_owned();
        let name_len = name.encode_utf16().count();
        if !(1..=200).contains(&name_len) {
            errors.push("nama wajib diisi, maksimal 200 karakter".to_owned());
        }

        let jenis = text(raw, "jenis").to_lowercase();
        let ty: Option<&'static str> = match jenis.as_str() {
            "libur nasional" => Some("NATIONAL_HOLIDAY"),
            "cuti bersama" => Some("JOINT_LEAVE"),
            _ => {
                errors.push("jenis harus Libur Nasional atau Cuti Bersama".to_owned());
                None
            }
        };

        let potong = text(raw, "potong_cuti").to_lowercase();
        let mut deducts_leave = false;
        if potong == "ya" {
            deducts_leave = true;
        } else if potong != "tidak" {
            errors.push("potong_cuti harus ya atau tidak".to_owned());
        }
        if deducts_leave && ty == Some("NATIONAL_HOLIDAY") {
            errors.push("Libur Nasional tidak bisa potong cuti".to_owned());
        }

        if date_ok {
            match first_row_by_date.get(&date) {
                None => {
                    first_row_by_date.insert(date.clone(), *row);
                }
                Some(first) => errors.push(format!("tanggal duplikat dengan baris {first}")),
            }
        }

        let Some(ty) = ty.filter(|_| errors.is_empty()) else {
            result.invalid.push(Invalid {
                row: *row,
                raw: raw.clone(),
                errors,
            });
            continue;
        };

        if let Some(existing_name) = existing_active_by_date.get(&date) {
            result.skipped_existing.push(Skipped {
                row: *row,
                date,
                name,
                existing_name: existing_name.clone(),
            });
            continue;
        }

        if ty == "JOINT_LEAVE" && deducts_leave {
            result.deducting_joint_leave_count += 1;
        }
        result.to_add.push(ToAdd {
            row: *row,
            date,
            name,
            ty,
            deducts_leave,
        });
    }
    result
}

/// Kunci tanggal valid unik dari kolom `tanggal` (`existingActiveByDate`).
pub fn valid_date_keys(rows: &[ParsedRow]) -> Vec<NaiveDate> {
    let mut seen = HashSet::new();
    rows.iter()
        .filter_map(|r| real_date(text(&r.raw, "tanggal")))
        .filter(|d| seen.insert(*d))
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;

    const HEADER: &str = "tanggal,nama,jenis,potong_cuti";

    fn csv_of(lines: &[&str]) -> Vec<u8> {
        lines.join("\n").into_bytes()
    }

    fn code_of(e: &ApiError) -> String {
        match e {
            ApiError::Body { body, .. } => body["code"].as_str().unwrap_or_default().to_owned(),
            other => format!("{other:?}"),
        }
    }

    fn raw(pairs: &[(&str, &str)]) -> Map<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), Value::String((*v).to_owned())))
            .collect()
    }

    fn ok_raw() -> Map<String, Value> {
        raw(&[
            ("tanggal", "2027-01-01"),
            ("nama", "Tahun Baru"),
            ("jenis", "Libur Nasional"),
            ("potong_cuti", "tidak"),
        ])
    }

    fn with(key: &str, value: &str) -> Map<String, Value> {
        let mut m = ok_raw();
        m.insert(key.to_owned(), Value::String(value.to_owned()));
        m
    }

    fn row(n: usize, raw: Map<String, Value>) -> ParsedRow {
        ParsedRow { row: n, raw }
    }

    fn run(raw: Map<String, Value>, existing: &[(&str, &str)]) -> Classification {
        let map = existing
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        classify(&[row(2, raw)], &map)
    }

    #[test]
    fn parse_header_wajib_lengkap() {
        let e = parse(&csv_of(&[
            "tanggal,nama,jenis",
            "2027-01-01,A,Libur Nasional",
        ]))
        .unwrap_err();
        assert_eq!(code_of(&e), "IMPORT_INVALID_FILE");
        match e {
            ApiError::Body { body, status } => {
                assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
                assert_eq!(body["message"], "Kolom wajib tidak ada: potong_cuti");
            }
            _ => panic!("harus 422"),
        }
    }

    #[test]
    fn parse_header_toleran_dan_kolom_ekstra() {
        let rows = parse(&csv_of(&[
            "Tanggal , NAMA,Jenis ,Potong_Cuti",
            "2027-01-01,A,Libur Nasional,tidak",
        ]))
        .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].raw["tanggal"], "2027-01-01");
        assert!(
            parse(&csv_of(&[&format!("{HEADER},catatan")]))
                .unwrap()
                .is_empty()
        );
        let rows = parse(&csv_of(&[
            &format!("{HEADER},catatan"),
            "2027-01-01,A,Libur Nasional,tidak,x",
        ]))
        .unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn parse_bom_batas_baris_dan_rusak() {
        let mut b = vec![0xEF, 0xBB, 0xBF];
        b.extend(csv_of(&[HEADER, "2027-01-01,A,Libur Nasional,tidak"]));
        assert_eq!(parse(&b).unwrap()[0].raw["tanggal"], "2027-01-01");

        let many = |n: usize| {
            let mut lines = vec![HEADER.to_owned()];
            lines.extend((0..n).map(|i| format!("2027-01-01,N{i},Libur Nasional,tidak")));
            lines.join("\n").into_bytes()
        };
        assert_eq!(parse(&many(366)).unwrap().len(), 366);
        assert_eq!(
            code_of(&parse(&many(367)).unwrap_err()),
            "IMPORT_INVALID_FILE"
        );

        assert_eq!(
            code_of(&parse(&csv_of(&[HEADER, "2027-01-01,\"A,Libur Nasional,tidak"])).unwrap_err()),
            "IMPORT_INVALID_FILE"
        );
        assert_eq!(
            code_of(&parse(&csv_of(&[HEADER, "2027-01-01,A"])).unwrap_err()),
            "IMPORT_INVALID_FILE"
        );
    }

    #[test]
    fn parse_nomor_baris_dan_baris_kosong() {
        let rows = parse(&csv_of(&[
            HEADER,
            "2027-01-01,A,Libur Nasional,tidak",
            "",
            "2027-01-02,B,Libur Nasional,tidak",
        ]))
        .unwrap();
        assert_eq!(rows.iter().map(|r| r.row).collect::<Vec<_>>(), [2, 4]);
    }

    #[test]
    fn parse_tanpa_isi_semua_header_hilang() {
        match parse(b"").unwrap_err() {
            ApiError::Body { body, .. } => assert_eq!(
                body["message"],
                "Kolom wajib tidak ada: tanggal, nama, jenis, potong_cuti"
            ),
            _ => panic!("harus 422"),
        }
    }

    #[test]
    fn klasifikasi_valid_nama_di_trim() {
        let r = run(with("nama", "  Tahun Baru  "), &[]);
        assert_eq!(
            r.to_add,
            [ToAdd {
                row: 2,
                date: "2027-01-01".into(),
                name: "Tahun Baru".into(),
                ty: "NATIONAL_HOLIDAY",
                deducts_leave: false
            }]
        );
    }

    #[test]
    fn klasifikasi_tanggal_tidak_valid() {
        for d in ["2027-02-30", "2027-1-01", "01/01/2027", ""] {
            let r = run(with("tanggal", d), &[]);
            assert!(
                r.invalid[0]
                    .errors
                    .contains(&"tanggal tidak valid (format YYYY-MM-DD)".to_owned()),
                "{d}"
            );
        }
    }

    #[test]
    fn klasifikasi_nama_batas() {
        assert_eq!(run(with("nama", "  "), &[]).invalid.len(), 1);
        assert_eq!(run(with("nama", &"x".repeat(201)), &[]).invalid.len(), 1);
        assert_eq!(run(with("nama", &"x".repeat(200)), &[]).to_add.len(), 1);
    }

    #[test]
    fn klasifikasi_jenis_dan_potong_cuti() {
        assert_eq!(
            run(with("jenis", " LIBUR NASIONAL "), &[]).to_add[0].ty,
            "NATIONAL_HOLIDAY"
        );
        assert_eq!(
            run(with("jenis", "cuti BERSAMA"), &[]).to_add[0].ty,
            "JOINT_LEAVE"
        );
        assert!(
            run(with("jenis", "lainnya"), &[]).invalid[0]
                .errors
                .contains(&"jenis harus Libur Nasional atau Cuti Bersama".to_owned())
        );

        let mut m = with("jenis", "Cuti Bersama");
        m.insert("potong_cuti".into(), Value::String("YA".into()));
        assert!(run(m, &[]).to_add[0].deducts_leave);
        assert!(!run(with("potong_cuti", "Tidak"), &[]).to_add[0].deducts_leave);
        assert!(
            run(with("potong_cuti", "mungkin"), &[]).invalid[0]
                .errors
                .contains(&"potong_cuti harus ya atau tidak".to_owned())
        );
        assert!(
            run(with("potong_cuti", "ya"), &[]).invalid[0]
                .errors
                .contains(&"Libur Nasional tidak bisa potong cuti".to_owned())
        );
    }

    #[test]
    fn klasifikasi_semua_error_dikumpulkan() {
        let r = run(
            raw(&[
                ("tanggal", "x"),
                ("nama", ""),
                ("jenis", "y"),
                ("potong_cuti", "z"),
            ]),
            &[],
        );
        assert_eq!(r.invalid[0].errors.len(), 4);
    }

    #[test]
    fn klasifikasi_tanggal_duplikat() {
        let r = classify(
            &[
                row(2, ok_raw()),
                row(3, with("nama", "Lain")),
                row(4, with("jenis", "x")),
            ],
            &HashMap::new(),
        );
        assert_eq!(r.to_add.iter().map(|x| x.row).collect::<Vec<_>>(), [2]);
        assert_eq!(r.invalid[0].row, 3);
        assert_eq!(r.invalid[0].errors, ["tanggal duplikat dengan baris 2"]);
        assert!(
            r.invalid[1]
                .errors
                .contains(&"tanggal duplikat dengan baris 2".to_owned())
        );

        let r = classify(
            &[row(2, with("jenis", "x")), row(3, ok_raw())],
            &HashMap::new(),
        );
        assert_eq!(r.invalid.iter().map(|x| x.row).collect::<Vec<_>>(), [2, 3]);
        assert_eq!(r.invalid[1].errors, ["tanggal duplikat dengan baris 2"]);
    }

    #[test]
    fn klasifikasi_yang_sudah_ada_dilewati() {
        let r = run(ok_raw(), &[("2027-01-01", "Nama Lama")]);
        assert_eq!(
            r.skipped_existing,
            [Skipped {
                row: 2,
                date: "2027-01-01".into(),
                name: "Tahun Baru".into(),
                existing_name: "Nama Lama".into()
            }]
        );
        assert!(r.to_add.is_empty());
        assert_eq!(run(ok_raw(), &[("2027-01-02", "x")]).to_add.len(), 1);
    }

    #[test]
    fn klasifikasi_hitung_cuti_bersama_memotong() {
        let jb = |d: &str, p: &str| {
            let mut m = with("tanggal", d);
            m.insert("jenis".into(), Value::String("Cuti Bersama".into()));
            m.insert("potong_cuti".into(), Value::String(p.into()));
            m
        };
        let r = classify(
            &[
                row(2, jb("2027-02-08", "ya")),
                row(3, jb("2027-02-09", "tidak")),
                row(4, jb("2027-02-10", "ya")),
                row(5, with("tanggal", "2027-02-11")),
            ],
            &HashMap::new(),
        );
        assert_eq!(r.deducting_joint_leave_count, 2);
    }

    #[test]
    fn unggahan() {
        let up = |bytes: &[u8], name: &str, mime: &str| Upload {
            original_name: name.into(),
            mime: mime.into(),
            bytes: bytes.to_vec(),
        };
        assert!(assert_csv_file(None).is_err());
        assert!(assert_csv_file(Some(&up(b"", "a.csv", "text/csv"))).is_err());
        assert!(assert_csv_file(Some(&up(b"a", "libur.xlsx", "text/csv"))).is_err());
        assert!(assert_csv_file(Some(&up(b"a", "a.csv", "image/png"))).is_err());
        assert!(assert_csv_file(Some(&up(b"a", "A.CSV", "application/vnd.ms-excel"))).is_ok());
        assert!(assert_csv_file(Some(&up(b"a", "a.csv", "application/octet-stream"))).is_ok());
    }

    #[test]
    fn kunci_tanggal_valid_unik() {
        let rows = [
            row(2, ok_raw()),
            row(3, ok_raw()),
            row(4, with("tanggal", "x")),
            row(5, with("tanggal", "2027-01-02")),
        ];
        assert_eq!(valid_date_keys(&rows).len(), 2);
    }
}
