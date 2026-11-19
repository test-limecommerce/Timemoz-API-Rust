//! Pemformat angka/tanggal yang meniru perilaku JavaScript (`toFixed`,
//! `parseFloat`, `toLocaleString('en-US')`, serialisasi `Number`) supaya respons
//! Rust identik dengan Node sampai ke teks ringkasan budget. Dipakai mz-projects,
//! mz-timesheets, mz-timeconfig (dulu salinan per crate).

use serde_json::{Number, Value};

/// Pembanding nama mirip `localeCompare(b, 'id')` (ICU): abaikan huruf besar/kecil dulu,
/// lalu huruf kecil sebelum huruf besar. Rumah bersama (dulu salinan di mz-overtime,
/// mz-divisions, mz-roles).
pub fn collate_id(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let primary = a.to_lowercase().cmp(&b.to_lowercase());
    if primary != Ordering::Equal {
        return primary;
    }
    for (x, y) in a.chars().zip(b.chars()) {
        if x != y {
            return if x.is_lowercase() {
                Ordering::Less
            } else {
                Ordering::Greater
            };
        }
    }
    Ordering::Equal
}

/// `Number.prototype.toFixed(digits)`: setengah dibulatkan KE ATAS dari nilai
/// biner persisnya (Rust `{:.N}` membulatkan setengah ke genap).
pub fn to_fixed(x: f64, digits: usize) -> String {
    if !x.is_finite() {
        return x.to_string();
    }
    let neg = x < 0.0;
    let mut a = x.abs();
    let wide = format!("{a:.60}");
    if let Some((_, frac)) = wide.split_once('.') {
        let tail = frac.get(digits..).unwrap_or("");
        let mut it = tail.chars();
        let first = it.next();
        if first == Some('5') && it.all(|c| c == '0') {
            // Tepat di tengah: naikkan satu ulp supaya Rust membulatkan ke atas.
            a = f64::from_bits(a.to_bits() + 1);
        }
    }
    let s = format!("{a:.digits$}");
    if neg { format!("-{s}") } else { s }
}

/// `parseFloat(x.toFixed(digits))`.
pub fn fixed(x: f64, digits: usize) -> f64 {
    to_fixed(x, digits).parse::<f64>().unwrap_or(0.0)
}

/// `String(x)` untuk angka JS.
pub fn num_string(x: f64) -> String {
    if x == 0.0 {
        return "0".into();
    }
    if x.is_finite() && x.fract() == 0.0 && x.abs() < 1e21 {
        return format!("{x:.0}");
    }
    format!("{x}")
}

/// Angka JSON seperti `JSON.stringify(number)`: `5`, bukan `5.0`.
pub fn num(x: f64) -> Value {
    if x.is_finite() && x.fract() == 0.0 && x.abs() < 9.0e15 {
        return Value::from(x as i64);
    }
    Number::from_f64(x).map_or(Value::Null, Value::Number)
}

