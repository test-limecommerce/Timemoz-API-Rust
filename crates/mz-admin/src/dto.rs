//! DTO + validasi bergaya class-validator untuk controller `admin` dan `admin/users`.
//!
//! Dua pipe Nest dipakai di sana:
//! * `ADMIN_VALIDATION_PIPE` (`whitelist` + `forbidNonWhitelisted` + `transform`): create/update/delete
//!   operator dan semua route `admin/users` → `whitelist = true` di [`run_body`].
//! * pipe global `ValidationPipe({ transform: true })`: login, forgot-password, `GET /admin` →
//!   `whitelist = false`.
//!
//! Urutan pesan (diverifikasi terhadap `ValidationPipe` Node dari `dist`):
//! properti asing lebih dulu (urutan kunci), lalu per properti sesuai deklarasi kelas (kelas anak sebelum
//! kelas induk), dan per properti urutan dekorator dari BAWAH ke atas.

use mz_core::extract::{is_email, validator_js_length};
use mz_core::jsfmt::{number as js_number, trim as js_trim};
use mz_core::qs::{self, QVal};
use mz_core::{ApiError, ApiResult};
use serde_json::{Map, Value};

// ── Body & query mentah ─────────────────────────────────────────────────────

/// Body mentah ala `express.json` (strict) — satu implementasi di `mz_core::extract`.
pub use mz_core::extract::RawBody;

/// Query mentah ala `req.query` — satu implementasi di `mz_core::extract`.
pub use mz_core::extract::ParsedQuery as RawQuery;

// ── Pemeriksa constraint ────────────────────────────────────────────────────

/// Constraint class-validator yang dipakai DTO di crate ini.
#[derive(Debug, Clone, Copy)]
pub enum Check {
    IsString,
    IsNotEmpty,
    IsEmail,
    MinLength(usize),
    MaxLength(usize),
    Length(usize, usize),
    IsInStr(&'static [&'static str]),
    IsInInt(&'static [i64]),
}

/// Transform sebelum validasi (hanya bila kunci dikirim).
#[derive(Debug, Clone, Copy)]
pub enum Transform {
    None,
    /// `typeof v === 'string' ? v.trim() : v`.
    Trim,
    /// `typeof v === 'string' ? v.trim().toLowerCase() : v`.
    TrimLower,
}

/// Satu properti DTO. `checks` ditulis dalam urutan PESAN (dekorator dari bawah ke atas).
#[derive(Debug, Clone, Copy)]
pub struct Field {
    pub name: &'static str,
    /// `@IsOptional()`: `null`/`undefined` melewati semua constraint.
    pub optional: bool,
    pub transform: Transform,
    pub checks: &'static [Check],
}

fn is_nullish(v: Option<&Value>) -> bool {
    matches!(v, None | Some(Value::Null))
}

use mz_core::extract::length_message;

/// `true` bila constraint terpenuhi.
fn passes(check: Check, v: Option<&Value>) -> bool {
    let s = v.and_then(Value::as_str);
    match check {
        Check::IsString => s.is_some(),
        Check::IsNotEmpty => !is_nullish(v) && s != Some(""),
        Check::IsEmail => s.is_some_and(is_email_strict),
        Check::MinLength(n) => s.is_some_and(|s| validator_js_length(s) >= n),
        Check::MaxLength(n) => s.is_some_and(|s| validator_js_length(s) <= n),
        Check::Length(a, b) => s.is_some_and(|s| (a..=b).contains(&validator_js_length(s))),
        Check::IsInStr(list) => s.is_some_and(|s| list.contains(&s)),
        Check::IsInInt(list) => v
            .and_then(Value::as_f64)
            .is_some_and(|n| list.iter().any(|i| (*i as f64 - n).abs() == 0.0)),
    }
}

/// `isEmail` validator.js: perkiraan `mz_core::extract::is_email` + batas panjang bagian lokal (64) dan
/// domain (254) yang dipakai validator.js (`ignore_max_length: false`).
fn is_email_strict(s: &str) -> bool {
    is_email(s)
        && s.rsplit_once('@').is_some_and(|(local, domain)| {
            local.chars().count() <= 64 && domain.chars().count() <= 254
        })
}

