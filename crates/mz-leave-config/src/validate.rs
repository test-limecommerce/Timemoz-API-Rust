//! Pemeriksa gaya class-validator + class-transformer untuk DTO modul konfigurasi cuti.
//!
//! `ValidationPipe({ transform: true })` (tanpa `whitelist`/`enableImplicitConversion`):
//! `@Type(() => Number)` memanggil `Number(x)` (string `"5"` → 5, `""` → 0, `true` → 1, objek →
//! NaN, `null` tetap `null`; array dipetakan per elemen), lalu dekorator memeriksa hasilnya.
//! Pesan satu properti keluar dalam urutan dekorator dari BAWAH ke atas, properti dalam urutan
//! deklarasi kelas (`mz_overtime::hr::common::dto` memakai kaidah yang sama).

use mz_core::ApiError;
use mz_core::jsfmt;
use serde::{Deserialize, Deserializer};
use serde_json::Value;

/// `deserialize_with` untuk field yang membedakan "tidak dikirim" (`None`) dari `null`
/// (`Some(Value::Null)`). Wajib bersama `#[serde(default)]`.
pub fn present<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(d).map(Some)
}

/// Hasil `@Type(() => Number)`.
#[derive(Debug, Clone, PartialEq)]
pub enum Num {
    /// Properti tidak dikirim (`undefined`).
    Absent,
    Null,
    /// Angka JS (bisa NaN/Infinity).
    Val(f64),
    /// Array: dipetakan per elemen oleh class-transformer.
    Arr(Vec<Num>),
}

impl Num {
    /// `Number(value)` sesuai `TransformOperationExecutor`.
    pub fn of(v: Option<&Value>) -> Self {
        match v {
            None => Self::Absent,
            Some(v) => Self::of_value(v),
        }
    }

    fn of_value(v: &Value) -> Self {
        match v {
            Value::Null => Self::Null,
            Value::Bool(b) => Self::Val(if *b { 1.0 } else { 0.0 }),
            Value::Number(n) => Self::Val(n.as_f64().unwrap_or(f64::NAN)),
            Value::String(s) => Self::Val(jsfmt::number(s)),
            Value::Array(items) => Self::Arr(items.iter().map(Self::of_value).collect()),
            Value::Object(_) => Self::Val(f64::NAN),
        }
    }

    /// `@IsOptional()`: `null`/`undefined` melewati semua pemeriksa properti.
    pub fn skipped(&self) -> bool {
        matches!(self, Self::Absent | Self::Null)
    }

    /// `Number.isInteger`.
    pub fn is_int(&self) -> bool {
        matches!(self, Self::Val(f) if f.is_finite() && f.fract() == 0.0)
    }

    /// `@Min(min)`: `typeof n === 'number' && n >= min`.
    pub fn at_least(&self, min: f64) -> bool {
        matches!(self, Self::Val(f) if *f >= min)
    }

    /// `@Max(max)`.
    pub fn at_most(&self, max: f64) -> bool {
        matches!(self, Self::Val(f) if *f <= max)
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Val(f) => Some(*f),
            _ => None,
        }
    }
}

/// `@IsInt() @Min(min) @Max(max)` (+ `@IsOptional()` bila `optional`): pesan Max, Min, IsInt.
pub fn check_int(
    errors: &mut Vec<String>,
    name: &str,
    n: &Num,
    optional: bool,
    min: Option<f64>,
    max: Option<f64>,
) {
    if optional && n.skipped() {
        return;
    }
    if let Some(max) = max
        && !n.at_most(max)
    {
        errors.push(format!(
            "{name} must not be greater than {}",
            jsfmt::num_string(max)
        ));
    }
    if let Some(min) = min
        && !n.at_least(min)
    {
        errors.push(format!(
            "{name} must not be less than {}",
            jsfmt::num_string(min)
        ));
    }
    if !n.is_int() {
        errors.push(format!("{name} must be an integer number"));
    }
}

/// `@IsBoolean() @IsOptional()`.
pub fn check_bool(errors: &mut Vec<String>, name: &str, v: Option<&Value>) {
    match v {
        None | Some(Value::Null | Value::Bool(_)) => {}
        Some(_) => errors.push(format!("{name} must be a boolean value")),
    }
}

/// `@IsEnum(E) @IsOptional()`: hanya string yang persis salah satu nilai.
pub fn check_enum(errors: &mut Vec<String>, name: &str, v: Option<&Value>, values: &[&str]) {
    match v {
        None | Some(Value::Null) => {}
        Some(Value::String(s)) if values.contains(&s.as_str()) => {}
        Some(_) => errors.push(format!(
            "{name} must be one of the following values: {}",
            values.join(", ")
        )),
    }
}