/// `Number.prototype.toLocaleString()` (en-US, maksimum 3 digit desimal).
pub fn locale(x: f64) -> String {
    let s = to_fixed(x.abs(), 3);
    let (int_part, frac) = s.split_once('.').unwrap_or((s.as_str(), ""));
    let frac = frac.trim_end_matches('0');
    let mut grouped = String::new();
    let n = int_part.len();
    for (i, ch) in int_part.chars().enumerate() {
        if i > 0 && (n - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    let body = if frac.is_empty() {
        grouped
    } else {
        format!("{grouped}.{frac}")
    };
    if x < 0.0 && body != "0" {
        format!("-{body}")
    } else {
        body
    }
}

/// `formatHours` di project-budget.service.ts.
pub fn format_hours(hours: f64) -> String {
    let a = hours.abs();
    let h = a.floor();
    let m = ((a - h) * 60.0 + 0.5).floor();
    if m == 0.0 {
        format!("{}h", h as i64)
    } else {
        format!("{}h {}m", h as i64, m as i64)
    }
}

/// `String.prototype.trim` (spasi Unicode + BOM `U+FEFF`).
pub fn trim(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
}

/// `Number(string)` JavaScript: trim, kosong → 0, `Infinity` bertanda, prefiks `0x`/`0o`/`0b`
/// (tanpa tanda), desimal/eksponen; selain itu `NaN`.
pub fn number(s: &str) -> f64 {
    let t = trim(s);
    if t.is_empty() {
        return 0.0;
    }
    match t {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    let prefix = t.get(..2).map(str::to_ascii_lowercase);
    let radix = match prefix.as_deref() {
        Some("0x") => 16,
        Some("0o") => 8,
        Some("0b") => 2,
        _ => 10,
    };
    if radix != 10 {
        let digits = t.get(2..).unwrap_or_default();
        if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
            return f64::NAN;
        }
        return digits.chars().fold(0.0, |acc, c| {
            acc * f64::from(radix) + f64::from(c.to_digit(radix).unwrap_or(0))
        });
    }
    // `str::parse::<f64>` juga menerima `inf`/`nan`/`infinity`; JS tidak.
    if !t
        .bytes()
        .all(|b| b.is_ascii_digit() || matches!(b, b'+' | b'-' | b'.' | b'e' | b'E'))
    {
        return f64::NAN;
    }
    t.parse::<f64>().unwrap_or(f64::NAN)
}

/// `parseInt(str, 10)`; `None` = NaN.
pub fn parse_int(s: &str) -> Option<i64> {
    let t = s.trim_start();
    let mut end = 0;
    for (i, c) in t.char_indices() {
        if c.is_ascii_digit() || (i == 0 && (c == '-' || c == '+')) {
            end = i + c.len_utf8();
        } else {
            break;
        }
    }
    t.get(..end).and_then(|p| p.parse::<i64>().ok())
}

/// Prisma memotong pecahan ke arah nol untuk kolom Int; di luar Int32 gagal.
pub fn as_i32(x: f64) -> Option<i32> {
    let t = x.trunc();
    if t.is_finite() && t >= f64::from(i32::MIN) && t <= f64::from(i32::MAX) {
        Some(t as i32)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_fixed_rounds_half_up_on_exact_ties() {
        assert_eq!(to_fixed(0.125, 2), "0.13");
        assert_eq!(to_fixed(2.5, 0), "3");
        assert_eq!(to_fixed(1.005, 2), "1.00");
        assert_eq!(to_fixed(10.0, 1), "10.0");
        assert_eq!(to_fixed(1.0 / 3.0, 2), "0.33");
    }

    #[test]
    fn number_rendering() {
        assert_eq!(num_string(40.0), "40");
        assert_eq!(num_string(12.5), "12.5");
        assert_eq!(num(5.0), Value::from(5));
        assert_eq!(num(0.5).to_string(), "0.5");
        assert_eq!(fixed(2.345, 1), 2.3);
    }

    #[test]
    fn locale_groups_thousands() {
        assert_eq!(locale(1234567.891_4), "1,234,567.891");
        assert_eq!(locale(1000.0), "1,000");
        assert_eq!(locale(12.5), "12.5");
        assert_eq!(locale(0.0), "0");
    }

    #[test]
    fn hours_format() {
        assert_eq!(format_hours(2.0), "2h");
        assert_eq!(format_hours(2.5), "2h 30m");
        assert_eq!(format_hours(0.25), "0h 15m");
    }

    #[test]
    fn parse_int_semantics() {
        assert_eq!(parse_int("12abc"), Some(12));
        assert_eq!(parse_int("abc"), None);
        assert_eq!(parse_int(""), None);
        assert_eq!(parse_int("-3"), Some(-3));
        assert_eq!(as_i32(3.0), Some(3));
        assert_eq!(as_i32(3.5), Some(3));
        assert_eq!(as_i32(-2.6), Some(-2));
        assert_eq!(as_i32(3_000_000_000.0), None);
        assert_eq!(as_i32(f64::NAN), None);
    }

    #[test]
    fn number_matches_node() {
        // `String(Number(x))` dari Node 2026-11-16.
        let cases: &[(&str, f64)] = &[
            ("", 0.0),
            ("  42 ", 42.0),
            ("3.0", 3.0),
            ("1e3", 1000.0),
            ("1E+3", 1000.0),
            ("0x10", 16.0),
            ("0X1f", 31.0),
            ("0o17", 15.0),
            ("0b101", 5.0),
            ("0B101", 5.0),
            ("0xFFFFFFFFFFFFFFFFFF", 4.722_366_482_869_645e21),
            ("Infinity", f64::INFINITY),
            ("+Infinity", f64::INFINITY),
            ("-Infinity", f64::NEG_INFINITY),
            (".5", 0.5),
            ("5.", 5.0),
            ("+5", 5.0),
            ("-5", -5.0),
            ("1.e1", 10.0),
            ("\u{a0}12\u{a0}", 12.0),
            ("\u{feff}7", 7.0),
            ("\n8\t", 8.0),
            ("00012", 12.0),
            ("-0", 0.0),
            ("1e400", f64::INFINITY),
        ];
        for (s, want) in cases {
            assert_eq!(number(s), *want, "{s:?}");
        }
        for s in [
            "infinity", "inf", "NaN", "nan", "1_0", "0x", "0xg", "1e", "-0x10", "+0x10", "--5",
            "1 2", "0x1.8", "abc", "12abc",
        ] {
            assert!(number(s).is_nan(), "{s:?}");
        }
    }
}
