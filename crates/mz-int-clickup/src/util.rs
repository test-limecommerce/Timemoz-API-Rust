//! Semantik nilai JavaScript yang dipakai `clickup.service.ts` atas data API ClickUp
//! (`String(x)`, truthiness, `??`, `Number(x)`).

use serde_json::Value;

/// `x ?? <lanjut>`: `None` untuk `undefined` (tidak ada) dan `null`.
pub(crate) fn nn(v: Option<&Value>) -> Option<&Value> {
    v.filter(|x| !x.is_null())
}

/// `obj?.key` (non-objek / tidak ada → `undefined`).
pub(crate) fn field<'a>(v: Option<&'a Value>, key: &str) -> Option<&'a Value> {
    v.and_then(|x| x.get(key))
}

/// Truthiness JS.
pub(crate) fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(_) | Value::Object(_)) => true,
    }
}

/// Format angka JS (`String(1)` = `"1"`, `String(1.5)` = `"1.5"`).
pub(crate) fn js_number(n: &serde_json::Number) -> String {
    if let Some(i) = n.as_i64() {
        return i.to_string();
    }
    if let Some(u) = n.as_u64() {
        return u.to_string();
    }
    let f = n.as_f64().unwrap_or(f64::NAN);
    if f.fract() == 0.0 && f.abs() < 1e21 {
        format!("{f:.0}")
    } else {
        f.to_string()
    }
}

/// `String(x)` (`undefined` untuk tidak ada).
pub(crate) fn js_str(v: Option<&Value>) -> String {
    match v {
        None => "undefined".into(),
        Some(Value::Null) => "null".into(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => js_number(n),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) => a
            .iter()
            .map(|x| match x {
                Value::Null => String::new(),
                other => js_str(Some(other)),
            })
            .collect::<Vec<_>>()
            .join(","),
        Some(Value::Object(_)) => "[object Object]".into(),
    }
}

/// Nilai untuk kolom teks Prisma dari `x ?? null`: string apa adanya, nilai lain
/// di-`String()`-kan (Node: Prisma menolak non-string — tidak pernah dikirim ClickUp).
pub(crate) fn opt_text(v: Option<&Value>) -> Option<String> {
    nn(v).map(|x| js_str(Some(x)))
}

/// `Number(x)` untuk string/angka (nilai lain → NaN).
pub(crate) fn js_to_number(v: Option<&Value>) -> f64 {
    match v {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        Some(Value::String(s)) => str_to_number(s),
        Some(Value::Bool(b)) => f64::from(u8::from(*b)),
        Some(Value::Null) => 0.0,
        _ => f64::NAN,
    }
}

/// `Number(string)`: spasi diabaikan, kosong = 0, `0x/0o/0b`, `Infinity`.
pub(crate) fn str_to_number(s: &str) -> f64 {
    let t = s.trim();
    if t.is_empty() {
        return 0.0;
    }
    match t {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    for (prefix, radix) in [
        ("0x", 16),
        ("0X", 16),
        ("0o", 8),
        ("0O", 8),
        ("0b", 2),
        ("0B", 2),
    ] {
        if let Some(rest) = t.strip_prefix(prefix) {
            return u64::from_str_radix(rest, radix).map_or(f64::NAN, |n| n as f64);
        }
    }
    let ok = t
        .chars()
        .all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-'));
    if !ok {
        return f64::NAN;
    }
    t.parse::<f64>().unwrap_or(f64::NAN)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn js_semantics() {
        assert_eq!(js_str(Some(&json!(123))), "123");
        assert_eq!(js_str(Some(&json!(1.5))), "1.5");
        assert_eq!(js_str(None), "undefined");
        assert_eq!(js_str(Some(&json!(null))), "null");
        assert!(!truthy(Some(&json!(""))));
        assert!(!truthy(Some(&json!(0))));
        assert!(truthy(Some(&json!("0"))));
        assert!(nn(Some(&json!(null))).is_none());
        assert_eq!(str_to_number(" 12 "), 12.0);
        assert_eq!(str_to_number(""), 0.0);
        assert!(str_to_number("abc").is_nan());
        assert!(str_to_number("inf").is_nan());
        assert_eq!(str_to_number("0x10"), 16.0);
        assert_eq!(js_to_number(Some(&json!("1700000000000"))), 1.7e12);
    }
}
