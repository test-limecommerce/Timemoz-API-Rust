//! Port `src/customers/customer-export.generator.ts` (`write-excel-file`): satu sheet `Customers`,
//! header tebal (latar `#1a56db`, huruf putih). Hanya Excel, tanpa kuota. Isi diformat dalam zona waktu
//! company. Kebijakan kolom (§6.3): `hidden` membuang kolom (judul + nilai), mode lain menyisakan judul
//! dan mengosongkan nilai — ekspor bukan JSON sehingga interceptor tidak menyentuhnya.

use chrono::SecondsFormat;
use mz_core::time::JsDate;
use mz_core::tz::zone_or_utc;
use mz_table_fields::policy::CensoredColumns;
use mz_table_fields::registry::FieldVisibilityMode;
use rust_xlsxwriter::{Color, Format, Workbook, XlsxError};

use crate::customers::CustomerRow;

/// Satu sel: teks atau angka.
enum Cell {
    Text(String),
    Num(f64),
}

struct Column {
    header: &'static str,
    /// Kunci kolom registry `customers`, atau `None` (`id`).
    column_key: Option<&'static str>,
    cell: fn(&CustomerRow, &str) -> Cell,
}

fn text(s: &str) -> Cell {
    Cell::Text(s.to_owned())
}

fn ref_label(r: Option<(&str, &str)>) -> Cell {
    Cell::Text(r.map_or_else(String::new, |(code, name)| format!("{code} — {name}")))
}

/// `moment(d).tz(timeZone).format('YYYY-MM-DD HH:mm:ss z')`.
pub fn format_date(d: Option<JsDate>, time_zone: &str) -> String {
    d.map_or_else(String::new, |d| {
        d.utc()
            .with_timezone(&zone_or_utc(Some(time_zone)))
            .format("%Y-%m-%d %H:%M:%S %Z")
            .to_string()
    })
}

const COLUMNS: &[Column] = &[
    Column {
        header: "id",
        column_key: None,
        cell: |r, _| text(&r.uuid),
    },
    Column {
        header: "code",
        column_key: Some("code"),
        cell: |r, _| text(&r.code),
    },
    Column {
        header: "name",
        column_key: Some("name"),
        cell: |r, _| text(&r.name),
    },
    Column {
        header: "type",
        column_key: Some("type"),
        cell: |r, _| ref_label(r.type_ref()),
    },
    Column {
        header: "group",
        column_key: Some("group"),
        cell: |r, _| ref_label(r.group_ref()),
    },
    Column {
        header: "channel",
        column_key: Some("channel"),
        cell: |r, _| ref_label(r.channel_ref()),
    },
    Column {
        header: "subChannel",
        column_key: Some("subChannel"),
        cell: |r, _| ref_label(r.sub_channel_ref()),
    },
    Column {
        header: "taxNumber",
        column_key: Some("taxNumber"),
        cell: |r, _| text(r.tax_number.as_deref().unwrap_or_default()),
    },
    Column {
        header: "phone",
        column_key: Some("phone"),
        cell: |r, _| text(r.phone.as_deref().unwrap_or_default()),
    },
    Column {
        header: "isActive",
        column_key: Some("isActive"),
        cell: |r, _| text(if r.is_active { "Yes" } else { "No" }),
    },
    Column {
        header: "storeCount",
        column_key: Some("storeCount"),
        #[allow(clippy::cast_precision_loss)]
        cell: |r, _| Cell::Num(r.store_count as f64),
    },
    Column {
        header: "createdAt",
        column_key: Some("createdAt"),
        cell: |r, tz| Cell::Text(format_date(Some(r.created_at), tz)),
    },
    Column {
        header: "updatedAt",
        column_key: Some("updatedAt"),
        cell: |r, tz| Cell::Text(format_date(r.updated_at, tz)),
    },
];

/// `columnsFor(censored)`: `hidden` membuang kolom; `masked`/`revealable` tetap ada (nilai kosong).
fn columns_for(censored: &CensoredColumns) -> Vec<&'static Column> {
    COLUMNS
        .iter()
        .filter(|c| {
            c.column_key
                .is_none_or(|k| censored.get(k) != Some(&FieldVisibilityMode::Hidden))
        })
        .collect()
}

/// Judul kolom yang akan tampil (untuk uji & dokumentasi).
pub fn visible_headers(censored: &CensoredColumns) -> Vec<&'static str> {
    columns_for(censored).iter().map(|c| c.header).collect()
}

