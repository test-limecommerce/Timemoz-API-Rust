//! Helper kecil peniru perilaku JS/Express yang dipakai modul ini.

use axum::http::HeaderMap;
use axum::response::Response;
use chrono::{DateTime, NaiveDateTime, Utc};
use serde_json::Value;

/// `String(v)` JS untuk id dari payload Jira (angka/string).
pub(crate) fn js_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "null".to_owned(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => match (n.as_i64(), n.as_f64()) {
            (Some(i), _) => i.to_string(),
            (None, Some(f)) if f.fract() == 0.0 && f.abs() < 1e21 => format!("{f:.0}"),
            _ => n.to_string(),
        },
        Value::Array(a) => a.iter().map(js_string).collect::<Vec<_>>().join(","),
        Value::Object(_) => "[object Object]".to_owned(),
    }
}

/// Truthiness JS.
pub(crate) fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

/// `a?.b?.c` pada `Value` (bukan objek → `None`; `null` dianggap tidak ada).
pub(crate) fn path<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    let mut cur = v;
    for k in keys {
        cur = cur.as_object()?.get(*k)?;
    }
    (!cur.is_null()).then_some(cur)
}

/// `parseInt(s, 10)`; `None` = `NaN` (atau di luar `Int` Prisma).
pub(crate) fn js_parse_int(s: &str) -> Option<i32> {
    let t = s.trim_start();
    let (neg, rest) = match t.as_bytes().first() {
        Some(b'-') => (true, t.get(1..)?),
        Some(b'+') => (false, t.get(1..)?),
        _ => (false, t),
    };
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    let n: i64 = digits.parse().ok()?;
    i32::try_from(if neg { -n } else { n }).ok()
}

/// `Date.parse` untuk `fields.updated` Jira (`2026-04-30T10:00:00.000+0700`) dan ISO lain.
pub(crate) fn parse_jira_datetime(s: &str) -> Option<NaiveDateTime> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc).naive_utc());
    }
    for fmt in ["%Y-%m-%dT%H:%M:%S%.f%z", "%Y-%m-%dT%H:%M:%S%z"] {
        if let Ok(dt) = DateTime::parse_from_str(s, fmt) {
            return Some(dt.with_timezone(&Utc).naive_utc());
        }
    }
    mz_core::time::parse_js_date(s)
}

const URI_COMPONENT_SAFE: &[u8] = b"-_.!~*'()";

/// `encodeURIComponent`.
pub(crate) fn encode_uri_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || URI_COMPONENT_SAFE.contains(&b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `res.redirect(url)` Express 4 (dipakai `@Redirect()` Nest), lihat [`mz_core::redirect`].
pub(crate) fn express_redirect(headers: &HeaderMap, url: &str) -> Response {
    mz_core::redirect::redirect_302(headers, url)
}

/// `req.query` Express (`mz_core::qs`).
pub(crate) fn query_pairs(raw: Option<&str>) -> Vec<(String, mz_core::qs::QVal)> {
    mz_core::qs::parse(raw.unwrap_or_default())
}

/// Nilai query `key`: `None` = tidak ada, `Some(Ok(v))` = string,
/// `Some(Err(()))` = array/objek (`qs`).
pub(crate) fn query_one(
    pairs: &[(String, mz_core::qs::QVal)],
    key: &str,
) -> Option<Result<String, ()>> {
    match mz_core::qs::get(pairs, key)? {
        mz_core::qs::QVal::One(s) => Some(Ok(s.clone())),
        _ => Some(Err(())),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use axum::http::{HeaderValue, StatusCode, header};
    use mz_core::redirect::encode_url;
    use serde_json::json;

    #[test]
    fn js_helpers() {
        assert_eq!(js_string(&json!(10001)), "10001");
        assert_eq!(js_string(&json!("10001")), "10001");
        assert_eq!(js_parse_int(" 12abc"), Some(12));
        assert_eq!(js_parse_int("abc"), None);
        assert_eq!(js_parse_int("99999999999"), None);
        assert!(truthy(Some(&json!("x"))));
        assert!(!truthy(Some(&json!(""))));
        assert!(!truthy(Some(&json!(0))));
        assert_eq!(path(&json!({"a":{"b":1}}), &["a", "b"]), Some(&json!(1)));
        assert_eq!(path(&json!({"a":null}), &["a", "b"]), None);
    }

    #[test]
    fn encoders() {
        assert_eq!(
            encode_uri_component("read:me offline_access"),
            "read%3Ame%20offline_access"
        );
        assert_eq!(encode_uri_component("a/b?c=d&é"), "a%2Fb%3Fc%3Dd%26%C3%A9");
        assert_eq!(
            encode_url("http://x/a b?m=100%25&q=%zz"),
            "http://x/a%20b?m=100%25&q=%25zz"
        );
    }

    #[test]
    fn jira_dates() {
        let a = parse_jira_datetime("2026-04-30T10:00:00.000+0700").unwrap();
        assert_eq!(a.to_string(), "2026-04-30 03:00:00");
        let b = parse_jira_datetime("2026-04-30T10:00:00.000Z").unwrap();
        assert_eq!(b.to_string(), "2026-04-30 10:00:00");
        assert!(parse_jira_datetime("nope").is_none());
    }

    #[test]
    fn redirect_shapes() {
        let mut h = HeaderMap::new();
        let r = express_redirect(&h, "http://fe/x?message=a%20b");
        assert_eq!(r.status(), StatusCode::FOUND);
        assert_eq!(
            r.headers().get(header::LOCATION).unwrap(),
            "http://fe/x?message=a%20b"
        );
        assert!(
            r.headers()
                .get(header::CONTENT_TYPE)
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("text/plain")
        );
        h.insert(
            header::ACCEPT,
            HeaderValue::from_static("text/html,application/xhtml+xml,*/*;q=0.8"),
        );
        let r = express_redirect(&h, "http://fe/x");
        assert!(
            r.headers()
                .get(header::CONTENT_TYPE)
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("text/html")
        );
    }

    #[test]
    fn query_parsing() {
        let p = query_pairs(Some("a=1&b=x+y%2F&a=2"));
        assert_eq!(query_one(&p, "a"), Some(Err(())));
        assert_eq!(query_one(&p, "b"), Some(Ok("x y/".to_owned())));
        assert_eq!(query_one(&p, "c"), None);
    }
}
