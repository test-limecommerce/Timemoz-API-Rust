//! Port `src/store-visits/stores-export.generator.ts`: kolom export toko (judul + kunci registry +
//! sel dalam SATU daftar) dan sensor kolom per pembaca (§6.3): `hidden` dibuang dari judul, mode
//! lain berkolom dengan sel kosong. `id`, `customerCode`, `customerName` tidak punya kunci registry.

use mz_table_fields::policy::CensoredColumns;
use mz_table_fields::registry::FieldVisibilityMode;

use super::common::StoreRow;
use crate::export::{Cell, csv_header_line, csv_line, format_date_tz};

/// Ukuran batch kursor CSV (`BATCH_SIZE`).
pub const BATCH_SIZE: i64 = 500;

/// Satu kolom berkas export.
pub struct ExportColumn {
    pub header: &'static str,
    pub column_key: Option<&'static str>,
    pub cell: fn(&StoreRow, &str) -> Cell,
}

fn s(v: &str) -> Cell {
    Cell::Text(v.to_owned())
}

/// `EXPORT_COLUMNS` (urutan berkas).
pub const EXPORT_COLUMNS: &[ExportColumn] = &[
    ExportColumn {
        header: "id",
        column_key: None,
        cell: |r, _| s(&r.uuid),
    },
    ExportColumn {
        header: "code",
        column_key: Some("code"),
        cell: |r, _| s(&r.code),
    },
    ExportColumn {
        header: "name",
        column_key: Some("name"),
        cell: |r, _| s(&r.name),
    },
    ExportColumn {
        header: "ownerName",
        column_key: Some("ownerName"),
        cell: |r, _| s(&r.owner_name),
    },
    ExportColumn {
        header: "phone",
        column_key: Some("phone"),
        cell: |r, _| Cell::s(r.phone.clone()),
    },
    ExportColumn {
        header: "address",
        column_key: Some("address"),
        cell: |r, _| s(&r.address),
    },
    ExportColumn {
        header: "lat",
        column_key: Some("lat"),
        cell: |r, _| Cell::Num(r.lat),
    },
    ExportColumn {
        header: "lng",
        column_key: Some("lng"),
        cell: |r, _| Cell::Num(r.lng),
    },
    ExportColumn {
        header: "radiusMeters",
        column_key: Some("radiusMeters"),
        cell: |r, _| Cell::n(r.radius_meters.map(f64::from)),
    },
    ExportColumn {
        header: "isActive",
        column_key: Some("isActive"),
        cell: |r, _| s(if r.is_active { "Yes" } else { "No" }),
    },
    // Judulnya `region`, kuncinya `group`.
    ExportColumn {
        header: "region",
        column_key: Some("group"),
        cell: |r, _| Cell::s(r.group_name.clone()),
    },
    ExportColumn {
        header: "customerCode",
        column_key: None,
        cell: |r, _| Cell::s(r.customer_code.clone()),
    },
    ExportColumn {
        header: "customerName",
        column_key: None,
        cell: |r, _| Cell::s(r.customer_name.clone()),
    },
    ExportColumn {
        header: "totalVisits",
        column_key: Some("totalVisits"),
        cell: |r, _| Cell::Num(f64::from(r.total_visits)),
    },
    ExportColumn {
        header: "lastVisitAt",
        column_key: Some("lastVisitAt"),
        cell: |r, tz| Cell::Text(format_date_tz(r.last_visit_at, tz)),
    },
    ExportColumn {
        header: "lastVisitStatus",
        column_key: Some("lastVisitStatus"),
        cell: |r, _| Cell::s(r.last_visit_status.clone()),
    },
    ExportColumn {
        header: "createdAt",
        column_key: Some("createdAt"),
        cell: |r, tz| Cell::Text(format_date_tz(Some(r.created_at), tz)),
    },
    ExportColumn {
        header: "updatedAt",
        column_key: Some("updatedAt"),
        cell: |r, tz| Cell::Text(format_date_tz(r.updated_at, tz)),
    },
];

