//! Validasi `ValidationPipe` + class-validator 0.14 di atas body JSON mentah.
//!
//! Aturan yang ditiru:
//! - `whitelist + forbidNonWhitelisted` (`ADMIN_VALIDATION_PIPE`): kunci asing dilaporkan lebih
//!   dulu, urutan kemunculan, `property <k> should not exist`.
//! - Per properti (urutan deklarasi kelas; properti kelas turunan sebelum kelas induk):
//!   `@IsOptional`/`@ValidateIf` palsu → lewati; `@IsDefined` dulu, lalu dekorator lain dari
//!   BAWAH ke atas (urutan registrasi metadata), lalu `@ValidateNested`.
//! - Galat anak (`ValidateNested`) diberi awalan `<induk>.` oleh Nest.

use mz_core::ApiError;
use mz_core::extract::validator_js_length;
use mz_core::jsfmt::{num_string, trim as js_trim};
use serde_json::{Map, Value};

pub type Obj = Map<String, Value>;

#[derive(Debug, Default)]
pub struct V {
    pub errors: Vec<String>,
    prefix: String,
}

impl V {
    pub fn new() -> Self {
        Self::default()
    }

    /// Validator anak (`ValidateNested`) dengan awalan `<prefix>.`.
    pub fn nested(prefix: &str) -> Self {
        Self {
            errors: Vec::new(),
            prefix: format!("{prefix}."),
        }
    }

    pub fn finish(self) -> Result<(), ApiError> {
        if self.errors.is_empty() {
            Ok(())
        } else {
            Err(ApiError::Validation(self.errors))
        }
    }

    pub fn push(&mut self, msg: impl AsRef<str>) {
        self.errors.push(format!("{}{}", self.prefix, msg.as_ref()));
    }

    pub fn extend(&mut self, other: V) {
        for e in other.errors {
            self.errors.push(format!("{}{e}", self.prefix));
        }
    }

    /// `forbidNonWhitelisted`.
    pub fn whitelist(&mut self, body: &Obj, allowed: &[&str]) {
        for k in body.keys() {
            if !allowed.contains(&k.as_str()) {
                self.push(format!("property {k} should not exist"));
            }
        }
    }

    // ── dekorator ───────────────────────────────────────────────────────────

    pub fn is_string(&mut self, p: &str, v: Option<&Value>) {
        if !matches!(v, Some(Value::String(_))) {
            self.push(format!("{p} must be a string"));
        }
    }

    pub fn is_boolean(&mut self, p: &str, v: Option<&Value>) {
        if !matches!(v, Some(Value::Bool(_))) {
            self.push(format!("{p} must be a boolean value"));
        }
    }

    pub fn not_empty(&mut self, p: &str, v: Option<&Value>) {
        let empty = match v {
            None | Some(Value::Null) => true,
            Some(Value::String(s)) => s.is_empty(),
            _ => false,
        };
        if empty {
            self.push(format!("{p} should not be empty"));
        }
    }

    /// `@IsDefined`.
    pub fn defined(&mut self, p: &str, v: Option<&Value>) {
        if matches!(v, None | Some(Value::Null)) {
            self.push(format!("{p} should not be null or undefined"));
        }
    }

    pub fn is_int(&mut self, p: &str, v: Option<&Value>) {
        if num_of(v).is_none_or(|n| !n.is_finite() || n.fract() != 0.0) {
            self.push(format!("{p} must be an integer number"));
        }
    }

    pub fn min(&mut self, p: &str, v: Option<&Value>, min: f64) {
        if num_of(v).is_none_or(|n| n.is_nan() || n < min) {
            self.push(format!("{p} must not be less than {}", num_string(min)));
        }
    }

    pub fn max(&mut self, p: &str, v: Option<&Value>, max: f64) {
        if num_of(v).is_none_or(|n| n.is_nan() || n > max) {
            self.push(format!("{p} must not be greater than {}", num_string(max)));
        }
    }

    /// `@Max(max) @Min(min) @IsInt()` ditulis dari atas: pesan Max, Min, IsInt.
    pub fn int_range(&mut self, p: &str, v: Option<&Value>, min: f64, max: f64) {
        self.max(p, v, max);
        self.min(p, v, min);
        self.is_int(p, v);
    }

    pub fn is_positive(&mut self, p: &str, v: Option<&Value>) {
        if num_of(v).is_none_or(|n| n.is_nan() || n <= 0.0) {
            self.push(format!("{p} must be a positive number"));
        }
    }

    /// `@IsNumber({ allowNaN: false, allowInfinity: false, maxDecimalPlaces })`.
    pub fn is_number(&mut self, p: &str, v: Option<&Value>, max_decimals: Option<usize>) {
        let ok = num_of(v)
            .is_some_and(|n| n.is_finite() && max_decimals.is_none_or(|m| decimal_places(n) <= m));
        if !ok {
            self.push(format!(
                "{p} must be a number conforming to the specified constraints"
            ));
        }
    }

