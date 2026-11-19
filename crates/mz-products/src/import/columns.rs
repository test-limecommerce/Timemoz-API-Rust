//! Port `products/product-import.columns.ts` + `product-import.constants.ts`: daftar kolom
//! bersama template & parser.

use std::time::Duration;

/// Antrean khusus import produk (`PRODUCT_IMPORT_QUEUE`).
pub const PRODUCT_IMPORT_QUEUE: &str = "product-import";
/// §9.2 — 10 MB.
pub const MAX_IMPORT_FILE_BYTES: usize = 10 * 1024 * 1024;
/// §9.5 — baris data maksimum.
pub const MAX_IMPORT_ROWS: usize = 20_000;
/// §9.4 — tulis per batch.
pub const IMPORT_BATCH_SIZE: usize = 500;
/// §9.1 — `errors`/`warnings` dipotong.
pub const MAX_STORED_IMPORT_ISSUES: usize = 500;
/// `PRODUCT_IMPORT_JOB_TIMEOUT_MS` — 15 menit.
pub const PRODUCT_IMPORT_JOB_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// §8.2 — unduhan gambar.
pub const IMAGE_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(10);
pub const IMAGE_DOWNLOAD_MAX_REDIRECTS: usize = 3;
pub const IMAGE_DOWNLOAD_CONCURRENCY: usize = 5;

/// Kolom tetap, urutan §9.3.
pub const IMPORT_FIXED_COLUMNS: &[&str] = &[
    "sku",
    "name",
    "description",
    "categoryCode",
    "subCategoryCode",
    "brandCode",
    "principalCode",
    "baseUomCode",
    "taxCode",
    "weight",
    "volume",
    "imageUrl",
    "isOrderable",
    "isActive",
    "barcode",
];

/// Kolom `*` (header hilang = galat file).
pub const IMPORT_REQUIRED_COLUMNS: &[&str] = &["sku", "name", "categoryCode", "baseUomCode"];

pub const CUSTOM_FIELD_HEADER_PREFIX: &str = "cf:";

/// Slot UOM tambahan.
pub const IMPORT_UOM_SLOTS: [u8; 3] = [2, 3, 4];

pub fn custom_field_column_header(key: &str) -> String {
    format!("{CUSTOM_FIELD_HEADER_PREFIX}{key}")
}

/// `cf:<key>` → `<key>`.
pub fn parse_custom_field_header(header: &str) -> Option<&str> {
    header.strip_prefix(CUSTOM_FIELD_HEADER_PREFIX)
}

pub fn uom_code_column(slot: u8) -> String {
    format!("uom{slot}Code")
}

pub fn uom_operator_column(slot: u8) -> String {
    format!("uom{slot}Operator")
}

pub fn uom_factor_column(slot: u8) -> String {
    format!("uom{slot}Factor")
}

/// Kolom template lama (`uomNRate`) yang ditolak di tingkat file.
pub fn legacy_uom_rate_columns() -> Vec<String> {
    IMPORT_UOM_SLOTS
        .iter()
        .map(|s| format!("uom{s}Rate"))
        .collect()
}

/// `buildImportHeaders(activeCustomFieldKeys)`.
pub fn build_import_headers(active_custom_field_keys: &[String]) -> Vec<String> {
    let mut out: Vec<String> = IMPORT_FIXED_COLUMNS
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    out.extend(
        active_custom_field_keys
            .iter()
            .map(|k| custom_field_column_header(k)),
    );
    for slot in IMPORT_UOM_SLOTS {
        out.push(uom_code_column(slot));
        out.push(uom_operator_column(slot));
        out.push(uom_factor_column(slot));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_template() {
        let h = build_import_headers(&["shelf_life".into(), "storage".into()]);
        assert_eq!(h.first().map(String::as_str), Some("sku"));
        assert_eq!(h.get(15).map(String::as_str), Some("cf:shelf_life"));
        assert_eq!(h.get(16).map(String::as_str), Some("cf:storage"));
        assert_eq!(h.last().map(String::as_str), Some("uom4Factor"));
        assert_eq!(h.len(), 15 + 2 + 9);
        assert_eq!(parse_custom_field_header("cf:x"), Some("x"));
        assert_eq!(parse_custom_field_header("sku"), None);
    }
}
