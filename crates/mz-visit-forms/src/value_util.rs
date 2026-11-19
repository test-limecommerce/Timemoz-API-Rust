//! Port `form-value.util.ts` + pembantu semantik JS untuk `serde_json::Value`.
//!
//! Konvensi: `Option<&Value>` — `None` = `undefined` (properti absen), `Some(Null)` = `null`.

use chrono::{Duration, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;
use serde_json::{Map, Value};

use crate::consts::{is_date_shape, is_time_shape, parse_relative_offset};

/// `isPlainObject`.
pub fn is_plain_object(v: Option<&Value>) -> bool {
    matches!(v, Some(Value::Object(_)))
}

/// `isEmptyValue`: absen, `null`, `""`/spasi, `[]`, `{}`. `0`/`false` TIDAK kosong.
pub fn is_empty_value(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) => js_trim(s).is_empty(),
        Some(Value::Array(a)) => a.is_empty(),
        Some(Value::Object(o)) => o.is_empty(),
        Some(_) => false,
    }
}

/// `String.prototype.trim` (whitespace + line terminator ECMAScript).
pub fn js_trim(s: &str) -> &str {
    s.trim_matches(is_js_whitespace)
}

pub fn is_js_whitespace(c: char) -> bool {
    c.is_whitespace() || c == '\u{FEFF}'
}

/// `typeof v === 'number'` → nilainya.
pub fn as_number(v: Option<&Value>) -> Option<f64> {
    v.and_then(Value::as_f64)
}

/// `Number.isInteger(x)`.
pub fn is_integer(x: f64) -> bool {
    x.is_finite() && x.fract() == 0.0
}

/// `decimalPlaces` — tahan notasi eksponen.
pub fn decimal_places(value: f64) -> i64 {
    if is_integer(value) {
        return 0;
    }
    // `{:e}` Rust = representasi terpendek seperti `toExponential()` JS.
    let repr = format!("{value:e}");
    let (mantissa, exponent) = repr.split_once('e').unwrap_or((repr.as_str(), "0"));
    let fraction_digits = mantissa.split_once('.').map_or(0, |(_, f)| f.len() as i64);
    let exp: i64 = exponent.parse().unwrap_or(0);
    (fraction_digits - exp).max(0)
}

/// `textLength` — jumlah titik kode.
pub fn text_length(s: &str) -> usize {
    s.chars().count()
}

/// `isCalendarDate` (`moment(value, 'YYYY-MM-DD', true).isValid()`).
pub fn is_calendar_date(s: &str) -> bool {
    parse_calendar_date(s).is_some()
}

pub fn parse_calendar_date(s: &str) -> Option<NaiveDate> {
    if !is_date_shape(s) {
        return None;
    }
    let y: i32 = s.get(0..4)?.parse().ok()?;
    let m: u32 = s.get(5..7)?.parse().ok()?;
    let d: u32 = s.get(8..10)?.parse().ok()?;
    NaiveDate::from_ymd_opt(y, m, d)
}

/// `isClockTime`.
pub fn is_clock_time(s: &str) -> bool {
    is_time_shape(s)
}

/// `moment.tz(timeZone)` — zona tak dikenal = UTC (perilaku moment).
pub fn zone(time_zone: &str) -> Tz {
    mz_core::tz::zone_or_utc(Some(time_zone))
}

/// Hari ini di zona tertentu.
pub fn today_in(time_zone: &str, now: chrono::DateTime<Utc>) -> NaiveDate {
    now.with_timezone(&zone(time_zone)).date_naive()
}

/// `resolveDateBound(bound, timeZone)` → `"YYYY-MM-DD"`.
pub fn resolve_date_bound(bound: &str, time_zone: &str) -> Option<String> {
    resolve_date_bound_at(bound, time_zone, Utc::now())
}