/// `validator.js` `isLength` (dipakai `@MaxLength`): lihat [`mz_core::extract::validator_js_length`].
pub fn max_length_ok(v: Option<&Value>, max: usize) -> bool {
    matches!(v, Some(Value::String(s)) if mz_core::extract::validator_js_length(s) <= max)
}

/// `@IsString()`.
pub fn is_string(v: Option<&Value>) -> bool {
    matches!(v, Some(Value::String(_)))
}

/// `@IsString() @MaxLength(max)` (+ `@IsOptional()` bila `optional`): pesan MaxLength, IsString.
pub fn check_string(
    errors: &mut Vec<String>,
    name: &str,
    v: Option<&Value>,
    optional: bool,
    max: usize,
) {
    if optional && matches!(v, None | Some(Value::Null)) {
        return;
    }
    if !max_length_ok(v, max) {
        errors.push(format!(
            "{name} must be shorter than or equal to {max} characters"
        ));
    }
    if !is_string(v) {
        errors.push(format!("{name} must be a string"));
    }
}

/// Nilai bertipe sesudah validasi: `undefined` / `null` / nilai.
#[derive(Debug, Clone, PartialEq)]
pub enum Tri<T> {
    Absent,
    Null,
    Val(T),
}

impl<T> Tri<T> {
    /// `x !== undefined`.
    pub fn defined(&self) -> bool {
        !matches!(self, Self::Absent)
    }

    /// Nilai bila ada (`null`/`undefined` → `None`), padanan `x ?? fallback`.
    pub fn val(&self) -> Option<&T> {
        match self {
            Self::Val(v) => Some(v),
            _ => None,
        }
    }

    /// `x ?? null` bila terdefinisi (null dan nilai), `None` bila `undefined`.
    pub fn value_or_null(self) -> Option<Option<T>> {
        match self {
            Self::Absent => None,
            Self::Null => Some(None),
            Self::Val(v) => Some(Some(v)),
        }
    }
}

/// Galat Prisma yang di Node menjadi 500 `Internal server error` (argumen tak sah, kolom
/// non-null diisi `null`, integer di luar Int32, NaN).
pub fn node_500(why: &str) -> ApiError {
    ApiError::internal(anyhow::anyhow!("{why}"))
}

/// Kolom `Int` Prisma: pecahan dipotong, di luar Int32 → galat (500).
pub fn tri_int(v: Option<&Value>, field: &str) -> Result<Tri<i32>, ApiError> {
    match Num::of(v) {
        Num::Absent => Ok(Tri::Absent),
        Num::Null => Ok(Tri::Null),
        Num::Val(f) => jsfmt::as_i32(f)
            .map(Tri::Val)
            .ok_or_else(|| node_500(&format!("{field}: bukan Int32"))),
        Num::Arr(_) => Err(node_500(&format!("{field}: array"))),
    }
}

/// `Float` Prisma.
pub fn tri_f64(v: Option<&Value>, field: &str) -> Result<Tri<f64>, ApiError> {
    match Num::of(v) {
        Num::Absent => Ok(Tri::Absent),
        Num::Null => Ok(Tri::Null),
        Num::Val(f) if f.is_finite() => Ok(Tri::Val(f)),
        _ => Err(node_500(&format!("{field}: bukan Float"))),
    }
}

pub fn tri_bool(v: Option<&Value>) -> Tri<bool> {
    match v {
        None => Tri::Absent,
        Some(Value::Bool(b)) => Tri::Val(*b),
        Some(_) => Tri::Null,
    }
}

pub fn tri_string(v: Option<&Value>) -> Tri<String> {
    match v {
        None => Tri::Absent,
        Some(Value::String(s)) => Tri::Val(s.clone()),
        Some(_) => Tri::Null,
    }
}

/// Enum string (sudah divalidasi oleh [`check_enum`]).
pub fn tri_enum<E: std::str::FromStr>(v: Option<&Value>) -> Tri<E> {
    match v {
        None => Tri::Absent,
        Some(Value::String(s)) => s.parse().map_or(Tri::Null, Tri::Val),
        Some(_) => Tri::Null,
    }
}

