//! Helper semantik JS (tanggal lokal proses = UTC, `parseInt`, `URLSearchParams`, redirect Express).

use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime};
use mz_core::jsfmt;
use serde_json::{Map, Value};

/// `new Date(y, m0, d, time)` dengan overflow bulan/hari ala JS (zona proses = UTC).
fn js_compose(year: i64, month0: i64, day: i64, time: NaiveTime) -> Option<NaiveDateTime> {
    let y = year + month0.div_euclid(12);
    let m = u32::try_from(month0.rem_euclid(12) + 1).ok()?;
    let first = NaiveDate::from_ymd_opt(i32::try_from(y).ok()?, m, 1)?;
    let date = first.checked_add_signed(Duration::days(day - 1))?;
    Some(date.and_time(time))
}

/// `d.setFullYear(d.getFullYear() + n)` (29 Feb → 1 Mar seperti JS).
pub fn add_years(dt: NaiveDateTime, n: i64) -> NaiveDateTime {
    js_compose(
        i64::from(dt.year()) + n,
        i64::from(dt.month0()),
        i64::from(dt.day()),
        dt.time(),
    )
    .unwrap_or(dt)
}

/// `d.setDate(d.getDate() + n)`.
pub fn add_days(dt: NaiveDateTime, n: i64) -> NaiveDateTime {
    dt.checked_add_signed(Duration::days(n)).unwrap_or(dt)
}

/// `x = new Date(); x.setMonth(x.getMonth() + 1); x.setDate(1); x.setHours(0, 0, 0, 0)`.
/// Hari yang meluap ikut JS: 31 Jan → "31 Feb" = 3 Mar → 1 Mar.
pub fn next_month_first(now: NaiveDateTime) -> NaiveDateTime {
    let bumped = js_compose(
        i64::from(now.year()),
        i64::from(now.month0()) + 1,
        i64::from(now.day()),
        now.time(),
    )
    .unwrap_or(now);
    js_compose(
        i64::from(bumped.year()),
        i64::from(bumped.month0()),
        1,
        NaiveTime::MIN,
    )
    .unwrap_or(bumped)
}

/// `nextInvoiceDate` handlePaymentCompleted (paypal.controller.ts:1052 & 1279).
pub fn next_invoice_date(now: NaiveDateTime, end: NaiveDateTime, yearly: bool) -> NaiveDateTime {
    let renewal_check = add_days(end, -3);
    if yearly {
        let next_month = next_month_first(now);
        if next_month < renewal_check {
            next_month
        } else {
            renewal_check
        }
    } else {
        renewal_check
    }
}

/// Akhir periode: tahunan `setFullYear(+1)`, selain itu `setDate(+30)`.
pub fn period_end(start: NaiveDateTime, yearly: bool) -> NaiveDateTime {
    if yearly {
        add_years(start, 1)
    } else {
        add_days(start, 30)
    }
}

/// `formatDate` controller: `dd-mm-yyyy`.
pub fn format_dmy(dt: NaiveDateTime) -> String {
    dt.format("%d-%m-%Y").to_string()
}

/// `date.toISOString().slice(0, 10)`.
pub fn iso_day(dt: NaiveDateTime) -> String {
    dt.format("%Y-%m-%d").to_string()
}

/// `String(v)` untuk nilai JSON (dipakai `parseInt(v)`).
fn js_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.as_f64().map_or_else(|| n.to_string(), jsfmt::num_string),
        Value::Bool(b) => b.to_string(),
        Value::Null => "null".into(),
        Value::Array(a) => a
            .iter()
            .map(|x| match x {
                Value::Null => String::new(),
                other => js_string(other),
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".into(),
    }
}

/// `parseInt(v)` (`None` = NaN). `None` input = `undefined`.
pub fn js_parse_int(v: Option<&Value>) -> Option<i64> {
    v.and_then(|v| jsfmt::parse_int(&js_string(v)))
}

/// `parseInt(a) || b || 1` untuk kolom Int.
pub fn quantity_or(resource_qty: Option<&Value>, local: i32) -> i64 {
    match js_parse_int(resource_qty) {
        Some(n) if n != 0 => n,
        _ if local != 0 => i64::from(local),
        _ => 1,
    }
}

