//! Pembantu validasi gaya class-validator + `class-transformer` yang dipakai DTO modul ini.
//!
//! Body/query diterima sebagai JSON mentah ([`Raw`]) lalu divalidasi manual supaya urutan &
//! bunyi pesan identik dengan `ValidationPipe({ transform: true })` Nest (dekorator dievaluasi
//! dari BAWAH ke ATAS, mis. `@IsInt() @Min(50) @Max(1000)` → Max, Min, IsInt).

use mz_core::extract::{Validate, validator_js_length};
use mz_core::jsfmt::{num_string, parse_int};
use serde::Deserialize;
use serde_json::{Map, Value};

/// Body/query mentah (objek JSON). Validasi dilakukan manual oleh pemanggil.
#[derive(Debug, Deserialize)]
#[serde(transparent)]
pub struct Raw(pub Map<String, Value>);

impl Validate for Raw {}

/// Nilai properti DTO: tidak dikirim / `null` / bernilai.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Field<T> {
    #[default]
    Absent,
    Null,
    Val(T),
}

impl<T> Field<T> {
    /// `x !== undefined`.
    pub fn is_defined(&self) -> bool {
        !matches!(self, Field::Absent)
    }
}

/// `String(value)` JS untuk nilai JSON (dipakai `parseInt`/`parseFloat` atas input mentah).
pub fn js_to_string(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.as_f64().map_or_else(|| n.to_string(), num_string),
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .map(|x| match x {
                Value::Null => String::new(),
                other => js_to_string(other),
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".into(),
    }
}

/// `parseInt(value, 10)`; `None` = NaN.
pub fn parse_int_value(v: &Value) -> Option<i64> {
    parse_int(&js_to_string(v))
}

/// `parseFloat(str)`; NaN bila tak ada awalan angka.
pub fn parse_float(s: &str) -> f64 {
    let t = s.trim_start();
    let b = t.as_bytes();
    let mut i = 0;
    if matches!(b.first(), Some(b'+' | b'-')) {
        i += 1;
    }
    let sign_neg = matches!(b.first(), Some(b'-'));
    if t.get(i..).is_some_and(|r| r.starts_with("Infinity")) {
        return if sign_neg {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        };
    }
    let digits_start = i;
    while b.get(i).is_some_and(u8::is_ascii_digit) {
        i += 1;
    }
    let mut int_digits = i - digits_start;
    if b.get(i) == Some(&b'.') {
        let mut j = i + 1;
        while b.get(j).is_some_and(u8::is_ascii_digit) {
            j += 1;
        }
        let frac = j - i - 1;
        if int_digits > 0 || frac > 0 {
            int_digits += frac;
            i = j;
        }
    }
    if int_digits == 0 {
        return f64::NAN;
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        let mut j = i + 1;
        if matches!(b.get(j), Some(b'+' | b'-')) {
            j += 1;
        }
        let es = j;
        while b.get(j).is_some_and(u8::is_ascii_digit) {
            j += 1;
        }
        if j > es {
            i = j;
        }
    }
    t.get(..i)
        .and_then(|p| p.parse::<f64>().ok())
        .unwrap_or(f64::NAN)
}

/// `typeof v === 'number'` dan hingga (`@IsNumber()` default: tanpa NaN/Infinity).
pub fn as_number(v: Option<&Value>) -> Option<f64> {
    match v {
        Some(Value::Number(n)) => n.as_f64().filter(|x| x.is_finite()),
        _ => None,
    }
}

/// `@IsNumber() @Min(min) @Max(max)` (pesan: Max, Min, IsNumber). `None` = bukan angka.
pub fn check_number(
    errs: &mut Vec<String>,
    field: &str,
    num: Option<f64>,
    min: Option<f64>,
    max: Option<f64>,
) -> Option<f64> {
    let before = errs.len();
    if let Some(mx) = max
        && num.is_none_or(|n| n > mx)
    {
        errs.push(format!(
            "{field} must not be greater than {}",
            num_string(mx)
        ));
    }
    if let Some(mn) = min
        && num.is_none_or(|n| n < mn)
    {
        errs.push(format!("{field} must not be less than {}", num_string(mn)));
    }
    if num.is_none() {
        errs.push(format!(
            "{field} must be a number conforming to the specified constraints"
        ));
    }
    num.filter(|_| errs.len() == before)
}

/// `@IsString()` pada nilai yang wajib/ada.
pub fn check_string(errs: &mut Vec<String>, field: &str, v: Option<&Value>) -> Option<String> {
    if let Some(Value::String(s)) = v {
        Some(s.clone())
    } else {
        errs.push(format!("{field} must be a string"));
        None
    }
}

/// `@IsBoolean()`.
pub fn check_bool(errs: &mut Vec<String>, field: &str, v: Option<&Value>) -> Option<bool> {
    if let Some(Value::Bool(b)) = v {
        Some(*b)
    } else {
        errs.push(format!("{field} must be a boolean value"));
        None
    }
}

/// Nilai ada & bukan `null` (`@IsOptional()` melewati `null`/`undefined`).
pub fn present<'a>(m: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    m.get(key).filter(|v| !v.is_null())
}