/// `ParseIntPipe`: string numerik (`isNumeric`) → `parseInt(v, 10)`; bukan numerik → 400
/// `Validation failed (numeric string is expected)`. `Ok(None)` = `parseInt` menghasilkan NaN.
pub fn parse_int_pipe(raw: &str) -> Result<Option<i64>, ApiError> {
    let t = jsfmt::trim(raw);
    let numeric = jsfmt::number(raw).is_finite() && {
        // `parseFloat(raw)` bukan NaN: awalan angka (tanda opsional, titik desimal).
        let s = t.strip_prefix(['+', '-']).unwrap_or(t);
        let mut chars = s.chars();
        match chars.next() {
            Some(c) if c.is_ascii_digit() => true,
            Some('.') => chars.next().is_some_and(|c| c.is_ascii_digit()),
            _ => false,
        }
    };
    if !numeric {
        return Err(ApiError::bad_request(
            "Validation failed (numeric string is expected)",
        ));
    }
    Ok(jsfmt::parse_int(raw))
}

/// `parseInt(s, 10)` dipakai sebagai nilai kolom `Int` Prisma: NaN/di luar Int32 → 500.
pub fn db_id(parsed: Option<i64>) -> Result<i32, ApiError> {
    parsed
        .and_then(|n| i32::try_from(n).ok())
        .ok_or_else(|| node_500("id: NaN atau di luar Int32"))
}

/// `db_id(parseInt(raw, 10))`.
pub fn parse_id(raw: &str) -> Result<i32, ApiError> {
    db_id(jsfmt::parse_int(raw))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn number_coercion() {
        assert_eq!(Num::of(Some(&json!("5"))), Num::Val(5.0));
        assert_eq!(Num::of(Some(&json!(""))), Num::Val(0.0));
        assert_eq!(Num::of(Some(&json!(true))), Num::Val(1.0));
        assert_eq!(Num::of(Some(&Value::Null)), Num::Null);
        assert!(matches!(Num::of(Some(&json!("abc"))), Num::Val(f) if f.is_nan()));
        assert!(matches!(Num::of(Some(&json!({}))), Num::Val(f) if f.is_nan()));
        assert_eq!(
            Num::of(Some(&json!(["1", 2]))),
            Num::Arr(vec![Num::Val(1.0), Num::Val(2.0)])
        );
        assert_eq!(Num::of(None), Num::Absent);
    }

    #[test]
    fn int_messages_follow_decorator_order() {
        let mut e = Vec::new();
        check_int(
            &mut e,
            "carryOverExpiryMonth",
            &Num::Val(13.5),
            true,
            Some(1.0),
            Some(12.0),
        );
        assert_eq!(
            e,
            [
                "carryOverExpiryMonth must not be greater than 12",
                "carryOverExpiryMonth must be an integer number"
            ]
        );
        let mut e = Vec::new();
        check_int(
            &mut e,
            "annualLeaveDays",
            &Num::Absent,
            false,
            Some(0.0),
            None,
        );
        assert_eq!(
            e,
            [
                "annualLeaveDays must not be less than 0",
                "annualLeaveDays must be an integer number"
            ]
        );
        let mut e = Vec::new();
        check_int(&mut e, "x", &Num::Null, true, Some(0.0), None);
        assert!(e.is_empty());
        check_int(&mut e, "x", &Num::Val(f64::NAN), true, Some(0.0), None);
        assert_eq!(e.len(), 2);
    }

    #[test]
    fn string_and_enum_messages() {
        let mut e = Vec::new();
        check_string(&mut e, "name", None, false, 100);
        assert_eq!(
            e,
            [
                "name must be shorter than or equal to 100 characters",
                "name must be a string"
            ]
        );
        let mut e = Vec::new();
        check_enum(&mut e, "sickLeaveMode", Some(&json!("X")), &["A", "B"]);
        assert_eq!(
            e,
            ["sickLeaveMode must be one of the following values: A, B"]
        );
        let mut e = Vec::new();
        check_bool(&mut e, "isActive", Some(&json!("true")));
        assert_eq!(e, ["isActive must be a boolean value"]);
    }

    #[test]
    fn parse_int_pipe_rules() {
        assert_eq!(parse_int_pipe("12").ok(), Some(Some(12)));
        assert_eq!(parse_int_pipe(" 12 ").ok(), Some(Some(12)));
        assert_eq!(parse_int_pipe("1.5").ok(), Some(Some(1)));
        assert_eq!(parse_int_pipe(".5").ok(), Some(None));
        assert!(parse_int_pipe("abc").is_err());
        assert!(parse_int_pipe("").is_err());
        assert!(parse_int_pipe("12abc").is_err());
        assert!(parse_int_pipe("employees").is_err());
        assert_eq!(db_id(Some(5)).ok(), Some(5));
        assert!(db_id(None).is_err());
        assert!(db_id(Some(3_000_000_000)).is_err());
    }
}