    pub fn is_in(&mut self, p: &str, v: Option<&Value>, allowed: &[&str]) {
        let ok = matches!(v, Some(Value::String(s)) if allowed.contains(&s.as_str()));
        if !ok {
            self.push(format!(
                "{p} must be one of the following values: {}",
                allowed.join(", ")
            ));
        }
    }

    pub fn is_uuid(&mut self, p: &str, v: Option<&Value>) {
        let ok = matches!(v, Some(Value::String(s)) if mz_core::uuid_rules::is_uuid(s));
        if !ok {
            self.push(format!("{p} must be a UUID"));
        }
    }

    /// `@IsISO8601()` / `@IsDateString()` (pesannya sama).
    pub fn is_iso(&mut self, p: &str, v: Option<&Value>) {
        if !matches!(v, Some(Value::String(s)) if mz_core::iso8601::is_iso8601(s)) {
            self.push(format!("{p} must be a valid ISO 8601 date string"));
        }
    }

    /// `@IsISO8601({ strict: true })`: format + tanggal kalender sah.
    pub fn is_iso_strict(&mut self, p: &str, v: Option<&Value>) {
        let ok = matches!(v, Some(Value::String(s))
            if mz_core::iso8601::is_iso8601(s) && strict_calendar_ok(s));
        if !ok {
            self.push(format!("{p} must be a valid ISO 8601 date string"));
        }
    }

    pub fn is_email(&mut self, p: &str, v: Option<&Value>) {
        if !matches!(v, Some(Value::String(s)) if mz_core::extract::is_email(s)) {
            self.push(format!("{p} must be an email"));
        }
    }

    pub fn is_time_zone(&mut self, p: &str, v: Option<&Value>) {
        if !matches!(v, Some(Value::String(s)) if is_iana_zone(s)) {
            self.push(format!("{p} must be a valid IANA time-zone"));
        }
    }

    /// `@Length(min, max)` (validator.js `isLength`; pesan bawaan memakai `.length`).
    pub fn length(&mut self, p: &str, v: Option<&Value>, min: usize, max: usize) {
        if let Some(msg) = mz_core::extract::length_error(p, v, min, max) {
            self.push(msg);
        }
    }

    /// `@MaxLength(max)`.
    pub fn max_length(&mut self, p: &str, v: Option<&Value>, max: usize) {
        if !matches!(v, Some(Value::String(s)) if validator_js_length(s) <= max) {
            self.push(format!(
                "{p} must be shorter than or equal to {max} characters"
            ));
        }
    }

    /// `@ReasonDto.reason` = `@IsString() @Length(5, 500)` (pesan Length, IsString).
    pub fn reason(&mut self, v: Option<&Value>) {
        self.length("reason", v, 5, 500);
        self.is_string("reason", v);
    }

    /// `ValidateNested` atas nilai bukan objek (dan bukan `undefined`).
    pub fn nested_not_object(&mut self, p: &str) {
        self.push(format!(
            "nested property {p} must be either object or array"
        ));
    }
}

/// Nilai number JS (bukan string angka).
pub fn num_of(v: Option<&Value>) -> Option<f64> {
    match v {
        Some(Value::Number(n)) => n.as_f64(),
        _ => None,
    }
}

/// Jumlah digit desimal seperti `value.toString().split('.')[1].length`.
pub fn decimal_places(n: f64) -> usize {
    if n.fract() == 0.0 {
        return 0;
    }
    let s = num_string(n);
    s.split_once('.')
        .map_or(0, |(_, d)| d.split(['e', 'E']).next().map_or(0, str::len))
}

/// Kalender sah untuk bentuk `YYYY-MM-DD...` (opsi `strict` validator.js).
fn strict_calendar_ok(s: &str) -> bool {
    let date = s.get(..10);
    match date {
        Some(d)
            if d.len() == 10
                && d.as_bytes().get(4) == Some(&b'-')
                && d.as_bytes().get(7) == Some(&b'-') =>
        {
            chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").is_ok()
        }
        _ => true,
    }
}

/// Zona offset yang diterima ICU Node 22: `±HH`, `±HHMM`, `±HH:MM` (jam ≤ 23, menit ≤ 59).
fn is_offset_zone(s: &str) -> bool {
    let b = s.as_bytes();
    let two = |i: usize, max: u8| {
        matches!(b.get(i..i + 2), Some(&[h, l])
            if h.is_ascii_digit() && l.is_ascii_digit() && (h - b'0') * 10 + (l - b'0') <= max)
    };
    matches!(b.first(), Some(b'+' | b'-'))
        && two(1, 23)
        && match b.len() {
            3 => true,
            5 => two(3, 59),
            6 => b.get(3) == Some(&b':') && two(4, 59),
            _ => false,
        }
}