fn message(check: Check, field: &str, v: Option<&Value>) -> String {
    match check {
        Check::IsString => format!("{field} must be a string"),
        Check::IsNotEmpty => format!("{field} should not be empty"),
        Check::IsEmail => format!("{field} must be an email"),
        Check::MinLength(n) => format!("{field} must be longer than or equal to {n} characters"),
        Check::MaxLength(n) => format!("{field} must be shorter than or equal to {n} characters"),
        Check::Length(a, b) => length_message(field, v, a, b),
        Check::IsInStr(list) => {
            format!(
                "{field} must be one of the following values: {}",
                list.join(", ")
            )
        }
        Check::IsInInt(list) => {
            let joined: Vec<String> = list.iter().map(ToString::to_string).collect();
            format!(
                "{field} must be one of the following values: {}",
                joined.join(", ")
            )
        }
    }
}

fn transformed(t: Transform, v: &Value) -> Value {
    match (t, v) {
        (Transform::Trim, Value::String(s)) => Value::String(js_trim(s).to_owned()),
        (Transform::TrimLower, Value::String(s)) => Value::String(js_trim(s).to_lowercase()),
        _ => v.clone(),
    }
}

/// `ValidationPipe` untuk body JSON. `fields` = properti DTO (kelas anak lebih dulu). Mengembalikan
/// objek hasil transform berisi hanya properti DTO yang dikirim.
pub fn run_body(
    body: &Value,
    fields: &[&[Field]],
    whitelist: bool,
) -> ApiResult<Map<String, Value>> {
    let empty = Map::new();
    let (obj, array_len) = match body {
        Value::Object(m) => (m, 0),
        Value::Array(a) => (&empty, a.len()),
        _ => (&empty, 0),
    };
    let all: Vec<&Field> = fields.iter().flat_map(|g| g.iter()).collect();
    let mut errors = Vec::new();
    if whitelist {
        for i in 0..array_len {
            errors.push(format!("property {i} should not exist"));
        }
        for k in obj.keys() {
            if !all.iter().any(|f| f.name == k) {
                errors.push(format!("property {k} should not exist"));
            }
        }
    }
    let mut out = Map::new();
    for f in all {
        let value = obj.get(f.name).map(|v| transformed(f.transform, v));
        if f.optional && is_nullish(value.as_ref()) {
            if let Some(v) = value {
                out.insert(f.name.to_owned(), v);
            }
            continue;
        }
        for c in f.checks {
            if !passes(*c, value.as_ref()) {
                errors.push(message(*c, f.name, value.as_ref()));
            }
        }
        if let Some(v) = value {
            out.insert(f.name.to_owned(), v);
        }
    }
    if errors.is_empty() {
        Ok(out)
    } else {
        Err(ApiError::Validation(errors))
    }
}

// ── DTO body ────────────────────────────────────────────────────────────────

/// Peran operator (`OPERATOR_ROLES`).
pub const OPERATOR_ROLES: [&str; 3] = ["OWNER", "FINANCE", "SUPPORT"];

/// `LoginDto` (`admin/dto/login.dto.ts`), pipe global.
pub const LOGIN: &[Field] = &[
    Field {
        name: "username",
        optional: false,
        transform: Transform::None,
        checks: &[Check::IsNotEmpty, Check::IsEmail],
    },
    Field {
        name: "password",
        optional: false,
        transform: Transform::None,
        checks: &[Check::MinLength(6), Check::IsNotEmpty, Check::IsString],
    },
];

/// `CreateAdminDto`.
pub const CREATE_ADMIN: &[Field] = &[
    Field {
        name: "username",
        optional: false,
        transform: Transform::TrimLower,
        checks: &[Check::MaxLength(255), Check::IsEmail],
    },
    Field {
        name: "password",
        optional: false,
        transform: Transform::None,
        checks: &[Check::Length(8, 72), Check::IsString],
    },
    Field {
        name: "name",
        optional: false,
        transform: Transform::Trim,
        checks: &[Check::MaxLength(255), Check::IsNotEmpty, Check::IsString],
    },
    Field {
        name: "role",
        optional: true,
        transform: Transform::None,
        checks: &[Check::IsInStr(&OPERATOR_ROLES)],
    },
];

