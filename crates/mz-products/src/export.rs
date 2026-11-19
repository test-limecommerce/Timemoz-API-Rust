//! Port `products/product-export.generator.ts` — `GET /v1/admin/products/export` (xlsx, sheet
//! `Products`, header tebal putih di `#1a56db`). Kolom tersensor: `hidden` dibuang, mode lain
//! dikosongkan.

use chrono::TimeZone;
use mz_core::ApiResult;
use mz_table_fields::policy::CensoredColumns;
use mz_table_fields::registry::FieldVisibilityMode;
use rust_decimal::Decimal;
use rust_xlsxwriter::{Color, Format, Workbook};

use crate::common::internal;
use crate::masters::MasterRow;

/// Satu baris `productUom` untuk export (desimal sudah string).
#[derive(Debug, Clone)]
pub struct ExportUom {
    pub code: String,
    pub operator: String,
    pub factor: String,
    pub base_equivalent: String,
    pub is_base: bool,
}

/// `ProductExportRow`.
#[derive(Debug, Clone)]
pub struct ExportRow {
    pub id: String,
    pub sku: String,
    pub name: String,
    pub category: Option<MasterRow>,
    pub sub_category: Option<MasterRow>,
    pub brand: Option<MasterRow>,
    pub principal: Option<MasterRow>,
    pub base_uom: Option<MasterRow>,
    pub tax_rate: Option<MasterRow>,
    pub weight: Option<String>,
    pub volume: Option<String>,
    pub barcodes: Vec<String>,
    pub uoms: Vec<ExportUom>,
    pub is_orderable: bool,
    pub is_active: bool,
    pub created_at: i64,
    pub updated_at: Option<i64>,
}

enum Cell {
    Str(String),
    Num(f64),
}

fn ref_label(r: Option<&MasterRow>) -> String {
    r.map_or_else(String::new, |r| format!("{} — {}", r.code, r.name))
}

fn dec(v: Option<&str>) -> Cell {
    match v.map(mz_core::jsfmt::number) {
        Some(n) if n.is_finite() => Cell::Num(n),
        _ => Cell::Str(String::new()),
    }
}

/// `moment(ms).tz(tz).format('YYYY-MM-DD HH:mm:ss z')`.
pub fn format_date(ms: Option<i64>, time_zone: &str) -> String {
    let Some(ms) = ms else {
        return String::new();
    };
    let tz = mz_core::tz::zone_or_utc(Some(time_zone));
    tz.timestamp_millis_opt(ms)
        .single()
        .map_or_else(String::new, |d| {
            d.format("%Y-%m-%d %H:%M:%S %Z").to_string()
        })
}

/// `uomsLabel(row)`: UOM non-base, base equivalent terkecil dulu.
pub fn uoms_label(row: &ExportRow) -> String {
    let base = row.base_uom.as_ref().map_or("", |b| b.code.as_str());
    let mut list: Vec<&ExportUom> = row.uoms.iter().filter(|u| !u.is_base).collect();
    list.sort_by(|a, b| {
        let p = |s: &str| s.parse::<Decimal>().unwrap_or_default();
        p(&a.base_equivalent).cmp(&p(&b.base_equivalent))
    });
    list.iter()
        .map(|u| {
            format!(
                "{}: {} {} = {} {base}",
                u.code, u.operator, u.factor, u.base_equivalent
            )
            .trim_end()
            .to_owned()
        })
        .collect::<Vec<_>>()
        .join("; ")
}

type CellFn = fn(&ExportRow, &str) -> Cell;

fn s(v: String) -> Cell {
    Cell::Str(v)
}

fn yes_no(b: bool) -> Cell {
    s(if b { "Yes" } else { "No" }.to_owned())
}

