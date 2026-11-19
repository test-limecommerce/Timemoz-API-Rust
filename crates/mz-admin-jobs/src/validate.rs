//! `ADMIN_VALIDATION_PIPE` (`src/admin/admin-validation.pipe.ts`: `whitelist` +
//! `forbidNonWhitelisted` + `transform`) untuk DTO operator crate ini.
//!
//! Urutan pesan = class-validator 0.14 di bawah `ValidationPipe` Nest: properti asing lebih
//! dulu (urutan kunci), lalu per properti menurut urutan deklarasi (properti kelas sendiri
//! sebelum properti warisan, mis. `reason` dari `ReasonDto` paling akhir), dan dalam satu
//! properti dekorator dari BAWAH ke atas. Diverifikasi terhadap `ValidationPipe` + DTO Node
//! asli (`tests/golden/validation.json`).

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use mz_core::ApiError;
use mz_core::extract::validator_js_length;
use mz_core::jsfmt::{number as js_number, trim as js_trim};
use mz_core::qs::QVal;
use serde_json::Value;

/// Pengumpul pesan validasi (`ValidationError.constraints`, diratakan).
#[derive(Debug, Default)]
pub struct Errors(pub Vec<String>);

impl Errors {
    pub fn push(&mut self, msg: impl Into<String>) {
        self.0.push(msg.into());
    }

    pub fn into_result(self) -> Result<(), ApiError> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(ApiError::Validation(self.0))
        }
    }

    /// `forbidNonWhitelisted` untuk body JSON: kunci objek di luar `allowed`; body array →
    /// setiap indeks.
    pub fn unknown_body(&mut self, body: &Value, allowed: &[&str]) {
        match body {
            Value::Object(m) => {
                for k in m.keys() {
                    if !allowed.contains(&k.as_str()) {
                        self.push(format!("property {k} should not exist"));
                    }
                }
            }
            Value::Array(a) => {
                for i in 0..a.len() {
                    self.push(format!("property {i} should not exist"));
                }
            }
            _ => {}
        }
    }

    /// `forbidNonWhitelisted` untuk query (`qs`).
    pub fn unknown_query(&mut self, q: &[(String, QVal)], allowed: &[&str]) {
        for (k, _) in q {
            if !allowed.contains(&k.as_str()) {
                self.push(format!("property {k} should not exist"));
            }
        }
    }

    /// `@IsUUID()` (versi `all`).
    pub fn uuid(&mut self, field: &str, v: Option<&Value>) {
        if !v
            .and_then(Value::as_str)
            .is_some_and(mz_core::uuid_rules::is_uuid)
        {
            self.push(format!("{field} must be a UUID"));
        }
    }

    /// `@IsBoolean()`.
    pub fn boolean(&mut self, field: &str, v: Option<&Value>) {
        if !matches!(v, Some(Value::Bool(_))) {
            self.push(format!("{field} must be a boolean value"));
        }
    }

    /// `@IsIn(values)` / `@IsEnum(enum)`.
    pub fn one_of(&mut self, field: &str, v: Option<&Value>, values: &[&str]) {
        if !v
            .and_then(Value::as_str)
            .is_some_and(|s| values.contains(&s))
        {
            self.push(format!(
                "{field} must be one of the following values: {}",
                values.join(", ")
            ));
        }
    }

    /// `@IsString()`.
    pub fn string(&mut self, field: &str, v: Option<&Value>) {
        if !matches!(v, Some(Value::String(_))) {
            self.push(format!("{field} must be a string"));
        }
    }

    /// `@Length(min, max)` (pesan bawaan memakai `value.length` UTF-16; validitas memakai
    /// panjang validator.js).
    pub fn length(&mut self, field: &str, v: Option<&Value>, min: usize, max: usize) {
        if let Some(msg) = mz_core::extract::length_error(field, v, min, max) {
            self.push(msg);
        }
    }

    /// `@MaxLength(max)`.
    pub fn max_length(&mut self, field: &str, v: Option<&Value>, max: usize) {
        if !matches!(v, Some(Value::String(s)) if validator_js_length(s) <= max) {
            self.push(format!(
                "{field} must be shorter than or equal to {max} characters"
            ));
        }
    }

    /// `@Matches(re)` dengan pesan bawaan (`<re>` = teks `RegExp.toString()`).
    pub fn matches(&mut self, field: &str, v: Option<&Value>, re: &str, ok: fn(&str) -> bool) {
        if !v.and_then(Value::as_str).is_some_and(ok) {
            self.push(format!("{field} must match {re} regular expression"));
        }
    }

    /// `@IsInt() @Min(min) @Max(max)` untuk angka JSON (tanpa `@Type`): pesan Max, Min, IsInt.
    pub fn int_range(&mut self, field: &str, v: Option<&Value>, min: i64, max: i64) -> Option<i64> {
        let num = v
            .and_then(Value::as_f64)
            .filter(|_| v.is_some_and(Value::is_number));
        mz_core::extract::check_int_range(&mut self.0, field, num, min, max)
    }

    /// Sama untuk nilai query dengan `@Type(() => Number)`.
    pub fn query_int_range(
        &mut self,
        field: &str,
        v: Option<&QVal>,
        min: i64,
        max: i64,
    ) -> Option<i64> {
        let num = match v {
            Some(QVal::One(s)) => Some(js_number(s)),
            _ => None,
        };
        mz_core::extract::check_int_range(&mut self.0, field, num, min, max)
    }
}