/// `UpdateAdminDto`.
pub const UPDATE_ADMIN: &[Field] = &[
    Field {
        name: "username",
        optional: true,
        transform: Transform::TrimLower,
        checks: &[Check::MaxLength(255), Check::IsEmail],
    },
    Field {
        name: "name",
        optional: true,
        transform: Transform::Trim,
        checks: &[Check::MaxLength(255), Check::IsNotEmpty, Check::IsString],
    },
    Field {
        name: "role",
        optional: true,
        transform: Transform::None,
        checks: &[Check::IsInStr(&OPERATOR_ROLES)],
    },
    Field {
        name: "status",
        optional: true,
        transform: Transform::None,
        checks: &[Check::IsInInt(&[0, 1])],
    },
    Field {
        name: "reason",
        optional: true,
        transform: Transform::Trim,
        checks: &[Check::Length(5, 500), Check::IsString],
    },
];

/// `ReasonDto` (wajib).
pub const REASON: &[Field] = &[Field {
    name: "reason",
    optional: false,
    transform: Transform::Trim,
    checks: &[Check::Length(5, 500), Check::IsString],
}];

/// `OptionalReasonDto`.
pub const OPTIONAL_REASON: &[Field] = &[Field {
    name: "reason",
    optional: true,
    transform: Transform::Trim,
    checks: &[Check::Length(5, 500), Check::IsString],
}];

/// `ChangeEmailDto` (turunan `ReasonDto`: `email` dulu, lalu `reason`).
pub const CHANGE_EMAIL: &[Field] = &[Field {
    name: "email",
    optional: false,
    transform: Transform::TrimLower,
    checks: &[Check::MaxLength(255), Check::IsEmail],
}];

/// String hasil validasi (`""` bila tak ada; hanya dipakai untuk properti wajib yang sudah lolos).
pub fn text<'a>(m: &'a Map<String, Value>, key: &str) -> &'a str {
    m.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// String opsional (`None` bila tak dikirim / null).
pub fn opt_text<'a>(m: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    m.get(key).and_then(Value::as_str)
}

// ── DTO query ───────────────────────────────────────────────────────────────

fn qval_to_value(v: &QVal) -> Value {
    match v {
        QVal::One(s) => Value::String(s.clone()),
        QVal::Many(items) => Value::Array(items.iter().cloned().map(Value::String).collect()),
        QVal::Obj => Value::Object(Map::new()),
    }
}

/// `@Type(() => Number)` class-transformer: string → `Number(s)`; array/objek → bukan angka
/// (diverifikasi: `pageNumber[]=3` gagal Min + IsInt).
fn type_number(v: &QVal) -> f64 {
    match v {
        QVal::One(s) => js_number(s),
        QVal::Many(_) | QVal::Obj => f64::NAN,
    }
}

/// Pesan `@Min`/`@IsInt`/`@Max` untuk angka hasil `@Type(() => Number)`; urutan: Max, Min, IsInt.
fn int_errors(errors: &mut Vec<String>, field: &str, n: f64, min: f64, max: Option<f64>) -> bool {
    let before = errors.len();
    if let Some(max) = max
        && (n.is_nan() || n > max)
    {
        errors.push(format!("{field} must not be greater than {max}"));
    }
    if n.is_nan() || n < min {
        errors.push(format!("{field} must not be less than {min}"));
    }
    if !(n.is_finite() && n.fract() == 0.0) {
        errors.push(format!("{field} must be an integer number"));
    }
    errors.len() == before
}

/// `AdminUserSearchDto` yang sudah tervalidasi.
#[derive(Debug, Clone, PartialEq)]
pub struct UserSearch {
    pub search: Option<String>,
    /// `ACTIVE` | `INVITED` | `DISABLED` | `DELETED`.
    pub status: Option<String>,
    pub page_number: f64,
    pub items_per_page: f64,
}

pub const ADMIN_USER_STATUSES: [&str; 4] = ["ACTIVE", "INVITED", "DISABLED", "DELETED"];