/// `EXPORT_COLUMNS`: (header, columnKey registry, sel).
const COLUMNS: &[(&str, Option<&str>, CellFn)] = &[
    ("id", None, |r, _| s(r.id.clone())),
    ("sku", Some("sku"), |r, _| s(r.sku.clone())),
    ("name", Some("name"), |r, _| s(r.name.clone())),
    ("category", Some("category"), |r, _| {
        s(ref_label(r.category.as_ref()))
    }),
    ("subCategory", Some("subCategory"), |r, _| {
        s(ref_label(r.sub_category.as_ref()))
    }),
    ("brand", Some("brand"), |r, _| {
        s(ref_label(r.brand.as_ref()))
    }),
    ("principal", Some("principal"), |r, _| {
        s(ref_label(r.principal.as_ref()))
    }),
    ("baseUom", Some("baseUom"), |r, _| {
        s(ref_label(r.base_uom.as_ref()))
    }),
    ("uoms", Some("baseUom"), |r, _| s(uoms_label(r))),
    ("taxRate", Some("taxRate"), |r, _| {
        s(ref_label(r.tax_rate.as_ref()))
    }),
    ("weight", Some("weight"), |r, _| dec(r.weight.as_deref())),
    ("volume", Some("volume"), |r, _| dec(r.volume.as_deref())),
    ("barcode", Some("barcode"), |r, _| s(r.barcodes.join(", "))),
    ("isOrderable", Some("isOrderable"), |r, _| {
        yes_no(r.is_orderable)
    }),
    ("isActive", Some("isActive"), |r, _| yes_no(r.is_active)),
    ("createdAt", Some("createdAt"), |r, tz| {
        s(format_date(Some(r.created_at), tz))
    }),
    ("updatedAt", Some("updatedAt"), |r, tz| {
        s(format_date(r.updated_at, tz))
    }),
];

/// `buildProductsXlsx(rows, timeZone, censored)`.
pub fn build_products_xlsx(
    rows: &[ExportRow],
    time_zone: &str,
    censored: &CensoredColumns,
) -> ApiResult<Vec<u8>> {
    let cols: Vec<&(&str, Option<&str>, CellFn)> = COLUMNS
        .iter()
        .filter(|(_, key, _)| {
            key.is_none_or(|k| censored.get(k) != Some(&FieldVisibilityMode::Hidden))
        })
        .collect();
    let header = Format::new()
        .set_bold()
        .set_background_color(Color::RGB(0x001a_56db))
        .set_font_color(Color::RGB(0x00ff_ffff));
    let mut wb = Workbook::new();
    let ws = wb.add_worksheet();
    let err = |e: rust_xlsxwriter::XlsxError| internal(&e.to_string());
    ws.set_name("Products").map_err(err)?;
    for (c, (h, _, _)) in cols.iter().enumerate() {
        let c = u16::try_from(c).unwrap_or(u16::MAX);
        ws.write_string_with_format(0, c, *h, &header)
            .map_err(err)?;
    }
    for (i, row) in rows.iter().enumerate() {
        let r = u32::try_from(i + 1).unwrap_or(u32::MAX);
        for (c, (_, key, cell)) in cols.iter().enumerate() {
            let c = u16::try_from(c).unwrap_or(u16::MAX);
            let value = if key.is_some_and(|k| censored.contains_key(k)) {
                Cell::Str(String::new())
            } else {
                cell(row, time_zone)
            };
            match value {
                Cell::Str(v) if v.is_empty() => {}
                Cell::Str(v) => {
                    ws.write_string(r, c, v).map_err(err)?;
                }
                Cell::Num(n) => {
                    ws.write_number(r, c, n).map_err(err)?;
                }
            }
        }
    }
    wb.save_to_buffer().map_err(err)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row() -> ExportRow {
        ExportRow {
            id: "u".into(),
            sku: "A".into(),
            name: "B".into(),
            category: None,
            sub_category: None,
            brand: None,
            principal: None,
            base_uom: None,
            tax_rate: None,
            weight: Some("12.5".into()),
            volume: None,
            barcodes: vec!["1".into(), "2".into()],
            uoms: vec![
                ExportUom {
                    code: "BOX".into(),
                    operator: "multiply".into(),
                    factor: "24".into(),
                    base_equivalent: "24".into(),
                    is_base: false,
                },
                ExportUom {
                    code: "PACK".into(),
                    operator: "multiply".into(),
                    factor: "6".into(),
                    base_equivalent: "6".into(),
                    is_base: false,
                },
                ExportUom {
                    code: "PCS".into(),
                    operator: "multiply".into(),
                    factor: "1".into(),
                    base_equivalent: "1".into(),
                    is_base: true,
                },
            ],
            is_orderable: true,
            is_active: false,
            created_at: 1_786_000_000_000,
            updated_at: None,
        }
    }

    // product-export.generator.spec.ts (ringkas)
    #[test]
    fn label_uom_dan_tanggal() {
        assert_eq!(
            uoms_label(&row()),
            "PACK: multiply 6 = 6; BOX: multiply 24 = 24"
        );
        assert_eq!(
            format_date(Some(1_786_000_000_000), "Asia/Jakarta"),
            "2026-08-06 14:06:40 WIB"
        );
        assert_eq!(format_date(None, "UTC"), "");
        let buf = build_products_xlsx(&[row()], "UTC", &CensoredColumns::new()).unwrap();
        assert!(buf.starts_with(b"PK"));
    }
}
