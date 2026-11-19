//! Perilaku kecil JavaScript yang memengaruhi bentuk respons / nilai tersimpan.

use serde_json::Value;

/// `Number` JS → JSON: bilangan bulat dicetak tanpa `.0` (`10`, bukan `10.0`).
pub fn num(v: f64) -> Value {
    const MAX_SAFE: f64 = 9_007_199_254_740_991.0;
    if v.is_finite() && v.fract() == 0.0 && v.abs() <= MAX_SAFE {
        #[allow(clippy::cast_possible_truncation)]
        return Value::from(v as i64);
    }
    serde_json::Number::from_f64(v).map_or(Value::Null, Value::Number)
}

/// `String(n)` untuk angka (dipakai di template string Node).
pub fn num_string(v: f64) -> String {
    match num(v) {
        Value::Number(n) => n.to_string(),
        _ if v.is_nan() => "NaN".into(),
        _ if v > 0.0 => "Infinity".into(),
        _ => "-Infinity".into(),
    }
}

/// `${value}` — konversi template string JS untuk nilai body yang tidak divalidasi.
pub fn template(v: Option<&Value>) -> String {
    match v {
        None => "undefined".into(),
        Some(Value::Null) => "null".into(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => n.as_f64().map_or_else(|| n.to_string(), num_string),
        Some(Value::Array(items)) => items
            .iter()
            .map(|i| match i {
                Value::Null => String::new(),
                other => template(Some(other)),
            })
            .collect::<Vec<_>>()
            .join(","),
        Some(Value::Object(_)) => "[object Object]".into(),
    }
}

/// Truthiness JS.
pub fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Some(Value::Array(_) | Value::Object(_)) => true,
    }
}

/// `value || fallback` untuk string opsional yang sudah lolos `@IsString`.
pub fn or_str<'a>(v: Option<&'a Value>, fallback: &'a str) -> &'a str {
    match v {
        Some(Value::String(s)) if !s.is_empty() => s,
        _ => fallback,
    }
}

/// `str.length` (unit UTF-16).
pub fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// `parseInt(raw, 10)`; `None` = `NaN`.
pub fn parse_int(raw: Option<&str>) -> Option<i64> {
    let t = raw?.trim_start();
    let (sign, digits) = match t.strip_prefix('-') {
        Some(rest) => (-1, rest),
        None => (1, t.strip_prefix('+').unwrap_or(t)),
    };
    let end = digits
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(digits.len());
    let n: i64 = digits.get(..end).filter(|d| !d.is_empty())?.parse().ok()?;
    Some(sign * n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn numbers_print_like_js() {
        assert_eq!(num(10.0), json!(10));
        assert_eq!(num(9.99), json!(9.99));
        assert_eq!(num_string(5.0), "5");
        assert_eq!(num_string(5.5), "5.5");
    }

    #[test]
    fn template_strings() {
        assert_eq!(template(None), "undefined");
        assert_eq!(template(Some(&json!("Acme"))), "Acme");
        assert_eq!(template(Some(&json!(3))), "3");
        assert_eq!(template(Some(&json!({"a":1}))), "[object Object]");
        assert_eq!(template(Some(&json!(["a", null, 1]))), "a,,1");
    }

    #[test]
    fn parse_int_semantics() {
        assert_eq!(parse_int(Some("300")), Some(300));
        assert_eq!(parse_int(Some(" 42abc")), Some(42));
        assert_eq!(parse_int(Some("abc")), None);
        assert_eq!(parse_int(None), None);
    }

    #[test]
    fn truthiness() {
        assert!(!truthy(Some(&json!(""))));
        assert!(truthy(Some(&json!([]))));
        assert!(!truthy(Some(&json!(0))));
    }
}