/// [`resolve_date_bound`] dengan jam tetap (uji).
pub fn resolve_date_bound_at(
    bound: &str,
    time_zone: &str,
    now: chrono::DateTime<Utc>,
) -> Option<String> {
    let fmt = |d: NaiveDate| d.format("%Y-%m-%d").to_string();
    if bound == "today" {
        return Some(fmt(today_in(time_zone, now)));
    }
    if let Some((neg, days)) = parse_relative_offset(bound) {
        let base = today_in(time_zone, now);
        let shifted = if neg {
            base.checked_sub_signed(Duration::days(days))
        } else {
            base.checked_add_signed(Duration::days(days))
        }?;
        return Some(fmt(shifted));
    }
    if is_calendar_date(bound) {
        return Some(bound.to_owned());
    }
    None
}

/// `resolveInstantBound(bound, timeZone, end)` → epoch ms.
pub fn resolve_instant_bound(bound: &str, time_zone: &str, end: bool) -> Option<i64> {
    resolve_instant_bound_at(bound, time_zone, end, Utc::now())
}

pub fn resolve_instant_bound_at(
    bound: &str,
    time_zone: &str,
    end: bool,
    now: chrono::DateTime<Utc>,
) -> Option<i64> {
    let tz = zone(time_zone);
    if let Some(day) = resolve_date_bound_at(bound, time_zone, now) {
        let d = parse_calendar_date(&day)?;
        let start = local_to_utc_ms(&tz, d.and_time(NaiveTime::MIN))?;
        if end {
            let next = d.succ_opt()?;
            return Some(local_to_utc_ms(&tz, next.and_time(NaiveTime::MIN))? - 1);
        }
        return Some(start);
    }
    parse_moment_iso_strict(bound, &tz)
}

/// Waktu lokal → epoch ms (celah DST: maju seperti moment; ambigu: offset pertama).
pub fn local_to_utc_ms(tz: &Tz, local: NaiveDateTime) -> Option<i64> {
    use chrono::LocalResult;
    match tz.from_local_datetime(&local) {
        LocalResult::Single(t) => Some(t.timestamp_millis()),
        LocalResult::Ambiguous(a, _) => Some(a.timestamp_millis()),
        LocalResult::None => tz
            .from_local_datetime(&(local + Duration::hours(1)))
            .earliest()
            .map(|t| t.timestamp_millis()),
    }
}

fn take_digits(s: &str, n: usize) -> Option<(u32, &str)> {
    let head = s.get(..n)?;
    if !head.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((head.parse().ok()?, s.get(n..)?))
}

/// `moment.tz(bound, moment.ISO_8601, true, timeZone)` — subset yang dipakai klien:
/// `YYYY-MM-DD` / `YYYYMMDD` / `YYYY-MM`, lalu opsional `T`/spasi + `HH`, `HH:mm`,
/// `HH:mm:ss`, `HH:mm:ss.SSS` (atau bentuk basic `HHmm[ss]`), lalu opsional `Z`/`±HH:mm`/
/// `±HHmm`/`±HH`. Tanpa offset dibaca di `tz`. Tanggal minggu/ordinal ISO tidak didukung.
pub fn parse_moment_iso_strict(s: &str, tz: &Tz) -> Option<i64> {
    let (date, rest) = if let Some((y, r)) = take_digits(s, 4) {
        if let Some(r) = r.strip_prefix('-') {
            let (m, r) = take_digits(r, 2)?;
            if let Some(r2) = r.strip_prefix('-') {
                let (d, r2) = take_digits(r2, 2)?;
                (NaiveDate::from_ymd_opt(y as i32, m, d)?, r2)
            } else {
                (NaiveDate::from_ymd_opt(y as i32, m, 1)?, r)
            }
        } else {
            let (m, r) = take_digits(r, 2)?;
            let (d, r) = take_digits(r, 2)?;
            (NaiveDate::from_ymd_opt(y as i32, m, d)?, r)
        }
    } else {
        return None;
    };
    let Some(r) = rest.strip_prefix(['T', ' ']) else {
        return finish(date.and_time(NaiveTime::MIN), rest, tz);
    };
    let (h, mut r) = take_digits(r, 2)?;
    let (mut mi, mut sec, mut ms) = (0, 0, 0);
    let extended = r.starts_with(':');
    let two_digits = |r: &str| {
        r.get(..2)
            .is_some_and(|p| p.bytes().all(|b| b.is_ascii_digit()))
    };
    // Komponen berikut: `:mm` (extended) atau `mm` (basic).
    fn next(r: &str, extended: bool) -> Option<(u32, &str)> {
        let r = if extended { r.strip_prefix(':')? } else { r };
        take_digits(r, 2)
    }
    if extended || two_digits(r) {
        (mi, r) = next(r, extended)?;
        if (extended && r.starts_with(':')) || (!extended && two_digits(r)) {
            (sec, r) = next(r, extended)?;
            if let Some(frac) = r.strip_prefix(['.', ',']) {
                let len = frac.bytes().take_while(u8::is_ascii_digit).count();
                if len == 0 {
                    return None;
                }
                let digits = frac.get(..len.min(3))?;
                ms = format!("{digits:0<3}").parse().ok()?;
                r = frac.get(len..)?;
            }
        }
    }
    let time = NaiveTime::from_hms_milli_opt(h, mi, sec, ms)?;
    finish(date.and_time(time), r, tz)
}

