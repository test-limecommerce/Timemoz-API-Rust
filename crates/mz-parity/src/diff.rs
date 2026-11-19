//! Pembanding JSON Node vs Rust.
//!
//! Aturan:
//! - Urutan kunci objek diabaikan; urutan array dihormati kecuali path ada di `unordered`.
//! - Angka dibandingkan PERSIS sesuai bentuk JSON-nya: `1` ≠ `1.0` (JS tidak pernah
//!   mencetak `1.0`, jadi selisih ini bug paritas yang nyata).
//! - Nilai "volatil" (dibuat saat request): dua UUID berbeda, dua timestamp ISO yang
//!   sama-sama dekat waktu run, atau dua JWT dengan payload sama selain `iat/exp/sid`
//!   → dianggap sama tapi dicatat sebagai PERINGATAN, bukan gagal.

use regex::Regex;
use serde_json::Value;
use std::sync::OnceLock;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Severity {
    Fail,
    Warn,
}

#[derive(Debug, Clone)]
pub struct Diff {
    pub path: String,
    pub severity: Severity,
    pub node: String,
    pub rust: String,
}

pub struct Opts<'a> {
    pub ignore: &'a [String],
    pub unordered: &'a [String],
    /// Rentang waktu run (ms epoch) untuk mengenali timestamp volatil.
    pub window_ms: (i64, i64),
}

fn uuid_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$")
            .unwrap_or_else(|e| unreachable!("{e}"))
    })
}

fn jwt_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"^[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+$")
            .unwrap_or_else(|e| unreachable!("{e}"))
    })
}

/// Pointer `a/*/b` cocok dengan `a/3/b`.
pub fn pointer_matches(pattern: &str, path: &str) -> bool {
    let p: Vec<&str> = pattern.trim_start_matches('/').split('/').collect();
    let s: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    p.len() == s.len() && p.iter().zip(&s).all(|(a, b)| *a == "*" || a == b)
}

fn short(v: &Value) -> String {
    let s = v.to_string();
    if s.chars().count() > 160 {
        let cut: String = s.chars().take(160).collect();
        format!("{cut}…")
    } else {
        s
    }
}

fn near_window(s: &str, window: (i64, i64)) -> bool {
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|d| d.timestamp_millis())
        .is_ok_and(|ms| ms >= window.0 - 120_000 && ms <= window.1 + 120_000)
}

fn jwt_payload(token: &str) -> Option<serde_json::Map<String, Value>> {
    use base64_url::decode;
    let part = token.split('.').nth(1)?;
    let bytes = decode(part)?;
    let Value::Object(mut m) = serde_json::from_slice(&bytes).ok()? else {
        return None;
    };
    for k in ["iat", "exp", "sid", "nbf"] {
        m.remove(k);
    }
    Some(m)
}

/// base64url tanpa padding, cukup untuk payload JWT (tanpa dependensi tambahan).
mod base64_url {
    pub fn decode(s: &str) -> Option<Vec<u8>> {
        let mut out = Vec::with_capacity(s.len() * 3 / 4);
        let mut buf = 0u32;
        let mut bits = 0u32;
        for c in s.bytes() {
            let v = match c {
                b'A'..=b'Z' => c - b'A',
                b'a'..=b'z' => c - b'a' + 26,
                b'0'..=b'9' => c - b'0' + 52,
                b'-' | b'+' => 62,
                b'_' | b'/' => 63,
                b'=' => continue,
                _ => return None,
            };
            buf = (buf << 6) | u32::from(v);
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                out.push(u8::try_from((buf >> bits) & 0xff).ok()?);
            }
        }
        Some(out)
    }
}

fn volatile_equal(a: &str, b: &str, window: (i64, i64)) -> bool {
    (uuid_re().is_match(a) && uuid_re().is_match(b))
        || (near_window(a, window) && near_window(b, window))
        || (jwt_re().is_match(a)
            && jwt_re().is_match(b)
            && jwt_payload(a).is_some_and(|pa| jwt_payload(b).is_some_and(|pb| pa == pb)))
}

pub fn compare(node: &Value, rust: &Value, opts: &Opts<'_>) -> Vec<Diff> {
    let mut out = Vec::new();
    walk("", node, rust, opts, &mut out);
    out
}

fn push(out: &mut Vec<Diff>, path: &str, sev: Severity, node: &Value, rust: &Value) {
    out.push(Diff {
        path: if path.is_empty() {
            "/".into()
        } else {
            path.into()
        },
        severity: sev,
        node: short(node),
        rust: short(rust),
    });
}

