//! Parser body (`express.json` strict + `express.urlencoded({ extended: true })`) dan
//! validator gaya class-validator untuk DTO OAuth/token yang memakai pipe GLOBAL
//! (`ValidationPipe({ transform: true })`, tanpa whitelist).
//!
//! Urutan pesan per properti = dekorator dari BAWAH ke ATAS (urutan registrasi
//! class-validator); urutan properti = urutan deklarasi, dengan properti kelas turunan
//! lebih dulu daripada yang diwarisi.

use axum::extract::{FromRequest, Request};
use mz_core::extract::validator_js_length;
use mz_core::qs::{self, QVal};
use mz_core::{ApiError, AppState};
use serde_json::{Map, Value};

/// Body request sebagai objek JSON. Content-type JSON → di-parse (strict: hanya objek/array,
/// array diperlakukan `{}`); urlencoded → `qs`; selain itu / kosong → `{}`.
#[derive(Debug, Clone)]
pub struct Body(pub Map<String, Value>);

fn qs_to_map(raw: &str) -> Map<String, Value> {
    qs::parse(raw)
        .into_iter()
        .map(|(k, v)| {
            let v = match v {
                QVal::One(s) => Value::String(s),
                QVal::Many(items) => Value::Array(items.into_iter().map(Value::String).collect()),
                QVal::Obj => Value::Object(Map::new()),
            };
            (k, v)
        })
        .collect()
}

/// `req.query` sebagai objek JSON.
pub fn query_map(raw: Option<&str>) -> Map<String, Value> {
    qs_to_map(raw.unwrap_or_default())
}

fn json_error(e: &serde_json::Error) -> ApiError {
    let s = e.to_string();
    ApiError::BadJson(match s.find(" at line ") {
        Some(i) => s.get(..i).unwrap_or(&s).to_owned(),
        None => s,
    })
}

impl FromRequest<AppState> for Body {
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &AppState) -> Result<Self, Self::Rejection> {
        let ct = req
            .headers()
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();
        let is_json = ct.starts_with("application/json") || ct.contains("+json");
        let is_form = ct.starts_with("application/x-www-form-urlencoded");
        let bytes = axum::body::Bytes::from_request(req, state)
            .await
            .map_err(mz_core::extract::bytes_rejection)?;
        if is_json && !bytes.is_empty() {
            let value: Value = serde_json::from_slice(&bytes).map_err(|e| json_error(&e))?;
            return match value {
                Value::Object(m) => Ok(Self(m)),
                Value::Array(_) => Ok(Self(Map::new())),
                other => Err(ApiError::BadJson(format!(
                    "Unexpected token in JSON: {other}"
                ))),
            };
        }
        if is_form && !bytes.is_empty() {
            return Ok(Self(qs_to_map(&String::from_utf8_lossy(&bytes))));
        }
        Ok(Self(Map::new()))
    }
}

// ───── validator ─────

/// Pengumpul pesan.
#[derive(Default)]
pub struct V {
    pub errors: Vec<String>,
}

/// `value === undefined || value === null` (dilewati `@IsOptional`).
pub fn is_nullish(v: Option<&Value>) -> bool {
    matches!(v, None | Some(Value::Null))
}

impl V {
    fn push(&mut self, m: String) {
        self.errors.push(m);
    }

    /// `@IsNotEmpty()` + `@IsString()` (pesan dalam urutan bawah → atas).
    pub fn not_empty_string(&mut self, f: &str, v: Option<&Value>) {
        if is_nullish(v) || matches!(v, Some(Value::String(s)) if s.is_empty()) {
            self.push(format!("{f} should not be empty"));
        }
        if !matches!(v, Some(Value::String(_))) {
            self.push(format!("{f} must be a string"));
        }
    }

    /// `@IsOptional() @IsString()`.
    pub fn opt_string(&mut self, f: &str, v: Option<&Value>) {
        if !is_nullish(v) && !matches!(v, Some(Value::String(_))) {
            self.push(format!("{f} must be a string"));
        }
    }

    /// `@IsOptional() @IsString() @IsNotEmpty()`.
    pub fn opt_not_empty_string(&mut self, f: &str, v: Option<&Value>) {
        if is_nullish(v) {
            return;
        }
        self.not_empty_string(f, v);
    }

    /// `@IsIn([...])`.
    pub fn is_in(&mut self, f: &str, v: Option<&Value>, allowed: &[&str]) {
        let ok = matches!(v, Some(Value::String(s)) if allowed.contains(&s.as_str()));
        if !ok {
            self.push(format!(
                "{f} must be one of the following values: {}",
                allowed.join(", ")
            ));
        }
    }

