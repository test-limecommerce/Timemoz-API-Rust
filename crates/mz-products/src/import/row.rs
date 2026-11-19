//! Port `products/product-import-row.processor.ts`: satu baris sheet → payload tulis yang sudah
//! di-resolve, atau daftar galat baris. Murni (master & definisi custom field ditarik sekali per job).

use std::collections::{HashMap, HashSet};

use serde::Serialize;
use serde_json::{Map, Value};

use super::columns::{
    IMPORT_UOM_SLOTS, parse_custom_field_header, uom_code_column, uom_factor_column,
    uom_operator_column,
};
use super::parser::ParsedImportWorkbook;
use crate::common::dec_str;
use crate::custom_fields_validator::{CfDefinition, CfEntity, validate_custom_field_values};
use crate::dto::{BARCODE, CONVERSION_RATE, PRODUCT_SKU, WEIGHT_VOLUME};
use crate::masters::MasterRow;
use crate::util::compute_base_equivalent;

/// Master per kode (kode tersimpan selalu huruf besar).
#[derive(Debug, Default, Clone)]
pub struct MasterMaps {
    pub category: HashMap<String, MasterRow>,
    pub sub_category: HashMap<String, MasterRow>,
    pub brand: HashMap<String, MasterRow>,
    pub principal: HashMap<String, MasterRow>,
    pub uom: HashMap<String, MasterRow>,
    pub tax: HashMap<String, MasterRow>,
}

/// `RowIssue` — bentuk entri `errors`/`warnings` job (`{ row, column, code, message }`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RowIssue {
    pub row: usize,
    pub column: Option<String>,
    pub code: String,
    pub message: String,
}

impl RowIssue {
    pub fn new(row: usize, column: Option<&str>, code: &str, message: String) -> Self {
        Self {
            row,
            column: column.map(str::to_owned),
            code: code.to_owned(),
            message,
        }
    }
}

/// `ResolvedImportUom`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedImportUom {
    pub uom_id: i32,
    pub operator: String,
    pub factor: String,
    /// Base equivalent (`productUom.conversionRate`).
    pub conversion_rate: String,
    pub is_base: bool,
}

/// `ResolvedImportRow`.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedImportRow {
    pub row_number: usize,
    pub sku: String,
    pub name: String,
    pub description: Option<String>,
    pub category_id: i32,
    pub sub_category_id: Option<i32>,
    pub brand_id: Option<i32>,
    pub principal_id: Option<i32>,
    pub base_uom_id: i32,
    pub tax_rate_id: Option<i32>,
    pub weight: Option<String>,
    pub volume: Option<String>,
    pub image_url: Option<String>,
    pub is_orderable: bool,
    pub is_active: bool,
    pub barcode: Option<String>,
    pub custom_fields: Map<String, Value>,
    pub uoms: Vec<ResolvedImportUom>,
}

/// `RowResolution` — `data = None` bila gagal.
#[derive(Debug, Clone)]
pub struct RowResolution {
    pub data: Option<ResolvedImportRow>,
    pub issues: Vec<RowIssue>,
}

fn cell<'a>(values: &'a [String], wb: &ParsedImportWorkbook, column: &str) -> &'a str {
    wb.index_of(column)
        .and_then(|i| values.get(i))
        .map_or("", String::as_str)
}

fn required_missing(row: usize, column: &str) -> RowIssue {
    RowIssue::new(
        row,
        Some(column),
        "IMPORT_ROW_MISSING_REQUIRED",
        format!("{column} is required"),
    )
}

/// `resolveMasterCode`: `None` = kosong / tidak dikenal / arsip (dua terakhir menambah galat).
fn resolve_master<'m>(
    map: &'m HashMap<String, MasterRow>,
    raw: &str,
    column: &str,
    row: usize,
    issues: &mut Vec<RowIssue>,
) -> Option<&'m MasterRow> {
    if raw.is_empty() {
        return None;
    }
    let Some(m) = map.get(&raw.to_uppercase()) else {
        issues.push(RowIssue::new(
            row,
            Some(column),
            "PRODUCT_MASTER_NOT_FOUND",
            format!("{column} \"{raw}\" does not match any known code"),
        ));
        return None;
    };
    if !m.is_active {
        issues.push(RowIssue::new(
            row,
            Some(column),
            "PRODUCT_MASTER_ARCHIVED",
            format!("{column} \"{raw}\" refers to an archived row"),
        ));
        return None;
    }
    Some(m)
}