/// `AdminUserSearchDto` di bawah `ADMIN_VALIDATION_PIPE` (query).
pub fn validate_user_search(q: &[(String, QVal)]) -> ApiResult<UserSearch> {
    const KNOWN: [&str; 4] = ["search", "status", "pageNumber", "itemsPerPage"];
    let mut errors = Vec::new();
    for (k, _) in q {
        if !KNOWN.contains(&k.as_str()) {
            errors.push(format!("property {k} should not exist"));
        }
    }

    // search: Transform(trim; '' → undefined), IsOptional, IsString, Length(3, 100).
    let mut search = None;
    if let Some(v) = qs::get(q, "search") {
        let value = match v {
            QVal::One(s) => {
                let t = js_trim(s);
                (!t.is_empty()).then(|| Value::String(t.to_owned()))
            }
            other => Some(qval_to_value(other)),
        };
        if let Some(value) = value {
            if !passes(Check::Length(3, 100), Some(&value)) {
                errors.push(length_message("search", Some(&value), 3, 100));
            }
            if let Value::String(s) = &value {
                search = Some(s.clone());
            } else {
                errors.push("search must be a string".to_owned());
            }
        }
    }

    let mut status = None;
    if let Some(v) = qs::get(q, "status") {
        let value = qval_to_value(v);
        if passes(Check::IsInStr(&ADMIN_USER_STATUSES), Some(&value)) {
            status = value.as_str().map(str::to_owned);
        } else {
            errors.push(message(
                Check::IsInStr(&ADMIN_USER_STATUSES),
                "status",
                Some(&value),
            ));
        }
    }

    let mut page_number = 1.0;
    if let Some(v) = qs::get(q, "pageNumber") {
        let n = type_number(v);
        if int_errors(&mut errors, "pageNumber", n, 1.0, None) {
            page_number = n;
        }
    }
    let mut items_per_page = 20.0;
    if let Some(v) = qs::get(q, "itemsPerPage") {
        let n = type_number(v);
        if int_errors(&mut errors, "itemsPerPage", n, 1.0, Some(50.0)) {
            items_per_page = n;
        }
    }

    if errors.is_empty() {
        Ok(UserSearch {
            search,
            status,
            page_number,
            items_per_page,
        })
    } else {
        Err(ApiError::Validation(errors))
    }
}

/// `parseInt(s, 10)` sebagai f64 (`None` = NaN); tidak meluap seperti `i64`.
pub fn parse_int_f64(s: &str) -> Option<f64> {
    let t = s.trim_start_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
    let (neg, rest) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    let n: f64 = digits.parse().ok()?;
    Some(if neg { -n } else { n })
}

/// `PaginatorDto` (pipe global, tanpa whitelist) yang sudah tervalidasi: hanya `pageNumber`,
/// `itemsPerPage`, dan `search` yang dipakai `AdminService.findAll`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Paginator {
    /// Hasil `Transform(parseInt)`; `None` = tak dikirim.
    pub page_number: Option<f64>,
    pub items_per_page: Option<f64>,
    pub search: Option<String>,
}