    /// `@IsOptional() @IsIn([...])`.
    pub fn opt_is_in(&mut self, f: &str, v: Option<&Value>, allowed: &[&str]) {
        if !is_nullish(v) {
            self.is_in(f, v, allowed);
        }
    }

    /// `@IsUUID()`.
    pub fn uuid(&mut self, f: &str, v: Option<&Value>) {
        let ok = matches!(v, Some(Value::String(s)) if mz_core::uuid_rules::is_uuid(s));
        if !ok {
            self.push(format!("{f} must be a UUID"));
        }
    }

    /// `@IsBoolean()`.
    pub fn boolean(&mut self, f: &str, v: Option<&Value>) {
        if !matches!(v, Some(Value::Bool(_))) {
            self.push(format!("{f} must be a boolean value"));
        }
    }

    /// `@MaxLength(n)` (validator.js `isLength`; bukan string → gagal).
    pub fn max_length(&mut self, f: &str, v: Option<&Value>, n: usize) {
        let ok = matches!(v, Some(Value::String(s)) if validator_js_length(s) <= n);
        if !ok {
            self.push(format!(
                "{f} must be shorter than or equal to {n} characters"
            ));
        }
    }

    /// `@IsArray() @IsString({ each: true })` (pesan: each dulu, lalu array).
    pub fn string_array(&mut self, f: &str, v: Option<&Value>) {
        let each_ok = match v {
            Some(Value::Array(a)) => a.iter().all(Value::is_string),
            Some(Value::String(_)) => true,
            _ => false,
        };
        if !each_ok {
            self.push(format!("each value in {f} must be a string"));
        }
        if !matches!(v, Some(Value::Array(_))) {
            self.push(format!("{f} must be an array"));
        }
    }

    /// `@IsOptional() @IsArray() @IsString({ each: true })`.
    pub fn opt_string_array(&mut self, f: &str, v: Option<&Value>) {
        if !is_nullish(v) {
            self.string_array(f, v);
        }
    }

    pub fn finish(self) -> Result<(), ApiError> {
        if self.errors.is_empty() {
            Ok(())
        } else {
            Err(ApiError::Validation(self.errors))
        }
    }
}

/// Nilai string (setelah divalidasi) atau `None`.
pub fn str_of<'a>(m: &'a Map<String, Value>, k: &str) -> Option<&'a str> {
    m.get(k).and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn errs(f: impl FnOnce(&mut V)) -> Vec<String> {
        let mut v = V::default();
        f(&mut v);
        v.errors
    }

    #[test]
    fn not_empty_string_order() {
        assert_eq!(
            errs(|v| v.not_empty_string("x", None)),
            ["x should not be empty", "x must be a string"]
        );
        assert_eq!(
            errs(|v| v.not_empty_string("x", Some(&json!("")))),
            ["x should not be empty"]
        );
        assert_eq!(
            errs(|v| v.not_empty_string("x", Some(&json!(5)))),
            ["x must be a string"]
        );
        assert!(errs(|v| v.opt_not_empty_string("x", None)).is_empty());
        assert_eq!(
            errs(|v| v.opt_not_empty_string("x", Some(&json!("")))),
            ["x should not be empty"]
        );
    }

    #[test]
    fn string_array_messages() {
        assert_eq!(
            errs(|v| v.string_array("u", None)),
            ["each value in u must be a string", "u must be an array"]
        );
        assert_eq!(
            errs(|v| v.string_array("u", Some(&json!("a")))),
            ["u must be an array"]
        );
        assert_eq!(
            errs(|v| v.string_array("u", Some(&json!(["a", 1])))),
            ["each value in u must be a string"]
        );
        assert!(errs(|v| v.string_array("u", Some(&json!([])))).is_empty());
    }

    #[test]
    fn is_in_and_uuid_and_bool() {
        assert_eq!(
            errs(|v| v.is_in("g", Some(&json!("x")), &["a", "b"])),
            ["g must be one of the following values: a, b"]
        );
        assert_eq!(
            errs(|v| v.uuid("c", Some(&json!("nope")))),
            ["c must be a UUID"]
        );
        assert_eq!(
            errs(|v| v.boolean("approved", Some(&json!("true")))),
            ["approved must be a boolean value"]
        );
        assert_eq!(
            errs(|v| v.max_length("n", None, 3)),
            ["n must be shorter than or equal to 3 characters"]
        );
    }
}
