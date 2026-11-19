//! Helper bersama crate ini: body/query mentah, validasi gaya class-validator di atas
//! `serde_json::Value`, paging, dan bentuk respons `{ success, message, data }`.
//!
//! Validasi ditulis di atas `Value` (bukan struct serde) supaya pesan galat dan urutannya
//! sama dengan `ValidationPipe` Node: class-validator mengumpulkan pesan per properti dalam
//! urutan deklarasi, dan per properti dalam urutan dekorator dari bawah ke atas.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use mz_core::extract::validator_js_length;
use mz_core::qs::{self, QVal};
use mz_core::{ApiError, ApiResult};
use serde_json::{Value, json};

/// Body mentah ala `express.json` (strict) — satu implementasi di `mz_core::extract`.
pub use mz_core::extract::RawBody;

/// Query string mentah (extractor tanpa gagal).
pub struct RawQuery(pub Option<String>);

impl<S: Send + Sync> FromRequestParts<S> for RawQuery {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(parts.uri.query().map(str::to_owned)))
    }
}

/// `{ success: true, message, data }` (urutan kunci = literal Node).
pub fn ok(message: &str, data: Value) -> Value {
    json!({ "success": true, "message": message, "data": data })
}

/// `{ success: false, message, data: null }`: respons 200 (atau 201 untuk POST) tanpa
/// exception, seperti `return { success: false, ... }` di service Node.
pub fn fail(message: &str) -> Value {
    json!({ "success": false, "message": message, "data": null })
}

/// Galat DB di dalam `try { } catch { return { success: false } }` Node: dicatat, lalu
/// dikembalikan sebagai `fail(message)`.
pub fn db_fail(op: &'static str, message: &str, err: &sqlx::Error) -> Value {
    tracing::error!(op, error = %err, "admin-misc: galat DB");
    fail(message)
}

/// Field body (`None` = `undefined`). Body bukan objek (array) → semua `undefined`.
pub fn field<'a>(body: &'a Value, key: &str) -> Option<&'a Value> {
    match body {
        Value::Object(m) => m.get(key),
        _ => None,
    }
}

/// `@IsOptional()` tidak memeriksa `null` maupun `undefined`.
pub fn is_present(v: Option<&Value>) -> bool {
    v.is_some_and(|v| !v.is_null())
}

/// `@IsNotEmpty()`: `!== '' && !== null && !== undefined`.
pub fn is_not_empty(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

/// `@IsString()` / `@IsNumber()` / `@IsBoolean()` atas nilai JSON.
pub fn is_string(v: Option<&Value>) -> bool {
    matches!(v, Some(Value::String(_)))
}

pub fn is_number(v: Option<&Value>) -> bool {
    matches!(v, Some(Value::Number(_)))
}

pub fn is_bool(v: Option<&Value>) -> bool {
    matches!(v, Some(Value::Bool(_)))
}

/// `@MaxLength(n)`: hanya string yang bisa lolos (`typeof value === 'string' && isLength`).
pub fn max_len_ok(v: Option<&Value>, n: usize) -> bool {
    matches!(v, Some(Value::String(s)) if validator_js_length(s) <= n)
}

/// `required(body, key, IsString)`: `@IsString() @IsNotEmpty()` (urutan pesan dari bawah).
pub fn req_string(errors: &mut Vec<String>, body: &Value, key: &str) {
    let v = field(body, key);
    if !is_not_empty(v) {
        errors.push(format!("{key} should not be empty"));
    }
    if !is_string(v) {
        errors.push(format!("{key} must be a string"));
    }
}

/// `@IsNumber() @IsNotEmpty()` (urutan pesan dari bawah).
pub fn req_number(errors: &mut Vec<String>, body: &Value, key: &str) {
    let v = field(body, key);
    if !is_not_empty(v) {
        errors.push(format!("{key} should not be empty"));
    }
    if !is_number(v) {
        errors.push(format!(
            "{key} must be a number conforming to the specified constraints"
        ));
    }
}

/// Opsional: bila ada (bukan null), harus string / number / boolean.
pub fn opt_string(errors: &mut Vec<String>, body: &Value, key: &str) {
    let v = field(body, key);
    if is_present(v) && !is_string(v) {
        errors.push(format!("{key} must be a string"));
    }
}

pub fn opt_number(errors: &mut Vec<String>, body: &Value, key: &str) {
    let v = field(body, key);
    if is_present(v) && !is_number(v) {
        errors.push(format!(
            "{key} must be a number conforming to the specified constraints"
        ));
    }
}

pub fn opt_bool(errors: &mut Vec<String>, body: &Value, key: &str) {
    let v = field(body, key);
    if is_present(v) && !is_bool(v) {
        errors.push(format!("{key} must be a boolean value"));
    }
}

/// `@IsOptional() @IsEnum(...)` / `@IsIn(...)` untuk nilai string.
pub fn opt_one_of(errors: &mut Vec<String>, body: &Value, key: &str, allowed: &[&str]) {
    let v = field(body, key);
    if is_present(v) && !matches!(v, Some(Value::String(s)) if allowed.contains(&s.as_str())) {
        errors.push(format!(
            "{key} must be one of the following values: {}",
            allowed.join(", ")
        ));
    }
}

/// `ValidationPipe` dengan `whitelist` + `forbidNonWhitelisted` pada body: properti asing
/// lebih dulu (urutan kemunculan). Array: satu pesan per indeks.
pub fn whitelist_body(errors: &mut Vec<String>, body: &Value, allowed: &[&str]) {
    match body {
        Value::Object(m) => {
            for k in m.keys() {
                if !allowed.contains(&k.as_str()) {
                    errors.push(format!("property {k} should not exist"));
                }
            }
        }
        Value::Array(a) => {
            for i in 0..a.len() {
                errors.push(format!("property {i} should not exist"));
            }
        }
        _ => {}
    }
}

/// `ReasonDto`: `@Transform(trim) @IsString() @Length(5, 500)`, pesan Length lalu IsString.
/// Nilai di-trim seperti `@Transform` sebelum diperiksa.
pub fn reason_errors(errors: &mut Vec<String>, body: &Value) -> Option<String> {
    let raw = field(body, "reason").map(|v| match v {
        Value::String(s) => Value::String(mz_core::jsfmt::trim(s).to_owned()),
        other => other.clone(),
    });
    if let Some(msg) = mz_core::extract::length_error("reason", raw.as_ref(), 5, 500) {
        errors.push(msg);
    }
    if !is_string(raw.as_ref()) {
        errors.push("reason must be a string".to_owned());
    }
    match raw {
        Some(Value::String(s)) => Some(s),
        _ => None,
    }
}

/// `ApiError::Validation` bila ada pesan.
pub fn finish(errors: Vec<String>) -> ApiResult<()> {
    if errors.is_empty() {
        Ok(())
    } else {
        Err(ApiError::Validation(errors))
    }
}

/// Nilai query string sebagai `QVal` (`None` = tidak ada kunci).
pub fn query_val<'a>(q: &'a [(String, QVal)], key: &str) -> Option<&'a QVal> {
    qs::get(q, key)
}