/// `columnsFor(censored)`: kolom yang benar-benar ditulis untuk satu pembaca.
pub fn columns_for(censored: &CensoredColumns) -> Vec<&'static ExportColumn> {
    EXPORT_COLUMNS
        .iter()
        .filter(|c| {
            c.column_key
                .is_none_or(|k| censored.get(k) != Some(&FieldVisibilityMode::Hidden))
        })
        .collect()
}

/// Judul kolom untuk pembaca ini.
pub fn headers(columns: &[&'static ExportColumn]) -> Vec<&'static str> {
    columns.iter().map(|c| c.header).collect()
}

/// `toRowCells`: kolom tersensor (`masked`/`revealable`) → sel kosong.
pub fn row_cells(
    columns: &[&'static ExportColumn],
    row: &StoreRow,
    time_zone: &str,
    censored: &CensoredColumns,
) -> Vec<Cell> {
    columns
        .iter()
        .map(|c| {
            if c.column_key.is_some_and(|k| censored.contains_key(k)) {
                Cell::blank()
            } else {
                (c.cell)(row, time_zone)
            }
        })
        .collect()
}

/// Baris judul CSV (diawali BOM).
pub fn csv_header(columns: &[&'static ExportColumn]) -> String {
    csv_header_line(&headers(columns))
}

/// Satu baris CSV.
pub fn csv_row(
    columns: &[&'static ExportColumn],
    row: &StoreRow,
    time_zone: &str,
    censored: &CensoredColumns,
) -> String {
    csv_line(&row_cells(columns, row, time_zone, censored))
}

/// `buildStoresXlsx`: seluruh workbook di memori, sheet `Stores`.
pub fn build_stores_xlsx(
    rows: &[StoreRow],
    time_zone: &str,
    censored: &CensoredColumns,
) -> Result<Vec<u8>, rust_xlsxwriter::XlsxError> {
    let columns = columns_for(censored);
    let cells: Vec<Vec<Cell>> = rows
        .iter()
        .map(|r| row_cells(&columns, r, time_zone, censored))
        .collect();
    crate::export::build_xlsx("Stores", &headers(&columns), &cells)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stores::common::tests::row;
    use mz_core::time::JsDate;

    const PHONE: &str = "+6281234567890";
    const TZ: &str = "Asia/Jakarta";

    fn store() -> StoreRow {
        let mut r = row();
        r.uuid = "e3d1a9ce-1a2b-4c3d-8e9f-0a1b2c3d4e5f".into();
        r.name = "Toko Makmur Jaya".into();
        r.phone = Some(PHONE.into());
        r.radius_meters = Some(120);
        r.group_name = Some("Jogja Barat".into());
        r.customer_code = Some("CUST-001".into());
        r.customer_name = Some("PT Sumber Makmur".into());
        r.total_visits = 12;
        r.last_visit_at = JsDate::from_millis(1_785_553_200_000);
        r.last_visit_status = Some("success".into());
        r
    }

    fn censored(pairs: &[(&str, FieldVisibilityMode)]) -> CensoredColumns {
        pairs.iter().map(|(k, m)| ((*k).to_owned(), *m)).collect()
    }

    fn csv(rows: &[StoreRow], c: &CensoredColumns) -> (Vec<String>, Vec<Vec<String>>) {
        let cols = columns_for(c);
        let mut text = csv_header(&cols);
        for r in rows {
            text.push_str(&csv_row(&cols, r, TZ, c));
        }
        let text = text.strip_prefix('\u{feff}').unwrap();
        let mut lines = text.split("\r\n").filter(|l| !l.is_empty());
        let header = lines
            .next()
            .unwrap()
            .split(',')
            .map(str::to_owned)
            .collect();
        let body = lines
            .map(|l| l.split(',').map(str::to_owned).collect())
            .collect();
        (header, body)
    }

    const HEADER_TODAY: [&str; 18] = [
        "id",
        "code",
        "name",
        "ownerName",
        "phone",
        "address",
        "lat",
        "lng",
        "radiusMeters",
        "isActive",
        "region",
        "customerCode",
        "customerName",
        "totalVisits",
        "lastVisitAt",
        "lastVisitStatus",
        "createdAt",
        "updatedAt",
    ];

    #[test]
    fn no_policy_keeps_header_and_values() {
        let (h, l) = csv(&[store()], &censored(&[]));
        assert_eq!(h, HEADER_TODAY);
        let i = h.iter().position(|x| x == "phone").unwrap();
        assert_eq!(l[0][i], PHONE);
        assert_eq!(l[0].len(), HEADER_TODAY.len());
        assert_eq!(
            l[0][h.iter().position(|x| x == "region").unwrap()],
            "Jogja Barat"
        );
        assert_eq!(l[0][h.iter().position(|x| x == "isActive").unwrap()], "Yes");
        assert_eq!(
            l[0][h.iter().position(|x| x == "lastVisitAt").unwrap()],
            "2026-08-01 10:00:00 WIB"
        );
    }

    #[test]
    fn customer_columns_never_censored_and_null_customer_blank() {
        let c = censored(&[
            ("phone", FieldVisibilityMode::Hidden),
            ("name", FieldVisibilityMode::Hidden),
        ]);
        let (h, l) = csv(&[store()], &c);
        assert!(h.contains(&"customerCode".to_owned()) && h.contains(&"customerName".to_owned()));
        assert_eq!(
            l[0][h.iter().position(|x| x == "customerCode").unwrap()],
            "CUST-001"
        );
        let mut r = store();
        r.customer_code = None;
        r.customer_name = None;
        let (h, l) = csv(&[r], &censored(&[]));
        assert_eq!(
            l[0][h.iter().position(|x| x == "customerName").unwrap()],
            ""
        );
    }

    #[test]
    fn hidden_dropped_masked_and_revealable_blank() {
        let c = censored(&[
            ("phone", FieldVisibilityMode::Hidden),
            ("ownerName", FieldVisibilityMode::Masked),
            ("address", FieldVisibilityMode::Revealable),
            ("group", FieldVisibilityMode::Hidden),
        ]);
        let (h, l) = csv(&[store(), store()], &c);
        assert!(!h.contains(&"phone".to_owned()) && !h.contains(&"region".to_owned()));
        for line in &l {
            assert!(!line.iter().any(|v| v == PHONE || v == "Jogja Barat"));
            assert_eq!(line[h.iter().position(|x| x == "ownerName").unwrap()], "");
            assert_eq!(line[h.iter().position(|x| x == "address").unwrap()], "");
            assert_eq!(
                line[h.iter().position(|x| x == "name").unwrap()],
                "Toko Makmur Jaya"
            );
        }
        assert_eq!(h[0], "id");
    }

    #[test]
    fn xlsx_respects_policy() {
        use std::io::Read;
        let read = |bytes: Vec<u8>| {
            let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
            let mut s = String::new();
            zip.by_name("xl/sharedStrings.xml")
                .unwrap()
                .read_to_string(&mut s)
                .unwrap();
            s
        };
        let all = read(build_stores_xlsx(&[store()], TZ, &censored(&[])).unwrap());
        assert!(all.contains(PHONE) && all.contains(">phone<"));
        let hidden = read(
            build_stores_xlsx(
                &[store()],
                TZ,
                &censored(&[("phone", FieldVisibilityMode::Hidden)]),
            )
            .unwrap(),
        );
        assert!(!hidden.contains(PHONE) && !hidden.contains(">phone<"));
        let masked = read(
            build_stores_xlsx(
                &[store()],
                TZ,
                &censored(&[("phone", FieldVisibilityMode::Masked)]),
            )
            .unwrap(),
        );
        assert!(!masked.contains(PHONE) && masked.contains(">phone<"));
    }
}
