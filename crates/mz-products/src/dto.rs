//! Validator gaya class-validator untuk DTO modul produk (urutan pesan = dekorator dari bawah
//! ke atas, properti = urutan deklarasi, anak `@ValidateNested` = `parent.index.child`).
//! Melengkapi `mz_sales_geo::dto` (dipakai ulang: `check_bool`, `check_length_string`, ...).

use std::sync::LazyLock;

use mz_core::uuid_rules::is_uuid_v4;
pub use mz_sales_geo::dto::{
    Field, Raw, check_bool, check_length_string, check_max_string, check_string, gate,
    parse_int_value, present, to_boolean,
};
use regex::Regex;
use serde_json::{Map, Value};

/// Regex literal (pola Node, `regex` tidak punya look-around jadi semuanya aman).
pub fn re(pattern: &str) -> Regex {
    // Pola di crate ini literal dan teruji (lihat test `pola_regex`).
    Regex::new(pattern).unwrap_or_else(|e| unreachable!("{e}"))
}

pub static PRODUCT_SKU: LazyLock<Regex> =
    LazyLock::new(|| re(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,99}$"));
pub static WEIGHT_VOLUME: LazyLock<Regex> = LazyLock::new(|| re(r"^\d{1,14}\.\d{1,4}$|^\d{1,14}$"));
pub static CONVERSION_RATE: LazyLock<Regex> =
    LazyLock::new(|| re(r"^\d{1,12}\.\d{1,6}$|^\d{1,12}$"));
pub static BARCODE: LazyLock<Regex> = LazyLock::new(|| re(r"^[A-Za-z0-9]{1,100}$"));
pub static MASTER_CODE: LazyLock<Regex> = LazyLock::new(|| re(r"^[A-Z0-9][A-Z0-9_-]{0,31}$"));
pub static TAX_RATE: LazyLock<Regex> = LazyLock::new(|| re(r"^\d{1,4}\.\d{1,4}$|^\d{1,4}$"));
pub static CUSTOM_FIELD_KEY: LazyLock<Regex> = LazyLock::new(|| re(r"^[a-z][a-z0-9_]{0,39}$"));
pub static CURRENCY_CODE: LazyLock<Regex> = LazyLock::new(|| re(r"^[A-Z]{3,10}$"));
pub static DATE_ONLY: LazyLock<Regex> = LazyLock::new(|| re(r"^\d{4}-\d{2}-\d{2}$"));
pub static QTY: LazyLock<Regex> = LazyLock::new(|| re(r"^\d{1,14}\.\d{1,4}$|^\d{1,14}$"));
pub static PRICE: LazyLock<Regex> = LazyLock::new(|| re(r"^\d{1,16}\.\d{1,2}$|^\d{1,16}$"));

/// `@IsString() @Matches(re, { message })` (pesan Matches lebih dulu, lalu IsString).
pub fn check_matches(
    errs: &mut Vec<String>,
    field: &str,
    v: Option<&Value>,
    re: &Regex,
    message: &str,
) -> Option<String> {
    let s = match v {
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    };
    let ok = s.as_deref().is_some_and(|s| re.is_match(s));
    if !ok {
        errs.push(message.to_owned());
    }
    if s.is_none() {
        errs.push(format!("{field} must be a string"));
    }
    s.filter(|_| ok)
}

/// `@IsUUID('4')`.
pub fn check_uuid4(errs: &mut Vec<String>, field: &str, v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::String(s)) if is_uuid_v4(s) => Some(s.clone()),
        _ => {
            errs.push(format!("{field} must be a UUID"));
            None
        }
    }
}

/// `@IsEnum(E)` / `@IsIn([...])`.
pub fn check_enum(
    errs: &mut Vec<String>,
    field: &str,
    v: Option<&Value>,
    values: &[&str],
) -> Option<String> {
    match v {
        Some(Value::String(s)) if values.contains(&s.as_str()) => Some(s.clone()),
        _ => {
            errs.push(format!(
                "{field} must be one of the following values: {}",
                values.join(", ")
            ));
            None
        }
    }
}

/// `@Transform(parseInt) @IsInt() @Min(min)` (pesan Min lalu IsInt). `v` = nilai mentah.
pub fn check_parse_int_min(
    errs: &mut Vec<String>,
    field: &str,
    v: &Value,
    min: i64,
) -> Option<i64> {
    let n = parse_int_value(v);
    if n.is_none_or(|n| n < min) {
        errs.push(format!("{field} must not be less than {min}"));
    }
    if n.is_none() {
        errs.push(format!("{field} must be an integer number"));
    }
    n.filter(|n| *n >= min)
}

