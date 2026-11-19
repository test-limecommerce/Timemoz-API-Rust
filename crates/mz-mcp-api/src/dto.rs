//! Validasi query gaya `ValidationPipe({ transform: true })` (class-validator, tanpa whitelist)
//! untuk DTO `src/mcp-api/**/dto/*.ts`. Urutan pesan: properti kelas turunan dulu, lalu yang
//! diwarisi (`PaginationDto`); per properti = dekorator dari BAWAH ke ATAS.

use mz_admin_mcp::body::{V, is_nullish, query_map};
use mz_core::ApiResult;
use mz_core::iso8601::is_iso8601;
use serde_json::{Map, Value};

/// `req.query` (parser `qs` Express).
pub fn query_of(raw: Option<&str>) -> Map<String, Value> {
    query_map(raw)
}

/// `@IsOptional() @IsString()` lalu nilai string yang tidak kosong (`if (q.x)`).
pub fn truthy_str<'a>(q: &'a Map<String, Value>, k: &str) -> Option<&'a str> {
    match q.get(k) {
        Some(Value::String(s)) if !s.is_empty() => Some(s),
        _ => None,
    }
}

/// Nilai string apa adanya (setelah divalidasi).
pub fn str_of<'a>(q: &'a Map<String, Value>, k: &str) -> Option<&'a str> {
    match q.get(k) {
        Some(Value::String(s)) => Some(s),
        _ => None,
    }
}

fn date_msg(f: &str) -> String {
    format!("{f} must be a valid ISO 8601 date string")
}

/// `@IsDateString()`.
pub fn date_string(v: &mut V, f: &str, val: Option<&Value>) {
    if !matches!(val, Some(Value::String(s)) if is_iso8601(s)) {
        v.errors.push(date_msg(f));
    }
}

/// `@IsOptional() @IsDateString()`.
pub fn opt_date_string(v: &mut V, f: &str, val: Option<&Value>) {
    if !is_nullish(val) {
        date_string(v, f, val);
    }
}

/// `@IsNotEmpty() @IsDateString()` (urutan pesan: IsDateString dulu, lalu IsNotEmpty).
pub fn required_date_not_empty(v: &mut V, f: &str, val: Option<&Value>) {
    date_string(v, f, val);
    if is_nullish(val) || matches!(val, Some(Value::String(s)) if s.is_empty()) {
        v.errors.push(format!("{f} should not be empty"));
    }
}

/// `@Type(() => Number)`: string tunggal -> `Number(s)`; array/objek -> bukan bilangan.
fn js_number(val: &Value) -> Option<f64> {
    match val {
        Value::String(s) => Some(mz_core::jsfmt::number(s)),
        _ => None,
    }
}

/// `@IsOptional() @Type(() => Number) @IsInt() @Min(min) [@Max(max)]` (urutan pesan: Max, Min,
/// IsInt). `None` = tidak dikirim (default dipakai) ATAU tidak valid (pesan sudah dicatat).
pub fn opt_int(v: &mut V, f: &str, val: Option<&Value>, min: i64, max: Option<i64>) -> Option<i64> {
    let val = val.filter(|x| !x.is_null())?;
    let num = js_number(val);
    let before = v.errors.len();
    #[allow(clippy::cast_precision_loss)]
    {
        if let Some(max) = max
            && num.is_none_or(|n| n.is_nan() || n > max as f64)
        {
            v.errors.push(format!("{f} must not be greater than {max}"));
        }
        if num.is_none_or(|n| n.is_nan() || n < min as f64) {
            v.errors.push(format!("{f} must not be less than {min}"));
        }
    }
    let int = num.filter(|n| n.is_finite() && n.fract() == 0.0);
    if int.is_none() {
        v.errors.push(format!("{f} must be an integer number"));
    }
    #[allow(clippy::cast_possible_truncation)]
    int.filter(|_| v.errors.len() == before).map(|n| n as i64)
}

/// `PaginationDto`: `page` (min 1) dan `perPage` (min 1, max 500), default 1 / 500.
pub fn pagination(
    v: &mut V,
    q: &Map<String, Value>,
    default_per_page: i64,
    max_per_page: i64,
) -> (i64, i64) {
    let page = opt_int(v, "page", q.get("page"), 1, None);
    let per_page = opt_int(v, "perPage", q.get("perPage"), 1, Some(max_per_page));
    (page.unwrap_or(1), per_page.unwrap_or(default_per_page))
}

/// `@IsOptional() @IsUUID()`: kunci `k` bila ada.
pub fn opt_uuid(v: &mut V, q: &Map<String, Value>, k: &str) {
    if !is_nullish(q.get(k)) {
        v.uuid(k, q.get(k));
    }
}

/// Selesaikan validasi -> 400 daftar pesan.
pub fn finish(v: V) -> ApiResult<()> {
    v.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn run(f: impl FnOnce(&mut V)) -> Vec<String> {
        let mut v = V::default();
        f(&mut v);
        v.errors
    }

    #[test]
    fn int_rules_and_order() {
        assert_eq!(
            run(|v| {
                opt_int(v, "perPage", Some(&json!("abc")), 1, Some(500));
            }),
            [
                "perPage must not be greater than 500",
                "perPage must not be less than 1",
                "perPage must be an integer number"
            ]
        );
        assert_eq!(
            run(|v| {
                opt_int(v, "page", Some(&json!("0")), 1, None);
            }),
            ["page must not be less than 1"]
        );
        assert_eq!(
            run(|v| {
                opt_int(v, "page", Some(&json!("1.5")), 1, None);
            }),
            ["page must be an integer number"]
        );
        assert_eq!(
            run(|v| {
                opt_int(v, "page", Some(&json!("")), 1, None);
            }),
            ["page must not be less than 1"]
        );
        // array (page=1&page=2) = bukan bilangan
        assert_eq!(
            run(|v| {
                opt_int(v, "page", Some(&json!(["1", "2"])), 1, None);
            })
            .len(),
            2
        );
        let mut v = V::default();
        assert_eq!(
            opt_int(&mut v, "n", Some(&json!(" 7 ")), 1, Some(9)),
            Some(7)
        );
        assert_eq!(opt_int(&mut v, "n", None, 1, Some(9)), None);
        assert!(v.errors.is_empty());
    }

    #[test]
    fn date_rules() {
        assert!(run(|v| date_string(v, "d", Some(&json!("2026-11-16")))).is_empty());
        assert_eq!(
            run(|v| required_date_not_empty(v, "dateFrom", None)),
            [
                "dateFrom must be a valid ISO 8601 date string",
                "dateFrom should not be empty"
            ]
        );
        assert_eq!(
            run(|v| required_date_not_empty(v, "dateFrom", Some(&json!("nope")))),
            ["dateFrom must be a valid ISO 8601 date string"]
        );
        assert!(run(|v| opt_date_string(v, "d", None)).is_empty());
    }

    #[test]
    fn pagination_defaults() {
        let q = query_of(Some("page=2&perPage=10"));
        let mut v = V::default();
        assert_eq!(pagination(&mut v, &q, 500, 500), (2, 10));
        let q = query_of(None);
        assert_eq!(pagination(&mut v, &q, 50, 200), (1, 50));
        assert!(v.errors.is_empty());
    }
}
