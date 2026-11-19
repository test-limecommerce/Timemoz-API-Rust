//! Padanan kecil perilaku JavaScript yang dibutuhkan port ini (encoding URL, `Number()`,
//! `new Date(string)`, serialisasi angka JSON).

use chrono::{NaiveDate, NaiveDateTime};
use serde::{Serialize, Serializer};
use serde_json::Value;

/// `encodeURIComponent`.
pub fn encode_uri_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        let keep = b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
            );
        if keep {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `new URLSearchParams(obj).toString()` (application/x-www-form-urlencoded WHATWG).
pub fn form_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'*' | b'-' | b'.' | b'_') {
            out.push(char::from(b));
        } else if b == b' ' {
            out.push('+');
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `URLSearchParams` dari pasangan berurutan.
pub fn search_params(pairs: &[(&str, String)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", form_encode(k), form_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// `encode` milik `axios/lib/helpers/buildURL.js` (axios 1.x) untuk `params`.
pub fn axios_encode(s: &str) -> String {
    encode_uri_component(s)
        .replace("%3A", ":")
        .replace("%24", "$")
        .replace("%2C", ",")
        .replace("%20", "+")
}

/// Query string bergaya `axios` `params` (nilai `None` = `undefined`, dilewati).
pub fn axios_query(params: &[(&str, Option<String>)]) -> String {
    params
        .iter()
        .filter_map(|(k, v)| {
            v.as_ref()
                .map(|v| format!("{}={}", axios_encode(k), axios_encode(v)))
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// Query Express (`qs`): satu sumber di `mz_core::qs`.
pub use mz_core::qs::{QVal, get as query_get};

/// `req.query` dari query mentah.
/// `Number(string)` JavaScript: satu sumber di `mz_core::jsfmt`.
pub use mz_core::jsfmt::number as js_number_str;

pub fn parse_query(raw: Option<&str>) -> Vec<(String, QVal)> {
    mz_core::qs::parse(raw.unwrap_or_default())
}

/// `Number(value)` untuk nilai JSON.
pub fn js_number(v: &Value) -> f64 {
    match v {
        Value::Null => 0.0,
        Value::Bool(b) => f64::from(u8::from(*b)),
        Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
        Value::String(s) => js_number_str(s),
        Value::Array(a) => match a.as_slice() {
            [] => 0.0,
            [one] => match one {
                Value::Array(_) | Value::Object(_) => f64::NAN,
                Value::Null => 0.0,
                other => js_number(&Value::String(js_string(other))),
            },
            _ => f64::NAN,
        },
        Value::Object(_) => f64::NAN,
    }
}

/// Interpolasi template literal JavaScript (`${v}`) untuk nilai JSON (`undefined` bila `None`).
pub fn js_template(v: Option<&Value>) -> String {
    match v {
        None => "undefined".into(),
        Some(v) => js_string(v),
    }
}

/// `String(v)`.
pub fn js_string(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n
            .as_f64()
            .map_or_else(|| n.to_string(), |f| JsNum(f).to_js_string()),
        Value::String(s) => s.clone(),
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

/// Truthiness JavaScript untuk nilai JSON (`None` = `undefined`).
pub fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

/// Angka yang diserialisasi seperti `JSON.stringify` (bilangan bulat tanpa `.0`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct JsNum(pub f64);

impl JsNum {
    fn as_int(self) -> Option<i64> {
        #[allow(clippy::cast_possible_truncation)]
        (self.0.is_finite() && self.0.fract() == 0.0 && self.0.abs() < 9.0e15)
            .then_some(self.0 as i64)
    }

    pub fn to_js_string(self) -> String {
        match self.as_int() {
            Some(i) => i.to_string(),
            None if self.0.is_nan() => "NaN".into(),
            None => self.0.to_string(),
        }
    }
}

impl Serialize for JsNum {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self.as_int() {
            Some(i) => s.serialize_i64(i),
            // JSON.stringify(NaN/Infinity) = null.
            None if !self.0.is_finite() => s.serialize_none(),
            None => s.serialize_f64(self.0),
        }
    }
}

/// Komponen hasil parse tanggal ISO 8601.
struct IsoParts {
    year: i32,
    month: u32,
    day: u32,
    time: Option<(u32, u32, u32, u32)>,
    /// Offset menit timur UTC; `None` = tanpa zona.
    offset_min: Option<i32>,
}

fn take_digits(s: &[u8], pos: &mut usize, n: usize) -> Option<u32> {
    let slice = s.get(*pos..*pos + n)?;
    if !slice.iter().all(u8::is_ascii_digit) {
        return None;
    }
    *pos += n;
    std::str::from_utf8(slice).ok()?.parse().ok()
}

fn parse_iso_parts(raw: &str) -> Option<IsoParts> {
    let s = raw.as_bytes();
    let mut p = 0usize;
    let neg = match s.first() {
        Some(b'+') => {
            p += 1;
            false
        }
        Some(b'-') => {
            p += 1;
            true
        }
        _ => false,
    };
    let y = i32::try_from(take_digits(s, &mut p, 4)?).ok()?;
    let year = if neg { -y } else { y };
    let (mut month, mut day) = (1, 1);
    if p < s.len() && s.get(p) == Some(&b'-') {
        p += 1;
        month = take_digits(s, &mut p, 2)?;
        if s.get(p) == Some(&b'-') {
            p += 1;
            day = take_digits(s, &mut p, 2)?;
        }
    } else if s.get(p).is_some_and(u8::is_ascii_digit) {
        // `YYYYMMDD`.
        month = take_digits(s, &mut p, 2)?;
        day = take_digits(s, &mut p, 2)?;
    }
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let mut time = None;
    let mut offset_min = None;
    if matches!(s.get(p), Some(b'T' | b' ' | b't')) && p + 1 < s.len() {
        p += 1;
        let h = take_digits(s, &mut p, 2)?;
        let (mut mi, mut sec, mut ms) = (0, 0, 0);
        let colon = |p: &mut usize| {
            if s.get(*p) == Some(&b':') {
                *p += 1;
            }
        };
        if s.get(p).is_some_and(|b| *b == b':' || b.is_ascii_digit()) {
            colon(&mut p);
            mi = take_digits(s, &mut p, 2)?;
            if s.get(p).is_some_and(|b| *b == b':' || b.is_ascii_digit()) {
                colon(&mut p);
                sec = take_digits(s, &mut p, 2)?;
            }
        }
        if matches!(s.get(p), Some(b'.' | b',')) {
            p += 1;
            let start = p;
            while s.get(p).is_some_and(u8::is_ascii_digit) {
                p += 1;
            }
            let frac = std::str::from_utf8(s.get(start..p)?).ok()?;
            if frac.is_empty() {
                return None;
            }
            let ms3: String = frac.chars().chain("000".chars()).take(3).collect();
            ms = ms3.parse().ok()?;
        }
        if h > 24 || mi > 59 || sec > 59 || (h == 24 && (mi, sec, ms) != (0, 0, 0)) {
            return None;
        }
        time = Some((h, mi, sec, ms));
        match s.get(p) {
            Some(b'Z' | b'z') => {
                p += 1;
                offset_min = Some(0);
            }
            Some(&sign @ (b'+' | b'-')) => {
                p += 1;
                let oh = take_digits(s, &mut p, 2)?;
                if s.get(p) == Some(&b':') {
                    p += 1;
                }
                let om = if s.get(p).is_some_and(u8::is_ascii_digit) {
                    take_digits(s, &mut p, 2)?
                } else {
                    0
                };
                if oh > 23 || om > 59 {
                    return None;
                }
                let total = i32::try_from(oh * 60 + om).ok()?;
                offset_min = Some(if sign == b'-' { -total } else { total });
            }
            _ => {}
        }
    }
    (p == s.len()).then_some(IsoParts {
        year,
        month,
        day,
        time,
        offset_min,
    })
}

/// `new Date(s)` untuk string ISO 8601: tanggal saja = UTC, tanggal+jam tanpa zona = waktu
/// lokal proses Node (produksi `TZ=UTC`). `None` = `Invalid Date`.
pub fn parse_js_date(s: &str) -> Option<NaiveDateTime> {
    let p = parse_iso_parts(s.trim())?;
    let date = NaiveDate::from_ymd_opt(p.year, p.month, p.day)?;
    let (h, mi, sec, ms) = p.time.unwrap_or((0, 0, 0, 0));
    let base = if h == 24 {
        date.and_hms_milli_opt(0, mi, sec, ms)? + chrono::Duration::days(1)
    } else {
        date.and_hms_milli_opt(h, mi, sec, ms)?
    };
    Some(base - chrono::Duration::minutes(i64::from(p.offset_min.unwrap_or(0))))
}

/// Isi `<a href>`/teks HTML (`escape-html`).
pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("&quot;"),
            '&' => out.push_str("&amp;"),
            '\'' => out.push_str("&#39;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn uri_component() {
        assert_eq!(
            encode_uri_component("a b:c/é!'()*~"),
            "a%20b%3Ac%2F%C3%A9!'()*~"
        );
        assert_eq!(form_encode("a b:c/*"), "a+b%3Ac%2F*");
        assert_eq!(
            axios_encode("2026-01-01T00:00:00.000Z"),
            "2026-01-01T00:00:00.000Z"
        );
        assert_eq!(axios_encode("a b,c$"), "a+b,c$");
    }

    #[test]
    fn query_parse() {
        let q = parse_query(Some("a=1&b=x+y&a=2&c"));
        assert_eq!(
            query_get(&q, "a"),
            Some(&QVal::Many(vec!["1".into(), "2".into()]))
        );
        assert_eq!(query_get(&q, "b"), Some(&QVal::One("x y".into())));
        assert_eq!(query_get(&q, "c"), Some(&QVal::One(String::new())));
    }

    #[test]
    fn numbers() {
        assert_eq!(js_number_str(""), 0.0);
        assert_eq!(js_number_str(" 12 "), 12.0);
        assert!(js_number_str("12abc").is_nan());
        assert!(js_number_str("nan").is_nan());
        assert_eq!(js_number(&json!(true)), 1.0);
        assert_eq!(serde_json::to_string(&JsNum(1.0)).unwrap(), "1");
        assert_eq!(serde_json::to_string(&JsNum(1.5)).unwrap(), "1.5");
        assert_eq!(js_template(None), "undefined");
        assert_eq!(js_template(Some(&json!(12))), "12");
    }

    #[test]
    fn dates() {
        let d = |s| parse_js_date(s).map(|d| d.format("%Y-%m-%dT%H:%M:%S%.3f").to_string());
        assert_eq!(
            d("2026-05-20T09:00:00+07:00").as_deref(),
            Some("2026-05-20T02:00:00.000")
        );
        assert_eq!(d("2026-05-20").as_deref(), Some("2026-05-20T00:00:00.000"));
        assert_eq!(
            d("2026-05-20T09:00:00.1234Z").as_deref(),
            Some("2026-05-20T09:00:00.123")
        );
        assert_eq!(d("2026-05").as_deref(), Some("2026-05-01T00:00:00.000"));
        assert!(d("2026-02-31").is_none());
    }
}