/// `@Transform(({ value }) => typeof value === 'string' ? value.trim() : value)`.
pub fn trimmed(v: Option<&Value>) -> Option<Value> {
    v.map(|v| match v {
        Value::String(s) => Value::String(js_trim(s).to_owned()),
        other => other.clone(),
    })
}

/// Nilai query sebagai JSON (`qs`: string, array, atau objek).
pub fn qval_json(v: &QVal) -> Value {
    match v {
        QVal::One(s) => Value::String(s.clone()),
        QVal::Many(items) => Value::Array(items.iter().cloned().map(Value::String).collect()),
        QVal::Obj => Value::Object(serde_json::Map::new()),
    }
}

/// `@IsOptional()`: lewati validasi bila `undefined`/`null`.
pub fn present(v: Option<&Value>) -> bool {
    !matches!(v, None | Some(Value::Null))
}

/// `ReasonDto.reason` (`@Transform(trim) @IsString @Length(5, 500)`): Length lalu IsString.
pub fn reason(errors: &mut Errors, body: &Value) -> Option<String> {
    let v = trimmed(body.get("reason"));
    errors.length("reason", v.as_ref(), 5, 500);
    errors.string("reason", v.as_ref());
    v.and_then(|v| v.as_str().map(str::to_owned))
}

/// `OptionalReasonDto.reason` (`@IsOptional` + sama dengan [`reason`]).
pub fn optional_reason(errors: &mut Errors, body: &Value) -> Option<String> {
    let v = trimmed(body.get("reason"));
    if !present(v.as_ref()) {
        return None;
    }
    errors.length("reason", v.as_ref(), 5, 500);
    errors.string("reason", v.as_ref());
    v.and_then(|v| v.as_str().map(str::to_owned))
}

/// validator.js `isISO8601(str, { strict: true })` — satu implementasi di `mz_core::iso8601`.
pub use mz_core::iso8601::is_iso8601_strict;

/// `^\d{4}-\d{2}-\d{2}$`.
pub fn is_date_only(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b.iter().enumerate().all(|(i, c)| match i {
            4 | 7 => *c == b'-',
            _ => c.is_ascii_digit(),
        })
}

// ── Body & query mentah ─────────────────────────────────────────────────────

/// Body mentah ala `express.json` (strict) — satu implementasi di `mz_core::extract`.
pub use mz_core::extract::RawBody;

/// Query mentah ala `req.query` — satu implementasi di `mz_core::extract`.
pub use mz_core::extract::ParsedQuery as RawQuery;

/// Param path mentah (`ParseUUIDPipe` dijalankan handler SESUDAH guard).
pub struct RawPath(pub String);

impl<S: Send + Sync> FromRequestParts<S> for RawPath {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let axum::extract::Path(map) = axum::extract::Path::<
            std::collections::HashMap<String, String>,
        >::from_request_parts(parts, state)
        .await
        .map_err(|e| ApiError::bad_request(e.body_text()))?;
        Ok(Self(map.get("uuid").cloned().unwrap_or_default()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_strict() {
        assert!(is_iso8601_strict("2026-10-12"));
        assert!(!is_iso8601_strict("2026-02-30"));
        assert!(!is_iso8601_strict("2026-1-01"));
        assert!(is_iso8601_strict("2024-02-29"));
        assert!(!is_iso8601_strict("2023-02-29"));
        assert!(is_iso8601_strict("2026-10-12T00:00:00Z"));
        assert!(!is_iso8601_strict("2026-366"));
        assert!(is_iso8601_strict("2024-366"));
    }

    #[test]
    fn date_only() {
        assert!(is_date_only("2026-10-12"));
        assert!(!is_date_only("2026-10-12T00:00:00Z"));
        assert!(!is_date_only("2026-1-01"));
    }
}