/// `Intl.DateTimeFormat(undefined, { timeZone })` berhasil (tanpa membedakan huruf besar).
pub fn is_iana_zone(s: &str) -> bool {
    if is_offset_zone(s) || s.parse::<chrono_tz::Tz>().is_ok() {
        return true;
    }
    let lower = s.to_ascii_lowercase();
    chrono_tz::TZ_VARIANTS
        .iter()
        .any(|tz| tz.name().to_ascii_lowercase() == lower)
}

/// `@Transform(({ value }) => typeof value === 'string' ? value.trim() : value)` di tempat.
pub fn trim_field(body: &mut Obj, key: &str) {
    if let Some(Value::String(s)) = body.get_mut(key) {
        let t = js_trim(s).to_owned();
        *s = t;
    }
}

/// Seperti [`trim_field`] lalu huruf kecil (`email`).
pub fn trim_lower_field(body: &mut Obj, key: &str) {
    if let Some(Value::String(s)) = body.get_mut(key) {
        let t = js_trim(s).to_lowercase();
        *s = t;
    }
}

/// `null`/`undefined` (`@IsOptional`).
pub fn absent(v: Option<&Value>) -> bool {
    matches!(v, None | Some(Value::Null))
}

pub fn as_obj(body: &Value) -> Obj {
    match body {
        Value::Object(m) => m.clone(),
        _ => Obj::new(),
    }
}

/// Body array: `ValidationPipe` melaporkan indeksnya sebagai properti asing.
pub fn array_keys(body: &Value) -> Vec<String> {
    match body {
        Value::Array(a) => (0..a.len())
            .map(|i| format!("property {i} should not exist"))
            .collect(),
        _ => Vec::new(),
    }
}

pub fn str_of<'a>(m: &'a Obj, k: &str) -> Option<&'a str> {
    m.get(k).and_then(Value::as_str)
}

pub fn bool_of(m: &Obj, k: &str) -> Option<bool> {
    m.get(k).and_then(Value::as_bool)
}

pub fn i32_of(m: &Obj, k: &str) -> Option<i32> {
    num_of(m.get(k)).and_then(mz_core::jsfmt::as_i32)
}

pub fn f64_of(m: &Obj, k: &str) -> Option<f64> {
    num_of(m.get(k))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reason_messages_like_class_validator() {
        let mut v = V::new();
        v.reason(None);
        assert_eq!(
            v.errors,
            vec![
                "reason must be longer than or equal to 5 characters",
                "reason must be a string"
            ]
        );
        let mut v = V::new();
        v.reason(Some(&json!("x".repeat(501))));
        assert_eq!(
            v.errors,
            vec!["reason must be shorter than or equal to 500 characters"]
        );
        let mut v = V::new();
        v.reason(Some(&json!(12345)));
        assert_eq!(
            v.errors,
            vec![
                "reason must be longer than or equal to 5 and shorter than or equal to 500 characters",
                "reason must be a string"
            ]
        );
    }

    #[test]
    fn nested_prefix_and_whitelist() {
        let mut child = V::nested("payment");
        child.whitelist(
            json!({ "x": 1 }).as_object().unwrap_or(&Obj::new()),
            &["amount"],
        );
        child.is_positive("amount", Some(&json!(-1)));
        assert_eq!(
            child.errors,
            vec![
                "payment.property x should not exist",
                "payment.amount must be a positive number"
            ]
        );
    }

    #[test]
    fn number_rules() {
        assert_eq!(decimal_places(1.25), 2);
        assert_eq!(decimal_places(1.255), 3);
        assert_eq!(decimal_places(30000.0), 0);
        let mut v = V::new();
        v.is_number("amount", Some(&json!(1.255)), Some(2));
        v.max("amount", Some(&json!(2e12)), 1e12);
        v.int_range("maxUsers", Some(&json!("5")), 1.0, 100_000.0);
        assert_eq!(
            v.errors,
            vec![
                "amount must be a number conforming to the specified constraints",
                "amount must not be greater than 1000000000000",
                "maxUsers must not be greater than 100000",
                "maxUsers must not be less than 1",
                "maxUsers must be an integer number",
            ]
        );
    }

    #[test]
    fn time_zone_and_strict_iso() {
        assert!(is_iana_zone("Asia/Jakarta"));
        assert!(is_iana_zone("asia/jakarta"));
        assert!(!is_iana_zone("Mars/Base"));
        let mut v = V::new();
        v.is_iso_strict("trialEndDate", Some(&json!("2026-02-30")));
        v.is_iso_strict("trialEndDate", Some(&json!("2026-02-28T00:00:00Z")));
        assert_eq!(v.errors.len(), 1);
    }
}