/// `PaginatorDto`: `@IsNumber @IsOptional @Transform(parseInt)` untuk dua angka, `@IsString @IsOptional`
/// untuk `orderBy`/`orderColumn`/`search`.
pub fn validate_paginator(q: &[(String, QVal)]) -> ApiResult<Paginator> {
    let mut errors = Vec::new();
    let mut out = Paginator::default();
    for (name, slot) in [
        ("pageNumber", &mut out.page_number),
        ("itemsPerPage", &mut out.items_per_page),
    ] {
        match qs::get(q, name) {
            None => {}
            Some(QVal::One(s)) => match parse_int_f64(s) {
                Some(n) => *slot = Some(n),
                None => errors.push(format!(
                    "{name} must be a number conforming to the specified constraints"
                )),
            },
            Some(_) => errors.push(format!(
                "{name} must be a number conforming to the specified constraints"
            )),
        }
    }
    for name in ["orderBy", "orderColumn", "search"] {
        match qs::get(q, name) {
            None => {}
            Some(QVal::One(s)) => {
                if name == "search" {
                    out.search = Some(s.clone());
                }
            }
            Some(_) => errors.push(format!("{name} must be a string")),
        }
    }
    if errors.is_empty() {
        Ok(out)
    } else {
        Err(ApiError::Validation(errors))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn msgs(r: ApiResult<impl std::fmt::Debug>) -> Vec<String> {
        match r {
            Err(ApiError::Validation(m)) => m,
            other => vec![format!("{other:?}")],
        }
    }

    fn q(raw: &str) -> Vec<(String, QVal)> {
        qs::parse(raw)
    }

    // Semua ekspektasi di bawah dihasilkan dengan menjalankan `ValidationPipe` Node (dist Mudah-API)
    // + DTO aslinya (`ADMIN_VALIDATION_PIPE` untuk body/query admin, pipe global untuk login/paginator).

    #[test]
    fn create_admin_matches_node() {
        let r = run_body(
            &json!({"username":"A@b.co","password":"x","name":"","role":"X","zzz":1}),
            &[CREATE_ADMIN],
            true,
        );
        assert_eq!(
            msgs(r),
            [
                "property zzz should not exist",
                "password must be longer than or equal to 8 characters",
                "name should not be empty",
                "role must be one of the following values: OWNER, FINANCE, SUPPORT",
            ]
        );
        let ok = run_body(
            &json!({"username":"  A@B.co ","password":"12345678","name":" Zed ","role":"OWNER"}),
            &[CREATE_ADMIN],
            true,
        );
        let ok = ok.unwrap_or_default();
        assert_eq!(text(&ok, "username"), "a@b.co");
        assert_eq!(text(&ok, "name"), "Zed");
    }

    #[test]
    fn reason_matches_node() {
        assert_eq!(
            msgs(run_body(&json!({}), &[REASON], true)),
            [
                "reason must be longer than or equal to 5 characters",
                "reason must be a string"
            ]
        );
        assert_eq!(
            msgs(run_body(&json!([1, 2]), &[REASON], true)),
            [
                "property 0 should not exist",
                "property 1 should not exist",
                "reason must be longer than or equal to 5 characters",
                "reason must be a string"
            ]
        );
    }

    #[test]
    fn change_email_matches_node() {
        assert_eq!(
            msgs(run_body(
                &json!({"email":"bad","reason":"ab"}),
                &[CHANGE_EMAIL, REASON],
                true
            )),
            [
                "email must be an email",
                "reason must be longer than or equal to 5 characters"
            ]
        );
    }

    #[test]
    fn login_matches_node() {
        assert_eq!(
            msgs(run_body(&json!({}), &[LOGIN], false)),
            [
                "username should not be empty",
                "username must be an email",
                "password must be longer than or equal to 6 characters",
                "password should not be empty",
                "password must be a string"
            ]
        );
    }

    #[test]
    fn user_search_matches_node() {
        assert_eq!(
            msgs(validate_user_search(&q(
                "foo=1&search=ab&status=X&pageNumber=abc&itemsPerPage=100"
            ))),
            [
                "property foo should not exist",
                "search must be longer than or equal to 3 characters",
                "status must be one of the following values: ACTIVE, INVITED, DISABLED, DELETED",
                "pageNumber must not be less than 1",
                "pageNumber must be an integer number",
                "itemsPerPage must not be greater than 50"
            ]
        );
        assert_eq!(
            msgs(validate_user_search(&q("search=%20%20&pageNumber=0"))),
            ["pageNumber must not be less than 1"]
        );
        assert_eq!(
            msgs(validate_user_search(&q(
                "search[]=abc&status[]=ACTIVE&pageNumber[]=3"
            ))),
            [
                "search must be longer than or equal to 3 characters",
                "search must be a string",
                "status must be one of the following values: ACTIVE, INVITED, DISABLED, DELETED",
                "pageNumber must not be less than 1",
                "pageNumber must be an integer number"
            ]
        );
        assert_eq!(
            validate_user_search(&q("search=abc&status=ACTIVE&pageNumber=2&itemsPerPage=10")).ok(),
            Some(UserSearch {
                search: Some("abc".into()),
                status: Some("ACTIVE".into()),
                page_number: 2.0,
                items_per_page: 10.0
            })
        );
        assert_eq!(
            msgs(validate_user_search(&q("itemsPerPage=1.5&pageNumber="))),
            [
                "pageNumber must not be less than 1",
                "itemsPerPage must be an integer number"
            ]
        );
        assert_eq!(
            msgs(validate_user_search(&q("search[a]=b&search=ccc"))),
            [
                "search must be longer than or equal to 3 and shorter than or equal to 100 characters",
                "search must be a string"
            ]
        );
    }

    #[test]
    fn paginator_matches_node() {
        assert_eq!(
            msgs(validate_paginator(&q(
                "pageNumber=abc&itemsPerPage=x&orderBy[]=a&search=z"
            ))),
            [
                "pageNumber must be a number conforming to the specified constraints",
                "itemsPerPage must be a number conforming to the specified constraints",
                "orderBy must be a string"
            ]
        );
        assert_eq!(
            validate_paginator(&q("pageNumber=3&itemsPerPage=5.9&search=q")).ok(),
            Some(Paginator {
                page_number: Some(3.0),
                items_per_page: Some(5.0),
                search: Some("q".into())
            })
        );
        assert_eq!(
            msgs(validate_paginator(&q("pageNumber[]=3&search[a]=1"))),
            [
                "pageNumber must be a number conforming to the specified constraints",
                "search must be a string"
            ]
        );
    }

    #[test]
    fn edge_cases_match_node() {
        let long_local = format!("{}@b.co", "a".repeat(250));
        let cases: Vec<(&[Field], Value, Vec<&str>)> = vec![
            (
                UPDATE_ADMIN,
                json!({"status":"1","username":"BAD","name":5,"reason":"abc","role":null}),
                vec![
                    "username must be an email",
                    "name must be shorter than or equal to 255 characters",
                    "name must be a string",
                    "status must be one of the following values: 0, 1",
                    "reason must be longer than or equal to 5 characters",
                ],
            ),
            (
                CREATE_ADMIN,
                json!({"username":5,"password":12345678,"name":"x","role":null}),
                vec![
                    "username must be shorter than or equal to 255 characters",
                    "username must be an email",
                    "password must be longer than or equal to 8 and shorter than or equal to 72 characters",
                    "password must be a string",
                ],
            ),
            (
                OPTIONAL_REASON,
                json!({"reason":12345}),
                vec![
                    "reason must be longer than or equal to 5 and shorter than or equal to 500 characters",
                    "reason must be a string",
                ],
            ),
            (
                REASON,
                json!({"reason":"  ab  "}),
                vec!["reason must be longer than or equal to 5 characters"],
            ),
            (
                UPDATE_ADMIN,
                json!({"status":1.0,"name":"  "}),
                vec!["name should not be empty"],
            ),
            (
                REASON,
                json!({"reason":["abcdef"]}),
                vec![
                    "reason must be longer than or equal to 5 characters",
                    "reason must be a string",
                ],
            ),
            (
                LOGIN,
                json!({"username":"A@b.co","password":123456}),
                vec![
                    "password must be longer than or equal to 6 characters",
                    "password must be a string",
                ],
            ),
            (
                REASON,
                json!({"reason":"x".repeat(501)}),
                vec!["reason must be shorter than or equal to 500 characters"],
            ),
            (
                REASON,
                json!({"reason":0}),
                vec![
                    "reason must be longer than or equal to 5 characters",
                    "reason must be a string",
                ],
            ),
            (
                REASON,
                json!({"reason":true}),
                vec![
                    "reason must be longer than or equal to 5 and shorter than or equal to 500 characters",
                    "reason must be a string",
                ],
            ),
            (
                UPDATE_ADMIN,
                json!({"username": long_local}),
                vec!["username must be an email"],
            ),
        ];
        for (fields, body, expected) in cases {
            let whitelist = !std::ptr::eq(fields, LOGIN);
            assert_eq!(
                msgs(run_body(&body, &[fields], whitelist)),
                expected,
                "{body}"
            );
        }
        assert!(run_body(&json!({"reason": null}), &[OPTIONAL_REASON], true).is_ok());
        assert_eq!(
            msgs(run_body(
                &json!({"email":null,"reason":null,"x":1}),
                &[CHANGE_EMAIL, REASON],
                true
            )),
            [
                "property x should not exist",
                "email must be shorter than or equal to 255 characters",
                "email must be an email",
                "reason must be longer than or equal to 5 characters",
                "reason must be a string"
            ]
        );
    }

    #[test]
    fn parse_int_handles_overflow() {
        assert_eq!(parse_int_f64(" 12abc"), Some(12.0));
        assert_eq!(parse_int_f64("abc"), None);
        assert_eq!(parse_int_f64("-3"), Some(-3.0));
        assert!(parse_int_f64("99999999999999999999").is_some_and(|n| n > 9.0e19));
    }
}