/// `@IsInt() @Min(min)` tanpa transform (angka JSON).
pub fn check_int_min(
    errs: &mut Vec<String>,
    field: &str,
    v: Option<&Value>,
    min: i64,
) -> Option<i64> {
    let num = match v {
        Some(Value::Number(n)) => n.as_f64(),
        _ => None,
    };
    #[allow(clippy::cast_precision_loss)]
    if num.is_none_or(|n| n.is_nan() || n < min as f64) {
        errs.push(format!("{field} must not be less than {min}"));
    }
    let int = num.filter(|n| n.is_finite() && n.fract() == 0.0);
    if int.is_none() {
        errs.push(format!("{field} must be an integer number"));
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    int.filter(|n| *n >= min as f64).map(|n| n as i64)
}

/// `@IsObject()`.
pub fn is_object(v: Option<&Value>) -> bool {
    matches!(v, Some(Value::Object(_)))
}

/// Query `page`/`limit` dengan batas `limit` yang bisa diatur (`@Max(max)`).
pub fn page_limit_max(m: &Map<String, Value>, errs: &mut Vec<String>, max: i64) -> (i64, i64) {
    let (mut page, mut limit) = (1, 20);
    if let Some(n) = m
        .get("page")
        .and_then(|v| check_parse_int_min(errs, "page", v, 1))
    {
        page = n;
    }
    if let Some(v) = m.get("limit") {
        #[allow(clippy::cast_precision_loss)]
        let n = parse_int_value(v).map(|n| n as f64);
        if let Some(i) = mz_core::extract::check_int_range(errs, "limit", n, 1, max) {
            limit = i;
        }
    }
    (page, limit)
}

/// `@Transform(toBoolean) @IsOptional() @IsBoolean()` untuk query string.
pub fn query_bool(m: &Map<String, Value>, errs: &mut Vec<String>, key: &str) -> Option<bool> {
    let v = present(m, key)?;
    let v = to_boolean(v);
    check_bool(errs, key, Some(&v))
}

/// `@IsOptional() @IsString() @MaxLength(n)`.
pub fn opt_max_string(
    m: &Map<String, Value>,
    errs: &mut Vec<String>,
    key: &str,
    max: usize,
) -> Option<String> {
    present(m, key).and_then(|v| check_max_string(errs, key, Some(v), max))
}

/// Tiga keadaan properti: absen / `null` / bernilai.
pub fn field_of(m: &Map<String, Value>, key: &str) -> Field<Value> {
    match m.get(key) {
        None => Field::Absent,
        Some(Value::Null) => Field::Null,
        Some(v) => Field::Val(v.clone()),
    }
}

/// `Field<Value>` → `Field<T>` dengan validator untuk nilai.
pub fn map_field<T>(
    f: Field<Value>,
    mut check: impl FnMut(&Value) -> Option<T>,
) -> Option<Field<T>> {
    match f {
        Field::Absent => Some(Field::Absent),
        Field::Null => Some(Field::Null),
        Field::Val(v) => check(&v).map(Field::Val),
    }
}

/// `@ValidateNested({ each: true })` pada array: elemen bukan objek.
pub fn nested_not_object(field: &str) -> String {
    format!("nested property {field} must be either object or array")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pola_regex() {
        assert!(PRODUCT_SKU.is_match("CFE-001"));
        assert!(!PRODUCT_SKU.is_match("-x"));
        assert!(CONVERSION_RATE.is_match("0.000001"));
        assert!(!CONVERSION_RATE.is_match("1.1234567"));
        assert!(TAX_RATE.is_match("11"));
        assert!(TAX_RATE.is_match("11.25"));
        assert!(!TAX_RATE.is_match("-1"));
        assert!(PRICE.is_match("12500.00"));
        assert!(!PRICE.is_match("1.234"));
    }

    #[test]
    fn urutan_pesan() {
        let mut e = Vec::new();
        check_matches(&mut e, "sku", Some(&json!(5)), &PRODUCT_SKU, "sku bad");
        assert_eq!(e, vec!["sku bad", "sku must be a string"]);
        let mut e = Vec::new();
        check_parse_int_min(&mut e, "sortOrder", &json!(null), 0);
        assert_eq!(
            e,
            vec![
                "sortOrder must not be less than 0",
                "sortOrder must be an integer number"
            ]
        );
        let mut e = Vec::new();
        check_enum(&mut e, "sortDir", Some(&json!("x")), &["asc", "desc"]);
        assert_eq!(
            e,
            vec!["sortDir must be one of the following values: asc, desc"]
        );
    }
}
