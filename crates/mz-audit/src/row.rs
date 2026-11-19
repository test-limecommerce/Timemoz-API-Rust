//! Baris snapshot (`AuditRow` di Node): peta kolom → nilai JSON yang URUTAN kuncinya
//! dipertahankan.
//!
//! Urutan penting di dua tempat: `changedFields` sebuah `create` adalah
//! `Object.keys(after)` — urutan kolom hasil Prisma (= urutan deklarasi di
//! `schema.prisma`) — dan respons yang mengirim baris apa adanya (mis. mz-companies).
//! Baris disimpan sebagai `Vec` berpasangan (urutan penyisipan, posisi kunci lama
//! dipertahankan saat ditimpa seperti objek JS); `Serialize`/[`Row::to_json`] mengikuti
//! urutan itu (workspace memakai `serde_json` dengan fitur `preserve_order`).
//!
//! Nilai disimpan dalam bentuk yang SAMA dengan yang ditulis Node ke kolom `Json`:
//! `DateTime` → ISO string ber-ms + `Z` (`Date.toISOString()`), `Decimal` → string
//! (`Decimal.toJSON()`), `BigInt` → string, sisanya apa adanya.

use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime};
use serde::ser::{Serialize, SerializeMap, Serializer};
use serde_json::{Map, Value};

/// Jenis kolom Prisma, untuk menormalkan nilai yang dibaca dari Postgres.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Int,
    BigInt,
    Float,
    Decimal,
    Text,
    Bool,
    /// `DateTime` (`timestamp(3)` tanpa zona).
    Timestamp,
    /// `DateTime @db.Timestamptz`.
    Timestamptz,
    /// `DateTime @db.Date` — Prisma mengembalikan tengah malam UTC.
    Date,
    /// `DateTime @db.Time` — Prisma mengembalikan `1970-01-01T..Z`.
    Time,
    Json,
    Enum,
    Bytes,
}

/// Satu baris (`Record<string, unknown>`), kunci terurut sesuai penyisipan.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Row(Vec<(String, Value)>);

impl Row {
    pub fn new() -> Self {
        Self(Vec::new())
    }

    /// Builder: `Row::new().with("id", 5).with("name", "Budi")`.
    #[must_use]
    pub fn with(mut self, field: &str, value: impl Into<Value>) -> Self {
        self.set(field, value);
        self
    }

    /// `row[field] = value` (posisi kunci yang sudah ada dipertahankan, seperti objek JS).
    pub fn set(&mut self, field: &str, value: impl Into<Value>) {
        let value = normalize_number(value.into());
        if let Some(slot) = self.0.iter_mut().find(|(k, _)| k == field) {
            slot.1 = value;
        } else {
            self.0.push((field.to_owned(), value));
        }
    }

    pub fn get(&self, field: &str) -> Option<&Value> {
        self.0.iter().find(|(k, _)| k == field).map(|(_, v)| v)
    }

    /// `field in row`.
    pub fn contains(&self, field: &str) -> bool {
        self.0.iter().any(|(k, _)| k == field)
    }

    pub fn remove(&mut self, field: &str) -> Option<Value> {
        let idx = self.0.iter().position(|(k, _)| k == field)?;
        Some(self.0.remove(idx).1)
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(|(k, _)| k.as_str())
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &Value)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v))
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// `{ ...self, ...other }`.
    #[must_use]
    pub fn merged(&self, other: &Row) -> Row {
        let mut out = self.clone();
        for (k, v) in other.iter() {
            out.set(k, v.clone());
        }
        out
    }

    /// Bentuk JSON, urutan kunci = urutan baris (untuk kolom `before`/`after` urutan
    /// tidak berarti karena jsonb tidak menyimpannya).
    pub fn to_json(&self) -> Value {
        Value::Object(self.0.iter().cloned().collect::<Map<String, Value>>())
    }

    /// Dari objek JSON; urutan kunci = urutan objek (`preserve_order`). Objek hasil
    /// baca jsonb berurutan versi Postgres (panjang kunci, lalu bytewise) — sama dengan
    /// yang diterima Node — jadi pakai [`Row::with`] bila urutan kolom berarti (hasil `create`).
    pub fn from_json(value: &Value) -> Option<Row> {
        let obj = value.as_object()?;
        let mut row = Row::new();
        for (k, v) in obj {
            row.set(k, v.clone());
        }
        Some(row)
    }

    /// `asInt(row[field])`.
    pub fn int(&self, field: &str) -> Option<i64> {
        self.get(field).and_then(as_int)
    }

    /// `asText(row[field])`.
    pub fn text(&self, field: &str) -> Option<&str> {
        self.get(field).and_then(as_text)
    }
}

/// Objek JSON dengan kunci berurutan sesuai baris (= objek JS hasil Prisma).
impl Serialize for Row {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut map = s.serialize_map(Some(self.0.len()))?;
        for (k, v) in &self.0 {
            map.serialize_entry(k, v)?;
        }
        map.end()
    }
}