fn parse_bool_cell(
    raw: &str,
    column: &str,
    row: usize,
    fallback: bool,
    issues: &mut Vec<RowIssue>,
) -> bool {
    if raw.is_empty() {
        return fallback;
    }
    match raw.to_lowercase().as_str() {
        "true" | "1" | "yes" => true,
        "false" | "0" | "no" => false,
        _ => {
            issues.push(RowIssue::new(
                row,
                Some(column),
                "IMPORT_ROW_INVALID_BOOLEAN",
                format!("{column} must be true/false/yes/no/1/0"),
            ));
            fallback
        }
    }
}

fn parse_decimal(
    raw: &str,
    column: &str,
    row: usize,
    issues: &mut Vec<RowIssue>,
) -> Option<String> {
    if raw.is_empty() {
        return None;
    }
    if !WEIGHT_VOLUME.is_match(raw) {
        issues.push(RowIssue::new(
            row,
            Some(column),
            "IMPORT_ROW_INVALID_DECIMAL",
            format!("{column} must be a non-negative number with up to 4 decimal places"),
        ));
        return None;
    }
    Some(raw.to_owned())
}

/// `castCustomFieldCellValue(type, raw)`; `None` = sel kosong (kunci tidak dikirim).
fn cast_cell(field_type: &str, raw: &str) -> Option<Value> {
    let t = mz_core::jsfmt::trim(raw);
    if t.is_empty() {
        return None;
    }
    Some(match field_type {
        "number" => {
            let n = mz_core::jsfmt::number(t);
            if n.is_finite() {
                mz_core::jsfmt::num(n)
            } else {
                Value::String(t.to_owned())
            }
        }
        "boolean" => match t.to_lowercase().as_str() {
            "true" | "1" | "yes" => Value::Bool(true),
            "false" | "0" | "no" => Value::Bool(false),
            _ => Value::String(t.to_owned()),
        },
        "multiselect" => Value::Array(
            t.split(',')
                .map(mz_core::jsfmt::trim)
                .filter(|s| !s.is_empty())
                .map(|s| Value::String(s.to_owned()))
                .collect(),
        ),
        _ => Value::String(t.to_owned()),
    })
}

fn resolve_custom_fields(
    values: &[String],
    wb: &ParsedImportWorkbook,
    defs: &[CfDefinition],
    row: usize,
    issues: &mut Vec<RowIssue>,
) -> Map<String, Value> {
    let type_by_key: HashMap<&str, &str> = defs
        .iter()
        .map(|d| (d.key.as_str(), d.field_type.as_str()))
        .collect();
    let mut raw = Map::new();
    for (header, index) in &wb.headers {
        let Some(key) = parse_custom_field_header(header) else {
            continue;
        };
        let t = type_by_key.get(key).copied().unwrap_or("text");
        let v = values.get(*index).map_or("", String::as_str);
        if let Some(v) = cast_cell(t, v) {
            raw.insert(key.to_owned(), v);
        }
    }
    for i in validate_custom_field_values(CfEntity::Product, defs, &raw) {
        issues.push(RowIssue {
            row,
            column: Some(format!("cf:{}", i.key)),
            code: i.code,
            message: i.message,
        });
    }
    raw
}