fn finish(local: NaiveDateTime, rest: &str, tz: &Tz) -> Option<i64> {
    if rest.is_empty() {
        return local_to_utc_ms(tz, local);
    }
    if rest == "Z" || rest == "z" {
        return Some(local.and_utc().timestamp_millis());
    }
    let (sign, r) = if let Some(r) = rest.strip_prefix('+') {
        (1, r)
    } else {
        (-1, rest.strip_prefix('-')?)
    };
    let (h, r) = take_digits(r, 2)?;
    let r = r.strip_prefix(':').unwrap_or(r);
    let (m, r) = if r.is_empty() {
        (0, r)
    } else {
        take_digits(r, 2)?
    };
    if !r.is_empty() || h > 23 || m > 59 {
        return None;
    }
    let offset_ms = sign * (i64::from(h) * 3_600_000 + i64::from(m) * 60_000);
    Some(local.and_utc().timestamp_millis() - offset_ms)
}

/// `===` antara dua nilai JSON (`undefined` = `None`). Object/array: identitas, jadi
/// dua nilai terpisah tidak pernah sama — kecuali keduanya `None`.
pub fn js_strict_eq(a: Option<&Value>, b: Option<&Value>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(Value::Null), Some(Value::Null)) => true,
        (Some(Value::Bool(x)), Some(Value::Bool(y))) => x == y,
        (Some(Value::Number(x)), Some(Value::Number(y))) => x.as_f64() == y.as_f64(),
        (Some(Value::String(x)), Some(Value::String(y))) => x == y,
        _ => false,
    }
}

/// `Array.prototype.includes` / `Set.has` (SameValueZero; JSON tidak punya NaN).
pub fn js_includes(list: &[Value], item: Option<&Value>) -> bool {
    list.iter().any(|x| js_strict_eq(Some(x), item))
}

/// Perbandingan string JS (`<`) = urutan unit kode UTF-16.
pub fn js_str_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

/// `structuredClone` + normalisasi angka seperti `JSON.parse` JS: angka bulat ber-
/// pecahan nol (`1.0`) menjadi bilangan bulat, supaya yang tersimpan sama dengan Node.
pub fn js_normalize(v: &Value) -> Value {
    match v {
        Value::Number(n) if !n.is_i64() && !n.is_u64() => {
            n.as_f64().map_or(Value::Null, mz_core::jsfmt::num)
        }
        Value::Array(a) => Value::Array(a.iter().map(js_normalize).collect()),
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, v)| (k.clone(), js_normalize(v)))
                .collect::<Map<String, Value>>(),
        ),
        other => other.clone(),
    }
}

/// `JSON.stringify(v)` seperti JS (angka gaya JS, tanpa spasi).
pub fn js_stringify(v: &Value) -> String {
    serde_json::to_string(&js_normalize(v)).unwrap_or_default()
}