impl FromIterator<(String, Value)> for Row {
    fn from_iter<T: IntoIterator<Item = (String, Value)>>(iter: T) -> Self {
        let mut row = Row::new();
        for (k, v) in iter {
            row.set(&k, v);
        }
        row
    }
}

/// `typeof v === 'number' && Number.isFinite(v)` — dipersempit ke bilangan bulat, karena
/// semua pemakainya (`id`, `companyId`, `userId`, ...) kolom `Int`.
pub fn as_int(value: &Value) -> Option<i64> {
    let n = value.as_number()?;
    if let Some(i) = n.as_i64() {
        return Some(i);
    }
    let f = n.as_f64()?;
    (f.is_finite() && f.fract() == 0.0 && f.abs() < 9_007_199_254_740_992.0).then_some(f as i64)
}

/// `typeof v === 'string' && v.length > 0`.
pub fn as_text(value: &Value) -> Option<&str> {
    value.as_str().filter(|s| !s.is_empty())
}

/// JS tidak membedakan `1` dan `1.0`; `serde_json::Number` membedakannya. Angka pecahan
/// yang bulat dinormalkan ke bilangan bulat supaya perbandingan & serialisasinya sama.
pub(crate) fn normalize_number(value: Value) -> Value {
    match value {
        Value::Number(ref n) if n.as_i64().is_none() && n.as_u64().is_none() => match n.as_f64() {
            Some(f) if f.is_finite() && f.fract() == 0.0 && f.abs() < 9_007_199_254_740_992.0 => {
                Value::from(f as i64)
            }
            _ => value,
        },
        Value::Array(items) => Value::Array(items.into_iter().map(normalize_number).collect()),
        other => other,
    }
}