fn resolve_uoms(
    values: &[String],
    wb: &ParsedImportWorkbook,
    masters: &MasterMaps,
    base: &MasterRow,
    row: usize,
    issues: &mut Vec<RowIssue>,
) -> Vec<ResolvedImportUom> {
    let mut uoms = vec![ResolvedImportUom {
        uom_id: base.id,
        operator: "multiply".into(),
        factor: "1".into(),
        conversion_rate: "1".into(),
        is_base: true,
    }];
    let mut seen: HashSet<i32> = HashSet::from([base.id]);
    for slot in IMPORT_UOM_SLOTS {
        let code_col = uom_code_column(slot);
        let op_col = uom_operator_column(slot);
        let factor_col = uom_factor_column(slot);
        let raw_code = cell(values, wb, &code_col);
        if raw_code.is_empty() {
            continue;
        }
        let Some(uom) = resolve_master(&masters.uom, raw_code, &code_col, row, issues) else {
            continue;
        };
        if seen.contains(&uom.id) {
            issues.push(RowIssue::new(
                row,
                Some(&code_col),
                "PRODUCT_UOM_DUPLICATE",
                format!(
                    "{code_col} \"{raw_code}\" is listed more than once (or equals baseUomCode)"
                ),
            ));
            continue;
        }
        seen.insert(uom.id);
        let raw_op = cell(values, wb, &op_col).to_lowercase();
        let operator = if raw_op.is_empty() {
            uom.default_operator
                .clone()
                .unwrap_or_else(|| "multiply".into())
        } else if raw_op == "multiply" || raw_op == "divide" {
            raw_op
        } else {
            issues.push(RowIssue::new(
                row,
                Some(&op_col),
                "PRODUCT_UOM_INVALID_OPERATOR",
                format!("{op_col} must be multiply or divide"),
            ));
            continue;
        };
        let raw_factor = cell(values, wb, &factor_col);
        let factor = if raw_factor.is_empty() {
            uom.default_factor
                .as_ref()
                .map_or_else(|| "1".to_owned(), dec_str)
        } else {
            raw_factor.to_owned()
        };
        let rate = if CONVERSION_RATE.is_match(&factor) {
            compute_base_equivalent(&operator, &factor)
        } else {
            None
        };
        let Some(conversion_rate) = rate else {
            let message = if operator == "divide" {
                format!(
                    "{factor_col} must be a positive number small enough that 1 / factor does not round to 0 at 6 decimal places"
                )
            } else {
                format!("{factor_col} must be a positive number with up to 6 decimal places")
            };
            issues.push(RowIssue::new(
                row,
                Some(&factor_col),
                "PRODUCT_UOM_INVALID_FACTOR",
                message,
            ));
            continue;
        };
        uoms.push(ResolvedImportUom {
            uom_id: uom.id,
            operator,
            factor,
            conversion_rate,
            is_base: false,
        });
    }
    uoms
}