/// Apakah properti divalidasi: `required` (DTO create) → selalu; opsional (`@IsOptional()`/`PartialType`)
/// → hanya bila ada dan bukan `null`. `None` = lewati; `Some(v)` = validasi `v` (`None` = hilang).
pub fn gate<'a>(m: &'a Map<String, Value>, key: &str, required: bool) -> Option<Option<&'a Value>> {
    if required {
        Some(m.get(key))
    } else {
        present(m, key).map(Some)
    }
}

/// `page`/`limit` query (`@Transform(parseInt)`; `page`: `@IsInt() @Min(1)`, `limit`: `@IsInt() @Min(1)
/// @Max(100)`); bawaan 1 dan 20. Pesan: page → Min, IsInt; limit → Max, Min, IsInt.
pub fn page_limit(m: &Map<String, Value>, errs: &mut Vec<String>) -> (i64, i64) {
    let (mut page, mut limit) = (1, 20);
    if let Some(v) = m.get("page") {
        let n = parse_int_value(v);
        if n.is_none_or(|n| n < 1) {
            errs.push("page must not be less than 1".into());
        }
        if n.is_none() {
            errs.push("page must be an integer number".into());
        }
        if let Some(n) = n {
            page = n;
        }
    }
    if let Some(v) = m.get("limit") {
        #[allow(clippy::cast_precision_loss)]
        let n = parse_int_value(v).map(|n| n as f64);
        if let Some(i) = mz_core::extract::check_int_range(errs, "limit", n, 1, 100) {
            limit = i;
        }
    }
    (page, limit)
}

/// Pesan bawaan `@Length(min, max)` — satu implementasi di `mz_core::extract`.
pub use mz_core::extract::length_message;

/// `@IsString() @Length(min, max)` (pesan Length lebih dulu, lalu IsString).
pub fn check_length_string(
    errs: &mut Vec<String>,
    field: &str,
    v: Option<&Value>,
    min: usize,
    max: usize,
) -> Option<String> {
    let s = match v {
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    };
    let ok = s
        .as_deref()
        .is_some_and(|s| (min..=max).contains(&validator_js_length(s)));
    if !ok {
        errs.push(length_message(field, v, min, max));
    }
    if s.is_none() {
        errs.push(format!("{field} must be a string"));
    }
    s.filter(|_| ok)
}

/// `@IsString() @MaxLength(max)` (pesan MaxLength lebih dulu, lalu IsString).
pub fn check_max_string(
    errs: &mut Vec<String>,
    field: &str,
    v: Option<&Value>,
    max: usize,
) -> Option<String> {
    let s = match v {
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    };
    let ok = s.as_deref().is_some_and(|s| validator_js_length(s) <= max);
    if !ok {
        errs.push(format!(
            "{field} must be shorter than or equal to {max} characters"
        ));
    }
    if s.is_none() {
        errs.push(format!("{field} must be a string"));
    }
    s.filter(|_| ok)
}

/// `@Transform(({value}) => value === 'true' ? true : value === 'false' ? false : value)`.
pub fn to_boolean(v: &Value) -> Value {
    match v {
        Value::String(s) if s == "true" => Value::Bool(true),
        Value::String(s) if s == "false" => Value::Bool(false),
        other => other.clone(),
    }
}

/// Bagian bulat dari angka yang lolos `@IsInt`, ke `i32` (Prisma `Int`); di luar rentang → `None`.
pub fn to_i32(n: i64) -> Option<i32> {
    i32::try_from(n).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_float_prefix() {
        assert_eq!(parse_float("12.5abc"), 12.5);
        assert_eq!(parse_float("  -6.2e1x"), -62.0);
        assert_eq!(parse_float(".5"), 0.5);
        assert!(parse_float("abc").is_nan());
        assert!(parse_float("").is_nan());
        assert!(parse_float("-").is_nan());
        assert_eq!(parse_float("1e"), 1.0);
        assert_eq!(parse_float("-Infinity"), f64::NEG_INFINITY);
    }

    #[test]
    fn number_messages_in_decorator_order() {
        let mut e = Vec::new();
        check_number(&mut e, "lat", None, Some(-90.0), Some(90.0));
        assert_eq!(
            e,
            vec![
                "lat must not be greater than 90",
                "lat must not be less than -90",
                "lat must be a number conforming to the specified constraints"
            ]
        );
        let mut e = Vec::new();
        assert_eq!(
            check_number(&mut e, "lat", Some(91.0), Some(-90.0), Some(90.0)),
            None
        );
        assert_eq!(e, vec!["lat must not be greater than 90"]);
    }

    #[test]
    fn string_helpers() {
        let mut e = Vec::new();
        assert!(check_max_string(&mut e, "d", Some(&Value::from(5)), 10).is_none());
        assert_eq!(
            e,
            vec![
                "d must be shorter than or equal to 10 characters",
                "d must be a string"
            ]
        );
        let mut e = Vec::new();
        check_length_string(&mut e, "name", Some(&Value::from("")), 1, 120);
        assert_eq!(e, vec!["name must be longer than or equal to 1 characters"]);
    }

    #[test]
    fn js_string_of_values() {
        assert_eq!(parse_int_value(&Value::from(5.7)), Some(5));
        assert_eq!(parse_int_value(&Value::Null), None);
        assert_eq!(parse_int_value(&serde_json::json!(["7"])), Some(7));
    }
}