fn walk(path: &str, a: &Value, b: &Value, opts: &Opts<'_>, out: &mut Vec<Diff>) {
    if opts.ignore.iter().any(|p| pointer_matches(p, path)) {
        return;
    }
    match (a, b) {
        (Value::Object(ma), Value::Object(mb)) => {
            let mut keys: Vec<&String> = ma.keys().chain(mb.keys()).collect();
            keys.sort();
            keys.dedup();
            for k in keys {
                let p = format!("{path}/{}", k.replace('~', "~0").replace('/', "~1"));
                match (ma.get(k), mb.get(k)) {
                    (Some(x), Some(y)) => walk(&p, x, y, opts, out),
                    (Some(x), None) => {
                        if !opts.ignore.iter().any(|i| pointer_matches(i, &p)) {
                            push(
                                out,
                                &p,
                                Severity::Fail,
                                x,
                                &Value::String("<tidak ada>".into()),
                            );
                        }
                    }
                    (None, Some(y)) => {
                        if !opts.ignore.iter().any(|i| pointer_matches(i, &p)) {
                            push(
                                out,
                                &p,
                                Severity::Fail,
                                &Value::String("<tidak ada>".into()),
                                y,
                            );
                        }
                    }
                    (None, None) => {}
                }
            }
        }
        (Value::Array(xa), Value::Array(xb)) => {
            if xa.len() != xb.len() {
                push(
                    out,
                    &format!("{path}#len"),
                    Severity::Fail,
                    &Value::from(xa.len()),
                    &Value::from(xb.len()),
                );
            }
            if opts.unordered.iter().any(|p| pointer_matches(p, path)) {
                let mut sa: Vec<String> = xa.iter().map(canonical).collect();
                let mut sb: Vec<String> = xb.iter().map(canonical).collect();
                sa.sort();
                sb.sort();
                if sa != sb {
                    push(out, path, Severity::Fail, a, b);
                }
                return;
            }
            for (i, (x, y)) in xa.iter().zip(xb).enumerate() {
                walk(&format!("{path}/{i}"), x, y, opts, out);
            }
        }
        (Value::String(x), Value::String(y)) if x != y => {
            let sev = if volatile_equal(x, y, opts.window_ms) {
                Severity::Warn
            } else {
                Severity::Fail
            };
            push(out, path, sev, a, b);
        }
        (Value::Number(x), Value::Number(y)) => {
            // Bandingkan bentuk teksnya: `1` vs `1.0` adalah selisih nyata.
            if x.to_string() != y.to_string() {
                push(out, path, Severity::Fail, a, b);
            }
        }
        _ if a == b => {}
        _ => push(out, path, Severity::Fail, a, b),
    }
}

/// Serialisasi kanonis (kunci terurut) untuk perbandingan array tak berurut.
fn canonical(v: &Value) -> String {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            let inner: Vec<String> = keys
                .into_iter()
                .map(|k| {
                    format!(
                        "{}:{}",
                        Value::String(k.clone()),
                        m.get(k).map(canonical).unwrap_or_default()
                    )
                })
                .collect();
            format!("{{{}}}", inner.join(","))
        }
        Value::Array(a) => format!(
            "[{}]",
            a.iter().map(canonical).collect::<Vec<_>>().join(",")
        ),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn opts<'a>(ignore: &'a [String], unordered: &'a [String]) -> Opts<'a> {
        Opts {
            ignore,
            unordered,
            window_ms: (1_800_000_000_000, 1_800_000_060_000),
        }
    }

    #[test]
    fn key_order_is_ignored_but_values_are_not() {
        let d = compare(
            &json!({"a":1,"b":2}),
            &json!({"b":2,"a":1}),
            &opts(&[], &[]),
        );
        assert!(d.is_empty());
        let d = compare(&json!({"a":1}), &json!({"a":2}), &opts(&[], &[]));
        assert_eq!(d.len(), 1);
    }

    #[test]
    fn integer_vs_float_form_is_a_failure() {
        let d = compare(
            &json!({"a":1}),
            &serde_json::from_str::<Value>(r#"{"a":1.0}"#).unwrap_or_default(),
            &opts(&[], &[]),
        );
        assert_eq!(d.first().map(|x| x.severity.clone()), Some(Severity::Fail));
    }

    #[test]
    fn missing_key_and_null_differ() {
        let d = compare(&json!({"a":null}), &json!({}), &opts(&[], &[]));
        assert_eq!(d.len(), 1);
    }

    #[test]
    fn fresh_uuids_are_warnings() {
        let d = compare(
            &json!({"uuid":"3f2504e0-4f89-41d3-9a0c-0305e82c3301"}),
            &json!({"uuid":"9b1deb4d-3b7d-4bad-9bdd-2b0d7b3dcb6d"}),
            &opts(&[], &[]),
        );
        assert_eq!(d.first().map(|x| x.severity.clone()), Some(Severity::Warn));
    }

    #[test]
    fn timestamps_near_run_are_warnings_old_ones_fail() {
        let near = compare(
            &json!("2027-01-15T08:00:00.000Z"),
            &json!("2027-01-15T08:00:01.000Z"),
            &Opts {
                ignore: &[],
                unordered: &[],
                window_ms: (1_800_000_000_000, 1_800_000_000_000),
            },
        );
        // 2027-01-15T08:00Z = 1_800_000_000_000 ms (kurang lebih) → peringatan.
        assert!(near.iter().all(|d| d.severity == Severity::Warn));
        let old = compare(
            &json!("2020-01-01T00:00:00.000Z"),
            &json!("2020-01-02T00:00:00.000Z"),
            &opts(&[], &[]),
        );
        assert_eq!(
            old.first().map(|x| x.severity.clone()),
            Some(Severity::Fail)
        );
    }

    #[test]
    fn ignore_and_unordered_pointers() {
        let ig = vec!["/data/*/createdAt".to_string()];
        let d = compare(
            &json!({"data":[{"createdAt":"x","id":1}]}),
            &json!({"data":[{"createdAt":"y","id":1}]}),
            &opts(&ig, &[]),
        );
        assert!(d.is_empty());
        let un = vec!["/tags".to_string()];
        let d = compare(
            &json!({"tags":[1,2]}),
            &json!({"tags":[2,1]}),
            &opts(&[], &un),
        );
        assert!(d.is_empty());
    }

    #[test]
    fn jwt_with_same_claims_is_warning() {
        // payload {"userId":1,"iat":1} vs {"userId":1,"iat":2}
        let a = "eyJhbGciOiJIUzI1NiJ9.eyJ1c2VySWQiOjEsImlhdCI6MX0.a";
        let b = "eyJhbGciOiJIUzI1NiJ9.eyJ1c2VySWQiOjEsImlhdCI6Mn0.b";
        let d = compare(&json!(a), &json!(b), &opts(&[], &[]));
        assert_eq!(d.first().map(|x| x.severity.clone()), Some(Severity::Warn));
    }
}
