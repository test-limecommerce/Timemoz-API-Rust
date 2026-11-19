//! Format nilai seperti JS/moment untuk generator PDF.

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::value_util::{parse_calendar_date, zone};

/// `String(value)` JS untuk nilai JSON.
pub fn js_string(v: &Value) -> String {
    match v {
        Value::Null => "null".to_owned(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n
            .as_f64()
            .map(mz_core::jsfmt::num_string)
            .unwrap_or_default(),
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .map(|i| match i {
                Value::Null => String::new(),
                other => js_string(other),
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".to_owned(),
    }
}

/// `new Intl.NumberFormat('en-US', { maximumFractionDigits: 20 }).format(x)`.
///
/// ICU memakai digit representasi terpendek, dibulatkan ke 20 digit pecahan, dengan
/// pemisah ribuan `,`.
pub fn intl_number(x: f64) -> String {
    if x.is_nan() {
        return "NaN".to_owned();
    }
    if x.is_infinite() {
        return if x > 0.0 { "∞" } else { "-∞" }.to_owned();
    }
    let negative = x.is_sign_negative() && x != 0.0;
    let repr = format!("{:e}", x.abs());
    let (mantissa, exponent) = repr.split_once('e').unwrap_or((repr.as_str(), "0"));
    let exp: i64 = exponent.parse().unwrap_or(0);
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    // Nilai = 0.DIGITS × 10^(exp+1).
    let point = exp + 1;
    let (mut int_part, mut frac_part) = if point <= 0 {
        (
            "0".to_owned(),
            format!(
                "{}{digits}",
                "0".repeat(usize::try_from(-point).unwrap_or(0))
            ),
        )
    } else {
        let p = usize::try_from(point).unwrap_or(0);
        if digits.len() <= p {
            (
                format!("{digits}{}", "0".repeat(p - digits.len())),
                String::new(),
            )
        } else {
            (
                digits.get(..p).unwrap_or("").to_owned(),
                digits.get(p..).unwrap_or("").to_owned(),
            )
        }
    };
    if frac_part.len() > 20 {
        // Bulatkan ke 20 digit (half-even ICU; kasus ini praktis hanya < 1e-20).
        let round_up = frac_part.as_bytes().get(20).is_some_and(|b| *b >= b'5');
        frac_part.truncate(20);
        if round_up {
            let mut bytes = format!("{int_part}{frac_part}").into_bytes();
            let mut i = bytes.len();
            loop {
                if i == 0 {
                    bytes.insert(0, b'1');
                    break;
                }
                i -= 1;
                if let Some(b) = bytes.get_mut(i) {
                    if *b == b'9' {
                        *b = b'0';
                    } else {
                        *b += 1;
                        break;
                    }
                }
            }
            let s = String::from_utf8(bytes).unwrap_or_default();
            let split = s.len() - 20;
            int_part = s.get(..split).unwrap_or("0").to_owned();
            frac_part = s.get(split..).unwrap_or("").to_owned();
        }
    }
    let frac_trimmed = frac_part.trim_end_matches('0');
    let int_trimmed = int_part.trim_start_matches('0');
    let int_digits = if int_trimmed.is_empty() {
        "0"
    } else {
        int_trimmed
    };
    let mut grouped = String::new();
    for (i, c) in int_digits.chars().enumerate() {
        if i > 0 && (int_digits.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(c);
    }
    let sign = if negative && !(int_digits == "0" && frac_trimmed.is_empty()) {
        "-"
    } else {
        ""
    };
    if frac_trimmed.is_empty() {
        format!("{sign}{grouped}")
    } else {
        format!("{sign}{grouped}.{frac_trimmed}")
    }
}

/// `moment(date).tz(tz).format('D MMM YYYY HH:mm')`.
pub fn format_instant(date: DateTime<Utc>, time_zone: &str) -> String {
    date.with_timezone(&zone(time_zone))
        .format("%-d %b %Y %H:%M")
        .to_string()
}

/// [`format_instant`] untuk epoch ms (nilai `datetime` di `answers`).
pub fn format_epoch_ms(ms: f64, time_zone: &str) -> String {
    if !ms.is_finite() {
        return "Invalid date".to_owned();
    }
    DateTime::<Utc>::from_timestamp_millis(ms as i64).map_or_else(
        || "Invalid date".to_owned(),
        |d| format_instant(d, time_zone),
    )
}

/// `moment(value, 'YYYY-MM-DD', true)` valid → `D MMM YYYY`.
pub fn format_date(raw: &str) -> Option<String> {
    parse_calendar_date(raw).map(|d| d.format("%-d %b %Y").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn intl() {
        assert_eq!(intl_number(150000.0), "150,000");
        assert_eq!(intl_number(0.1 + 0.2), "0.30000000000000004");
        assert_eq!(intl_number(1234.5), "1,234.5");
        assert_eq!(intl_number(-1234.567), "-1,234.567");
        assert_eq!(intl_number(1e21), "1,000,000,000,000,000,000,000");
        assert_eq!(intl_number(1.5e-7), "0.00000015");
        assert_eq!(intl_number(123456789.12345679), "123,456,789.12345679");
        assert_eq!(intl_number(9007199254740994.0), "9,007,199,254,740,994");
        assert_eq!(intl_number(1e-21), "0");
        assert_eq!(intl_number(0.0), "0");
        assert_eq!(intl_number(42.0), "42");
    }

    #[test]
    fn strings_and_dates() {
        assert_eq!(js_string(&json!([1, null, "a"])), "1,,a");
        assert_eq!(js_string(&json!({"a": 1})), "[object Object]");
        assert_eq!(js_string(&json!(2.5)), "2.5");
        assert_eq!(format_date("2026-08-20").as_deref(), Some("20 Aug 2026"));
        assert_eq!(format_date("2026-02-30"), None);
        let d = DateTime::parse_from_rfc3339("2026-08-20T07:05:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(format_instant(d, "Asia/Jakarta"), "20 Aug 2026 14:05");
        assert_eq!(
            format_epoch_ms(1_787_236_200_000.0, "UTC"),
            "20 Aug 2026 14:30"
        );
    }
}