/// `typeof v` untuk pesan galat.
pub fn js_typeof(v: Option<&Value>) -> &'static str {
    match v {
        None => "undefined",
        Some(Value::Null) | Some(Value::Array(_)) | Some(Value::Object(_)) => "object",
        Some(Value::Bool(_)) => "boolean",
        Some(Value::Number(_)) => "number",
        Some(Value::String(_)) => "string",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn at(s: &str) -> chrono::DateTime<Utc> {
        chrono::DateTime::parse_from_rfc3339(s)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn empties() {
        assert!(is_empty_value(None));
        assert!(is_empty_value(Some(&json!(null))));
        assert!(is_empty_value(Some(&json!("  "))));
        assert!(is_empty_value(Some(&json!([]))));
        assert!(is_empty_value(Some(&json!({}))));
        assert!(!is_empty_value(Some(&json!(0))));
        assert!(!is_empty_value(Some(&json!(false))));
    }

    #[test]
    fn decimals() {
        assert_eq!(decimal_places(1.0), 0);
        assert_eq!(decimal_places(1.5), 1);
        assert_eq!(decimal_places(0.125), 3);
        assert_eq!(decimal_places(1e-7), 7);
        assert_eq!(decimal_places(1.25e-7), 9);
    }

    #[test]
    fn dates() {
        assert!(is_calendar_date("2024-02-29"));
        assert!(!is_calendar_date("2023-02-29"));
        assert!(!is_calendar_date("2023-13-01"));
        // 17:30 UTC = 00:30 WIB keesokan harinya.
        let now = at("2026-11-16T17:30:00Z");
        assert_eq!(
            resolve_date_bound_at("today", "Asia/Jakarta", now).as_deref(),
            Some("2026-11-17")
        );
        assert_eq!(
            resolve_date_bound_at("today", "UTC", now).as_deref(),
            Some("2026-11-16")
        );
        assert_eq!(
            resolve_date_bound_at("today-7d", "Asia/Jakarta", now).as_deref(),
            Some("2026-11-10")
        );
        assert_eq!(
            resolve_date_bound_at("2026-01-02", "Asia/Jakarta", now).as_deref(),
            Some("2026-01-02")
        );
        assert_eq!(resolve_date_bound_at("tomorrow", "UTC", now), None);
    }

    #[test]
    fn instants() {
        let now = at("2026-11-16T00:00:00Z");
        let start = resolve_instant_bound_at("2026-11-16", "Asia/Jakarta", false, now).unwrap();
        assert_eq!(start, at("2026-11-15T17:00:00Z").timestamp_millis());
        let end = resolve_instant_bound_at("2026-11-16", "Asia/Jakarta", true, now).unwrap();
        assert_eq!(end, at("2026-11-16T16:59:59.999Z").timestamp_millis());
        let iso = resolve_instant_bound_at("2026-11-16T08:00:00Z", "Asia/Jakarta", false, now);
        assert_eq!(iso, Some(at("2026-11-16T08:00:00Z").timestamp_millis()));
        let local = resolve_instant_bound_at("2026-11-16T08:00", "Asia/Jakarta", false, now);
        assert_eq!(local, Some(at("2026-11-16T01:00:00Z").timestamp_millis()));
        let off = resolve_instant_bound_at("2026-11-16T08:00:00.5+07:00", "UTC", false, now);
        assert_eq!(off, Some(at("2026-11-16T01:00:00.500Z").timestamp_millis()));
        assert_eq!(resolve_instant_bound_at("nope", "UTC", false, now), None);
        assert_eq!(
            resolve_instant_bound_at("2026-11-16T25:00", "UTC", false, now),
            None
        );
    }

    #[test]
    fn js_eq() {
        assert!(js_strict_eq(Some(&json!(1)), Some(&json!(1.0))));
        assert!(!js_strict_eq(Some(&json!({})), Some(&json!({}))));
        assert!(!js_strict_eq(None, Some(&json!(null))));
        assert!(js_strict_eq(None, None));
        assert_eq!(
            js_stringify(&json!({"a": 1.0, "b": [2.5]})),
            r#"{"a":1,"b":[2.5]}"#
        );
    }
}