/// `resolveImportRow(input)`.
pub fn resolve_import_row(
    row: usize,
    values: &[String],
    wb: &ParsedImportWorkbook,
    masters: &MasterMaps,
    defs: &[CfDefinition],
    seen_skus: &mut HashSet<String>,
) -> RowResolution {
    let mut issues = Vec::new();
    let get = |c: &str| cell(values, wb, c);

    let sku = get("sku");
    if sku.is_empty() {
        issues.push(required_missing(row, "sku"));
    } else if !PRODUCT_SKU.is_match(sku) {
        issues.push(RowIssue::new(
            row,
            Some("sku"),
            "IMPORT_ROW_INVALID_SKU",
            "sku must be 1-100 letters, digits, dot, underscore, or hyphen".into(),
        ));
    } else {
        if seen_skus.contains(sku) {
            issues.push(RowIssue::new(
                row,
                Some("sku"),
                "PRODUCT_IMPORT_DUPLICATE_SKU_IN_FILE",
                format!("sku \"{sku}\" appears more than once in this file"),
            ));
        }
        seen_skus.insert(sku.to_owned());
    }

    let name = get("name");
    if name.is_empty() {
        issues.push(required_missing(row, "name"));
    }

    let raw_category = get("categoryCode");
    if raw_category.is_empty() {
        issues.push(required_missing(row, "categoryCode"));
    }
    let category = resolve_master(
        &masters.category,
        raw_category,
        "categoryCode",
        row,
        &mut issues,
    );
    let sub = resolve_master(
        &masters.sub_category,
        get("subCategoryCode"),
        "subCategoryCode",
        row,
        &mut issues,
    );
    if let (Some(c), Some(s)) = (category, sub)
        && s.category_id != Some(c.id)
    {
        issues.push(RowIssue::new(
            row,
            Some("subCategoryCode"),
            "SUBCATEGORY_CATEGORY_MISMATCH",
            "subCategoryCode does not belong to categoryCode".into(),
        ));
    }
    let brand = resolve_master(
        &masters.brand,
        get("brandCode"),
        "brandCode",
        row,
        &mut issues,
    );
    let principal = resolve_master(
        &masters.principal,
        get("principalCode"),
        "principalCode",
        row,
        &mut issues,
    );
    let tax = resolve_master(&masters.tax, get("taxCode"), "taxCode", row, &mut issues);

    let raw_base = get("baseUomCode");
    if raw_base.is_empty() {
        issues.push(required_missing(row, "baseUomCode"));
    }
    let base = resolve_master(&masters.uom, raw_base, "baseUomCode", row, &mut issues);

    let weight = parse_decimal(get("weight"), "weight", row, &mut issues);
    let volume = parse_decimal(get("volume"), "volume", row, &mut issues);
    let image_url = Some(get("imageUrl"))
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let is_orderable = parse_bool_cell(get("isOrderable"), "isOrderable", row, true, &mut issues);
    let is_active = parse_bool_cell(get("isActive"), "isActive", row, true, &mut issues);

    let raw_barcode = get("barcode");
    let barcode_ok = BARCODE.is_match(raw_barcode);
    if !raw_barcode.is_empty() && !barcode_ok {
        issues.push(RowIssue::new(
            row,
            Some("barcode"),
            "IMPORT_ROW_INVALID_BARCODE",
            "barcode must be 1-100 letters/digits".into(),
        ));
    }
    let barcode = (barcode_ok && !raw_barcode.is_empty()).then(|| raw_barcode.to_owned());

    let custom_fields = resolve_custom_fields(values, wb, defs, row, &mut issues);
    let uoms = match base {
        Some(b) => resolve_uoms(values, wb, masters, b, row, &mut issues),
        None => Vec::new(),
    };

    if !issues.is_empty() {
        return RowResolution { data: None, issues };
    }
    let (Some(category), Some(base)) = (category, base) else {
        return RowResolution { data: None, issues };
    };
    let description = Some(get("description"))
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    RowResolution {
        issues,
        data: Some(ResolvedImportRow {
            row_number: row,
            sku: sku.to_owned(),
            name: name.to_owned(),
            description,
            category_id: category.id,
            sub_category_id: sub.map(|s| s.id),
            brand_id: brand.map(|s| s.id),
            principal_id: principal.map(|s| s.id),
            base_uom_id: base.id,
            tax_rate_id: tax.map(|s| s.id),
            weight,
            volume,
            image_url,
            is_orderable,
            is_active,
            barcode,
            custom_fields,
            uoms,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::custom_fields_validator::CfOption;
    use crate::import::columns::build_import_headers;
    use chrono::NaiveDateTime;
    use rust_decimal::Decimal;
    use serde_json::json;
    use std::str::FromStr;

    fn master(id: i32, code: &str) -> MasterRow {
        MasterRow {
            id,
            uuid: format!("uuid-{id}"),
            company_id: 1,
            code: code.into(),
            name: code.into(),
            is_active: true,
            created_at: NaiveDateTime::default(),
            updated_at: None,
            description: None,
            sort_order: None,
            category_id: None,
            contact_person: None,
            phone: None,
            email: None,
            address: None,
            uom_type: None,
            default_operator: None,
            default_factor: None,
            rate: None,
            is_inclusive: None,
        }
    }

    fn masters() -> MasterMaps {
        let m = |rows: Vec<MasterRow>| rows.into_iter().map(|r| (r.code.clone(), r)).collect();
        let mut soft = master(20, "SOFT");
        soft.category_id = Some(10);
        let mut snack = master(21, "SNACK");
        snack.category_id = Some(999);
        let mut boxu = master(51, "BOX");
        boxu.default_operator = Some("multiply".into());
        boxu.default_factor = Decimal::from_str("24.000000").ok();
        let mut drum = master(52, "DRUM");
        drum.default_operator = Some("divide".into());
        drum.default_factor = Decimal::from_str("4").ok();
        let mut ppn = master(60, "PPN");
        ppn.is_active = false;
        MasterMaps {
            category: m(vec![master(10, "BEV")]),
            sub_category: m(vec![soft, snack]),
            brand: m(vec![master(30, "ABC")]),
            principal: m(vec![master(40, "PT-XYZ")]),
            uom: m(vec![master(50, "PCS"), boxu, drum]),
            tax: m(vec![ppn]),
        }
    }

    fn defs() -> Vec<CfDefinition> {
        let d = |k: &str, t: &str| CfDefinition {
            key: k.into(),
            field_type: t.into(),
            is_required: false,
            is_active: true,
            options: None,
        };
        let mut flavors = d("flavors", "multiselect");
        flavors.options = Some(vec![
            CfOption {
                value: "lime".into(),
                label: "Lime".into(),
            },
            CfOption {
                value: "orange".into(),
                label: "Orange".into(),
            },
        ]);
        vec![
            d("shelf_life_days", "number"),
            d("is_frozen", "boolean"),
            flavors,
        ]
    }

    fn headers() -> Vec<String> {
        build_import_headers(&[
            "shelf_life_days".into(),
            "is_frozen".into(),
            "flavors".into(),
        ])
    }

    fn row(over: &[(&str, &str)]) -> Vec<String> {
        let mut base: HashMap<&str, &str> = HashMap::from([
            ("sku", "SKU-1"),
            ("name", "Product 1"),
            ("categoryCode", "BEV"),
            ("baseUomCode", "PCS"),
        ]);
        for (k, v) in over {
            base.insert(k, v);
        }
        headers()
            .iter()
            .map(|h| base.get(h.as_str()).copied().unwrap_or("").to_owned())
            .collect()
    }

    fn resolve_with(values: Vec<String>, seen: &mut HashSet<String>) -> RowResolution {
        let h = headers();
        let refs: Vec<&str> = h.iter().map(String::as_str).collect();
        let wb = ParsedImportWorkbook::from_headers(&refs);
        resolve_import_row(2, &values, &wb, &masters(), &defs(), seen)
    }

    fn resolve(over: &[(&str, &str)]) -> RowResolution {
        resolve_with(row(over), &mut HashSet::new())
    }

    fn has(r: &RowResolution, column: Option<&str>, code: &str) -> bool {
        r.issues
            .iter()
            .any(|i| i.code == code && column.is_none_or(|c| i.column.as_deref() == Some(c)))
    }

    fn uom(id: i32, op: &str, factor: &str, rate: &str, base: bool) -> ResolvedImportUom {
        ResolvedImportUom {
            uom_id: id,
            operator: op.into(),
            factor: factor.into(),
            conversion_rate: rate.into(),
            is_base: base,
        }
    }

    #[test]
    fn baris_minimal() {
        let r = resolve(&[]);
        let d = r.data.unwrap();
        assert_eq!(d.sku, "SKU-1");
        assert_eq!(d.category_id, 10);
        assert_eq!(d.base_uom_id, 50);
        assert_eq!(d.uoms, vec![uom(50, "multiply", "1", "1", true)]);
        assert!(d.custom_fields.is_empty());
    }

    #[test]
    fn kolom_wajib() {
        for col in ["sku", "name", "categoryCode", "baseUomCode"] {
            let r = resolve(&[(col, "")]);
            assert!(r.data.is_none());
            assert!(has(&r, Some(col), "IMPORT_ROW_MISSING_REQUIRED"), "{col}");
        }
    }

    #[test]
    fn kode_master() {
        assert!(has(
            &resolve(&[("categoryCode", "NOPE")]),
            Some("categoryCode"),
            "PRODUCT_MASTER_NOT_FOUND"
        ));
        assert!(has(
            &resolve(&[("taxCode", "PPN")]),
            Some("taxCode"),
            "PRODUCT_MASTER_ARCHIVED"
        ));
        assert!(has(
            &resolve(&[("subCategoryCode", "SNACK")]),
            None,
            "SUBCATEGORY_CATEGORY_MISMATCH"
        ));
        assert_eq!(
            resolve(&[("subCategoryCode", "SOFT")])
                .data
                .unwrap()
                .sub_category_id,
            Some(20)
        );
        assert!(resolve(&[("categoryCode", "bev")]).data.is_some());
    }

    #[test]
    fn sku_ganda() {
        let mut seen = HashSet::new();
        assert!(resolve_with(row(&[]), &mut seen).data.is_some());
        let second = resolve_with(row(&[]), &mut seen);
        assert!(has(&second, None, "PRODUCT_IMPORT_DUPLICATE_SKU_IN_FILE"));
        let mut seen = HashSet::new();
        assert!(
            resolve_with(row(&[("categoryCode", "NOPE")]), &mut seen)
                .data
                .is_none()
        );
        let second = resolve_with(row(&[]), &mut seen);
        assert!(has(&second, None, "PRODUCT_IMPORT_DUPLICATE_SKU_IN_FILE"));
    }

    #[test]
    fn uom_tambahan() {
        let r = resolve(&[
            ("uom2Code", "BOX"),
            ("uom2Operator", "multiply"),
            ("uom2Factor", "12"),
        ]);
        assert!(
            r.data
                .unwrap()
                .uoms
                .contains(&uom(51, "multiply", "12", "12", false))
        );
        let r = resolve(&[
            ("uom2Code", "BOX"),
            ("uom2Operator", "DIVIDE"),
            ("uom2Factor", "3"),
        ]);
        assert!(
            r.data
                .unwrap()
                .uoms
                .contains(&uom(51, "divide", "3", "0.333333", false))
        );
        let r = resolve(&[("uom2Code", "BOX")]);
        assert!(
            r.data
                .unwrap()
                .uoms
                .contains(&uom(51, "multiply", "24", "24", false))
        );
        let r = resolve(&[("uom2Code", "DRUM"), ("uom2Factor", "8")]);
        assert!(
            r.data
                .unwrap()
                .uoms
                .contains(&uom(52, "divide", "8", "0.125", false))
        );
        for f in ["0", "-1", "abc", "1.1234567"] {
            let r = resolve(&[("uom2Code", "BOX"), ("uom2Factor", f)]);
            assert!(
                has(&r, Some("uom2Factor"), "PRODUCT_UOM_INVALID_FACTOR"),
                "{f}"
            );
        }
        let r = resolve(&[
            ("uom2Code", "BOX"),
            ("uom2Operator", "divide"),
            ("uom2Factor", "10000000"),
        ]);
        assert!(has(&r, Some("uom2Factor"), "PRODUCT_UOM_INVALID_FACTOR"));
        let r = resolve(&[
            ("uom2Code", "BOX"),
            ("uom2Operator", "times"),
            ("uom2Factor", "12"),
        ]);
        assert!(has(
            &r,
            Some("uom2Operator"),
            "PRODUCT_UOM_INVALID_OPERATOR"
        ));
        let r = resolve(&[
            ("uom2Code", "BOX"),
            ("uom2Factor", "0"),
            ("uom3Code", "BOX"),
            ("uom3Factor", "12"),
        ]);
        assert!(has(&r, Some("uom3Code"), "PRODUCT_UOM_DUPLICATE"));
        let r = resolve(&[("uom2Code", "PCS"), ("uom2Factor", "1")]);
        assert!(has(&r, Some("uom2Code"), "PRODUCT_UOM_DUPLICATE"));
    }

    #[test]
    fn custom_field() {
        let r = resolve(&[
            ("cf:shelf_life_days", "365"),
            ("cf:is_frozen", "yes"),
            ("cf:flavors", "lime,orange"),
        ]);
        assert_eq!(
            Value::Object(r.data.unwrap().custom_fields),
            json!({"shelf_life_days": 365, "is_frozen": true, "flavors": ["lime", "orange"]})
        );
        let r = resolve(&[("cf:shelf_life_days", "not-a-number")]);
        assert!(has(
            &r,
            Some("cf:shelf_life_days"),
            "CUSTOM_FIELD_TYPE_MISMATCH"
        ));
        let r = resolve(&[("cf:flavors", "lime,grape")]);
        assert!(has(&r, Some("cf:flavors"), "CUSTOM_FIELD_INVALID_OPTION"));
    }

    #[test]
    fn bool_desimal_gambar_barcode() {
        let d = resolve(&[]).data.unwrap();
        assert!(d.is_orderable && d.is_active);
        let d = resolve(&[("isOrderable", "no"), ("isActive", "0")])
            .data
            .unwrap();
        assert!(!d.is_orderable && !d.is_active);
        assert!(has(
            &resolve(&[("isActive", "maybe")]),
            Some("isActive"),
            "IMPORT_ROW_INVALID_BOOLEAN"
        ));
        assert!(has(
            &resolve(&[("weight", "-5")]),
            Some("weight"),
            "IMPORT_ROW_INVALID_DECIMAL"
        ));
        assert_eq!(
            resolve(&[("imageUrl", "https://example.com/x.jpg")])
                .data
                .unwrap()
                .image_url
                .as_deref(),
            Some("https://example.com/x.jpg")
        );
        assert!(has(
            &resolve(&[("barcode", "bad barcode!")]),
            Some("barcode"),
            "IMPORT_ROW_INVALID_BARCODE"
        ));
        assert_eq!(
            resolve(&[("barcode", "8991234567890")])
                .data
                .unwrap()
                .barcode
                .as_deref(),
            Some("8991234567890")
        );
    }
}