/// String JS non-kosong (`x || fallback` untuk string).
pub fn truthy_str(v: Option<&Value>) -> Option<&str> {
    v.and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// `Math.round`: setengah dibulatkan ke +∞.
pub fn js_round(x: f64) -> f64 {
    (x + 0.5).floor()
}

/// Bentuk angka `JSON.parse` → `JSON.stringify` (bilangan bulat `1.0` → `1`), rekursif.
pub fn js_normalize(v: &Value) -> Value {
    match v {
        Value::Number(n) if n.is_f64() => n.as_f64().map_or(Value::Null, jsfmt::num),
        Value::Array(a) => Value::Array(a.iter().map(js_normalize).collect()),
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, v)| (k.clone(), js_normalize(v)))
                .collect::<Map<_, _>>(),
        ),
        other => other.clone(),
    }
}

/// `application/x-www-form-urlencoded` ala `URLSearchParams.toString()`.
pub fn form_encode(pairs: &[(&str, &str)]) -> String {
    fn enc(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for b in s.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'*' | b'-' | b'.' | b'_' => {
                    out.push(char::from(b));
                }
                b' ' => out.push('+'),
                _ => out.push_str(&format!("%{b:02X}")),
            }
        }
        out
    }
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", enc(k), enc(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// `res.redirect(302, url)` Express — satu implementasi di `mz_core::redirect`.
pub use mz_core::redirect::redirect_302;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn dt(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").unwrap_or_default()
    }

    #[test]
    fn js_date_arithmetic() {
        assert_eq!(
            add_years(dt("2028-02-29 10:00:00"), 1),
            dt("2029-03-01 10:00:00")
        );
        assert_eq!(
            add_days(dt("2026-12-15 08:30:00"), 30),
            dt("2027-01-14 08:30:00")
        );
        // 31 Jan → setMonth(1) = 3 Mar → setDate(1) = 1 Mar.
        assert_eq!(
            next_month_first(dt("2027-01-31 13:00:00")),
            dt("2027-03-01 00:00:00")
        );
        assert_eq!(
            next_month_first(dt("2026-12-10 13:00:00")),
            dt("2027-01-01 00:00:00")
        );
        let now = dt("2026-11-17 10:00:00");
        assert_eq!(
            next_invoice_date(now, period_end(now, true), true),
            dt("2026-12-01 00:00:00")
        );
        assert_eq!(
            next_invoice_date(now, period_end(now, false), false),
            dt("2026-12-14 10:00:00")
        );
        assert_eq!(format_dmy(now), "17-11-2026");
        assert_eq!(iso_day(now), "2026-11-17");
    }

    #[test]
    fn parse_int_like_js() {
        assert_eq!(js_parse_int(Some(&json!("3"))), Some(3));
        assert_eq!(js_parse_int(Some(&json!(" 12abc"))), Some(12));
        assert_eq!(js_parse_int(Some(&json!(3.7))), Some(3));
        assert_eq!(js_parse_int(Some(&json!("abc"))), None);
        assert_eq!(js_parse_int(None), None);
        assert_eq!(quantity_or(Some(&json!("0")), 4), 4);
        assert_eq!(quantity_or(None, 0), 1);
        assert_eq!(quantity_or(Some(&json!("7")), 4), 7);
    }

    #[test]
    fn misc_js() {
        assert!((js_round(2.5) - 3.0).abs() < f64::EPSILON);
        assert!((js_round(-2.5) + 2.0).abs() < f64::EPSILON);
        assert_eq!(
            form_encode(&[("proration", "success"), ("invoice", "INV-1 a&b")]),
            "proration=success&invoice=INV-1+a%26b"
        );
        let v: Value = serde_json::from_str(r#"{"b":1.0,"a":[2.5,3.0]}"#).unwrap_or_default();
        assert_eq!(js_normalize(&v).to_string(), r#"{"b":1,"a":[2.5,3]}"#);
    }
}