/// `Number(value)` untuk `@Type(() => Number)`: string tunggal → `Number(s)`; array/objek → NaN.
pub fn query_number(v: Option<&QVal>) -> Option<f64> {
    v.map(|v| match v {
        QVal::One(s) => mz_core::jsfmt::number(s),
        QVal::Many(_) | QVal::Obj => f64::NAN,
    })
}

/// Bilangan bulat JS (`Number.isInteger`).
pub fn is_js_integer(n: f64) -> bool {
    n.is_finite() && n.fract() == 0.0
}

/// `AdminListQueryDto`: `pageNumber` (`@Min(1)`, `@IsInt()`) lalu `itemsPerPage`
/// (`@Max(100)`, `@Min(1)`, `@IsInt()`), pesan dari bawah ke atas.
pub fn paging_errors(errors: &mut Vec<String>, page: Option<f64>, per: Option<f64>) {
    if let Some(n) = page {
        if n.is_nan() || n < 1.0 {
            errors.push("pageNumber must not be less than 1".to_owned());
        }
        if !is_js_integer(n) {
            errors.push("pageNumber must be an integer number".to_owned());
        }
    }
    if let Some(n) = per {
        if n.is_nan() || n > 100.0 {
            errors.push("itemsPerPage must not be greater than 100".to_owned());
        }
        if n.is_nan() || n < 1.0 {
            errors.push("itemsPerPage must not be less than 1".to_owned());
        }
        if !is_js_integer(n) {
            errors.push("itemsPerPage must be an integer number".to_owned());
        }
    }
}

/// Nilai paging setelah lolos [`paging_errors`] (default 1 dan 25).
pub fn paging_values(page: Option<f64>, per: Option<f64>) -> (i64, i64) {
    (page.map_or(1, |n| n as i64), per.map_or(25, |n| n as i64))
}

/// `skip: (pageNumber - 1) * itemsPerPage` ke Prisma. Paritas (adm-misc): Node menjawab 200
/// `result: []` untuk skip raksasa (mis. `pageNumber=2147483648`), jadi tidak pernah galat;
/// OFFSET Postgres bigint, nilai dijenuhkan.
pub fn skip_for(page: i64, per: i64) -> ApiResult<i64> {
    Ok(page.saturating_sub(1).max(0).saturating_mul(per))
}

/// Nilai angka body untuk kolom Int32 Prisma. Paritas (adm-misc): pecahan DIPOTONG seperti
/// Prisma/pg (`1.5` → 1, `-1.5` → -1), bukan galat; di luar rentang Int32 / tak hingga = galat.
pub fn int32(n: f64) -> Option<i32> {
    if n.is_finite() {
        mz_core::jsfmt::as_i32(n.trunc())
    } else {
        None
    }
}