/// Nilai dari `to_jsonb(col)` Postgres → bentuk yang ditulis Node untuk kolom itu.
pub(crate) fn normalize_db_value(kind: Kind, is_list: bool, value: Value) -> Value {
    if value.is_null() {
        // Prisma membaca kolom daftar NULL sebagai `[]`.
        return if is_list {
            Value::Array(Vec::new())
        } else {
            Value::Null
        };
    }
    if is_list {
        return match value {
            Value::Array(items) => Value::Array(
                items
                    .into_iter()
                    .map(|v| normalize_db_value(kind, false, v))
                    .collect(),
            ),
            other => other,
        };
    }
    match kind {
        Kind::Timestamp => map_str(value, |s| {
            NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f")
                .ok()
                .map(iso)
        }),
        Kind::Timestamptz => map_str(value, |s| {
            DateTime::parse_from_rfc3339(s)
                .or_else(|_| DateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f%#z"))
                .ok()
                .map(|d| iso(d.naive_utc()))
        }),
        Kind::Date => map_str(value, |s| {
            NaiveDate::parse_from_str(s, "%Y-%m-%d")
                .ok()
                .map(|d| iso(d.and_time(NaiveTime::MIN)))
        }),
        Kind::Time => map_str(value, |s| {
            NaiveTime::parse_from_str(s, "%H:%M:%S%.f")
                .ok()
                .and_then(|t| NaiveDate::from_ymd_opt(1970, 1, 1).map(|d| iso(d.and_time(t))))
        }),
        // Dibaca sebagai `col::text` supaya presisinya utuh.
        Kind::Decimal => map_str(value, |s| Some(decimal_js_string(s))),
        Kind::BigInt => match value {
            Value::Number(n) => Value::String(n.to_string()),
            other => other,
        },
        // `JSON.stringify(Buffer)`.
        Kind::Bytes => {
            let bytes = value
                .as_str()
                .and_then(|s| s.strip_prefix("\\x"))
                .and_then(|hex| {
                    (0..hex.len())
                        .step_by(2)
                        .map(|i| {
                            hex.get(i..i + 2)
                                .and_then(|b| u8::from_str_radix(b, 16).ok())
                        })
                        .collect::<Option<Vec<u8>>>()
                });
            match bytes {
                Some(bytes) => serde_json::json!({ "type": "Buffer", "data": bytes }),
                None => value,
            }
        }
        Kind::Float | Kind::Int => normalize_number(value),
        Kind::Text | Kind::Bool | Kind::Json | Kind::Enum => value,
    }
}

fn map_str(value: Value, f: impl FnOnce(&str) -> Option<String>) -> Value {
    match value {
        Value::String(s) => f(&s).map_or(Value::String(s), Value::String),
        other => other,
    }
}

/// `Date.prototype.toISOString()`.
pub(crate) fn iso(t: NaiveDateTime) -> String {
    t.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

/// `new Decimal(text).toJSON()` (decimal.js): tanpa nol di belakang koma, notasi
/// eksponen bila eksponen ≥ 21 atau ≤ −7.
pub(crate) fn decimal_js_string(text: &str) -> String {
    let t = text.trim();
    let (neg, body) = match t.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let (int_part, frac_part) = body.split_once('.').unwrap_or((body, ""));
    if !int_part.bytes().all(|b| b.is_ascii_digit())
        || !frac_part.bytes().all(|b| b.is_ascii_digit())
    {
        return t.to_owned(); // NaN / Infinity
    }
    let digits: String = format!("{int_part}{frac_part}");
    let lead = digits.bytes().take_while(|b| *b == b'0').count();
    let coeff = digits.get(lead..).unwrap_or("").trim_end_matches('0');
    if coeff.is_empty() {
        return "0".to_owned();
    }
    // Eksponen desimal digit pertama yang bukan nol.
    let exp = int_part.len() as i64 - 1 - lead as i64;
    let sign = if neg { "-" } else { "" };
    if exp >= 21 || exp <= -7 {
        let (first, rest) = coeff.split_at(1);
        let mantissa = if rest.is_empty() {
            first.to_owned()
        } else {
            format!("{first}.{rest}")
        };
        let exp_sign = if exp < 0 { "-" } else { "+" };
        return format!("{sign}{mantissa}e{exp_sign}{}", exp.abs());
    }
    let s = if exp < 0 {
        format!("0.{}{}", "0".repeat((-exp - 1) as usize), coeff)
    } else {
        let int_len = exp as usize + 1;
        if coeff.len() <= int_len {
            format!("{coeff}{}", "0".repeat(int_len - coeff.len()))
        } else {
            let (i, f) = coeff.split_at(int_len);
            format!("{i}.{f}")
        }
    };
    format!("{sign}{s}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn null_list_column_is_empty_array() {
        assert_eq!(normalize_db_value(Kind::Text, true, Value::Null), json!([]));
        assert_eq!(
            normalize_db_value(Kind::Text, false, Value::Null),
            Value::Null
        );
    }

    #[test]
    fn set_keeps_insertion_order_and_replaces_in_place() {
        let mut row = Row::new().with("b", 1).with("a", 2);
        row.set("b", 3);
        assert_eq!(row.keys().collect::<Vec<_>>(), vec!["b", "a"]);
        assert_eq!(row.get("b"), Some(&json!(3)));
    }

    #[test]
    fn serializes_in_row_order() {
        let row = Row::new()
            .with("id", 1)
            .with("name", "Budi")
            .with("createdAt", "x");
        let text = serde_json::to_string(&row).unwrap_or_default();
        assert_eq!(text, r#"{"id":1,"name":"Budi","createdAt":"x"}"#);
        let text = serde_json::to_string(&row.to_json()).unwrap_or_default();
        assert_eq!(text, r#"{"id":1,"name":"Budi","createdAt":"x"}"#);
    }

    #[test]
    fn integral_floats_become_ints() {
        let row = Row::new().with("x", 2.0).with("y", 2.5);
        assert_eq!(row.get("x"), Some(&json!(2)));
        assert_eq!(row.get("y"), Some(&json!(2.5)));
    }

    #[test]
    fn decimal_matches_decimal_js() {
        assert_eq!(decimal_js_string("1.50"), "1.5");
        assert_eq!(decimal_js_string("100"), "100");
        assert_eq!(decimal_js_string("100.00"), "100");
        assert_eq!(decimal_js_string("0.00"), "0");
        assert_eq!(decimal_js_string("-0.25"), "-0.25");
        assert_eq!(decimal_js_string("0.0000001"), "1e-7");
        assert_eq!(decimal_js_string("0.000001"), "0.000001");
        assert_eq!(decimal_js_string("1000000000000000000000"), "1e+21");
    }

    #[test]
    fn db_values_are_normalized_like_prisma() {
        assert_eq!(
            normalize_db_value(Kind::Timestamp, false, json!("2026-11-16T08:00:00.1")),
            json!("2026-11-16T08:00:00.100Z")
        );
        assert_eq!(
            normalize_db_value(Kind::Timestamp, false, json!("2026-11-16T08:00:00")),
            json!("2026-11-16T08:00:00.000Z")
        );
        assert_eq!(
            normalize_db_value(
                Kind::Timestamptz,
                false,
                json!("2026-11-16T15:00:00.25+07:00")
            ),
            json!("2026-11-16T08:00:00.250Z")
        );
        assert_eq!(
            normalize_db_value(Kind::Date, false, json!("2026-08-20")),
            json!("2026-08-20T00:00:00.000Z")
        );
        assert_eq!(
            normalize_db_value(Kind::Time, false, json!("07:30:00")),
            json!("1970-01-01T07:30:00.000Z")
        );
        assert_eq!(
            normalize_db_value(Kind::BigInt, false, json!(12)),
            json!("12")
        );
        assert_eq!(
            normalize_db_value(Kind::Decimal, false, json!("12.500")),
            json!("12.5")
        );
        assert_eq!(
            normalize_db_value(Kind::Bytes, false, json!("\\x0102")),
            json!({"type": "Buffer", "data": [1, 2]})
        );
        assert_eq!(
            normalize_db_value(Kind::Int, true, json!([1, 2])),
            json!([1, 2])
        );
    }
}