/// `buildCustomersXlsx`: seluruh workbook di memori.
pub fn build_customers_xlsx(
    rows: &[CustomerRow],
    time_zone: &str,
    censored: &CensoredColumns,
) -> Result<Vec<u8>, XlsxError> {
    let columns = columns_for(censored);
    let header = Format::new()
        .set_bold()
        .set_background_color(Color::RGB(0x001a_56db))
        .set_font_color(Color::RGB(0x00ff_ffff));
    let mut wb = Workbook::new();
    let ws = wb.add_worksheet().set_name("Customers")?;
    for (ci, c) in columns.iter().enumerate() {
        ws.write_string_with_format(0, u16::try_from(ci).unwrap_or(u16::MAX), c.header, &header)?;
    }
    for (ri, row) in rows.iter().enumerate() {
        let r = u32::try_from(ri + 1).unwrap_or(u32::MAX);
        for (ci, c) in columns.iter().enumerate() {
            let col = u16::try_from(ci).unwrap_or(u16::MAX);
            // Kolom tersensor (mode apa pun yang bukan `visible`) → sel kosong.
            if c.column_key.is_some_and(|k| censored.contains_key(k)) {
                ws.write_string(r, col, "")?;
                continue;
            }
            match (c.cell)(row, time_zone) {
                Cell::Text(s) => {
                    ws.write_string(r, col, s.chars().take(32_767).collect::<String>())?
                }
                Cell::Num(n) => ws.write_number(r, col, n)?,
            };
        }
    }
    let _ = SecondsFormat::Millis;
    wb.save_to_buffer()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    const TAX: &str = "01.234.567.8-901.000";
    const PHONE: &str = "+6281234567890";

    fn row(group: bool) -> CustomerRow {
        let mut r = crate::customers::tests_support::sample_row();
        r.tax_number = Some(TAX.into());
        r.phone = Some(PHONE.into());
        if !group {
            r.clear_optional_refs();
        }
        r
    }

    fn censored(pairs: &[(&str, FieldVisibilityMode)]) -> CensoredColumns {
        pairs.iter().map(|(k, m)| ((*k).to_owned(), *m)).collect()
    }

    /// Seluruh string workbook ada di `xl/sharedStrings.xml`.
    fn shared_strings(bytes: &[u8]) -> String {
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        let mut f = zip.by_name("xl/sharedStrings.xml").unwrap();
        let mut s = String::new();
        f.read_to_string(&mut s).unwrap();
        s
    }

    #[test]
    fn without_policy_all_columns_appear() {
        let b =
            build_customers_xlsx(&[row(true)], "Asia/Jakarta", &CensoredColumns::new()).unwrap();
        let t = shared_strings(&b);
        assert!(t.contains(TAX) && t.contains(PHONE));
        assert!(t.contains(">storeCount<"));
        assert!(t.contains(">createdAt<"));
    }

    #[test]
    fn hidden_drops_header_and_value() {
        let c = censored(&[("taxNumber", FieldVisibilityMode::Hidden)]);
        let b = build_customers_xlsx(&[row(true)], "Asia/Jakarta", &c).unwrap();
        let t = shared_strings(&b);
        assert!(!t.contains(TAX) && !t.contains(">taxNumber<"));
        assert!(t.contains(">phone<") && t.contains(PHONE));
        assert!(!visible_headers(&c).contains(&"taxNumber"));
    }

    #[test]
    fn masked_keeps_header_not_value() {
        let c = censored(&[("taxNumber", FieldVisibilityMode::Masked)]);
        let b = build_customers_xlsx(&[row(true)], "Asia/Jakarta", &c).unwrap();
        let t = shared_strings(&b);
        assert!(t.contains(">taxNumber<") && !t.contains(TAX));
    }

    #[test]
    fn null_refs_write_blank_cells_without_crashing() {
        let b =
            build_customers_xlsx(&[row(false)], "Asia/Jakarta", &CensoredColumns::new()).unwrap();
        assert!(shared_strings(&b).contains("DIST — Distributor"));
    }

    #[test]
    fn dates_use_company_time_zone() {
        // 2026-01-02T03:00:00Z di Asia/Jakarta = 10:00 WIB.
        let d = JsDate::from_millis(1_767_322_800_000);
        assert_eq!(format_date(d, "Asia/Jakarta"), "2026-01-02 10:00:00 WIB");
        assert_eq!(format_date(d, "UTC"), "2026-01-02 03:00:00 UTC");
        assert_eq!(format_date(None, "UTC"), "");
        assert_eq!(format_date(d, "Not/AZone"), "2026-01-02 03:00:00 UTC");
    }
}