/// Serialisasi `Float` Prisma seperti `JSON.stringify(number)` (`100`, bukan `100.0`).
pub fn js_f64<S: serde::Serializer>(v: &f64, s: S) -> Result<S::Ok, S::Error> {
    serde::Serialize::serialize(&mz_core::jsfmt::num(*v), s)
}

/// `v.success === true` (interceptor audit melewati respons ber-`success: false`).
pub fn is_success(v: &Value) -> bool {
    v.get("success") == Some(&Value::Bool(true))
}

/// String dari body (validasi sudah memastikan tipenya bila wajib).
pub fn str_of<'a>(body: &'a Value, key: &str) -> Option<&'a str> {
    field(body, key).and_then(Value::as_str)
}

/// `@IsOptional` + `?? default` untuk Int Prisma. `None` = nilai tidak bisa disimpan
/// sebagai Int32 (Prisma melempar).
pub fn int_or(body: &Value, key: &str, default: i32) -> Option<i32> {
    match field(body, key) {
        None | Some(Value::Null) => Some(default),
        Some(Value::Number(n)) => n.as_f64().and_then(int32),
        Some(_) => None,
    }
}

/// Field opsional yang bisa di-UPDATE: `undefined` = tidak diubah (`None`); `null` ke kolom
/// NOT NULL / Json non-null = galat Prisma (`None`).
pub fn updatable<'a>(body: &'a Value, key: &str) -> Option<Option<&'a Value>> {
    match field(body, key) {
        None => Some(None),
        Some(Value::Null) => None,
        Some(v) => Some(Some(v)),
    }
}

/// `ApiError` dengan body `{ code, message }` (BadRequestException Node).
pub fn coded_bad_request(code: &str, message: &str) -> ApiError {
    ApiError::body(
        axum::http::StatusCode::BAD_REQUEST,
        json!({ "code": code, "message": message }),
    )
}

/// Hasil `AdminListQueryDto` (+ allowlist `orderColumn` per controller), sudah tervalidasi.
pub struct ListQuery {
    pub page: i64,
    pub per_page: i64,
    /// `search` setelah trim (`None` bila tidak ada).
    pub search: Option<String>,
    pub order_column: &'static str,
    pub order_asc: bool,
}

/// `AdminListQueryDto` dengan pipe global (tanpa whitelist). Urutan pesan mengikuti deklarasi
/// properti: pageNumber, itemsPerPage, search, orderColumn, orderBy.
pub fn parse_list_query(
    raw: Option<&str>,
    order_columns: &'static [&'static str],
) -> ApiResult<ListQuery> {
    let q = qs::parse(raw.unwrap_or_default());
    let mut errors = Vec::new();

    let page = query_number(query_val(&q, "pageNumber"));
    let per = query_number(query_val(&q, "itemsPerPage"));
    paging_errors(&mut errors, page, per);

    // `@Transform(trim) @IsString() @MaxLength(100)` (pesan dari bawah: MaxLength, IsString).
    let search = match query_val(&q, "search") {
        None => None,
        Some(QVal::One(s)) => {
            let t = mz_core::jsfmt::trim(s);
            if validator_js_length(t) > 100 {
                errors.push("search must be shorter than or equal to 100 characters".to_owned());
            }
            Some(t.to_owned())
        }
        Some(_) => {
            errors.push("search must be shorter than or equal to 100 characters".to_owned());
            errors.push("search must be a string".to_owned());
            None
        }
    };

    // Base `@IsString()` lalu subclass `@IsIn(allowlist)`; default `createdAt`.
    let order_column = match query_val(&q, "orderColumn") {
        None => "createdAt",
        Some(QVal::One(s)) => match order_columns.iter().find(|c| **c == s.as_str()) {
            Some(c) => *c,
            None => {
                errors.push(format!(
                    "orderColumn must be one of the following values: {}",
                    order_columns.join(", ")
                ));
                "createdAt"
            }
        },
        Some(_) => {
            errors.push("orderColumn must be a string".to_owned());
            errors.push(format!(
                "orderColumn must be one of the following values: {}",
                order_columns.join(", ")
            ));
            "createdAt"
        }
    };

    let order_asc = match query_val(&q, "orderBy") {
        None => false,
        Some(QVal::One(s)) if s == "asc" => true,
        Some(QVal::One(s)) if s == "desc" => false,
        Some(_) => {
            errors.push("orderBy must be one of the following values: asc, desc".to_owned());
            false
        }
    };

    finish(errors)?;
    let (page, per_page) = paging_values(page, per);
    Ok(ListQuery {
        page,
        per_page,
        search,
        order_column,
        order_asc,
    })
}
