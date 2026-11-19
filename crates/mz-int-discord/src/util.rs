//! Helper murni peniru semantik JavaScript yang dipakai `discord.controller.ts` /
//! `discord.service.ts` (truthiness, `String(v)`, `encodeURIComponent`, regex snowflake,
//! panjang string ala validator.js, parser query `qs` Express).

use serde_json::Value;

/// `SNOWFLAKE_REGEX = /^\d{17,20}$/` (`\d` JS = digit ASCII; `$` tanpa flag `m`).
pub fn is_snowflake(s: &str) -> bool {
    (17..=20).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit())
}

/// `encodeURIComponent` (UTF-8, huruf hex besar).
pub fn encode_uri_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `String(v)` / interpolasi template literal (`None` = `undefined`).
pub fn js_string(v: Option<&Value>) -> String {
    match v {
        None => "undefined".to_owned(),
        Some(Value::Null) => "null".to_owned(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => js_number(n),
        Some(Value::Array(items)) => items
            .iter()
            .map(|i| match i {
                Value::Null => String::new(),
                other => js_string(Some(other)),
            })
            .collect::<Vec<_>>()
            .join(","),
        Some(Value::Object(_)) => "[object Object]".to_owned(),
    }
}

fn js_number(n: &serde_json::Number) -> String {
    if let Some(i) = n.as_i64() {
        return i.to_string();
    }
    if let Some(u) = n.as_u64() {
        return u.to_string();
    }
    match n.as_f64() {
        Some(f) if f.is_finite() && f.fract() == 0.0 && f.abs() < 1e21 => format!("{f:.0}"),
        Some(f) => f.to_string(),
        None => n.to_string(),
    }
}

/// Truthiness JS (`None` = `undefined`).
pub fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(_) | Value::Object(_)) => true,
    }
}

/// `a ?? b` sisi kiri: `Some` hanya bila bukan `null`/`undefined`.
pub fn nn(v: Option<&Value>) -> Option<&Value> {
    v.filter(|v| !v.is_null())
}

/// `v?.key` untuk nilai yang mungkin `undefined`/`null`/primitif.
pub fn opt_get<'a>(v: Option<&'a Value>, key: &str) -> Option<&'a Value> {
    match v {
        Some(Value::Object(m)) => m.get(key),
        _ => None,
    }
}

/// Panjang string menurut `validator.isLength` (dipakai `@MaxLength`): unit UTF-16
/// dikurangi pasangan surrogate dan selektor variasi U+FE0E/U+FE0F.
pub fn validator_len(s: &str) -> usize {
    s.chars()
        .filter(|c| *c != '\u{FE0E}' && *c != '\u{FE0F}')
        .count()
}

/// `str.length` JS (unit UTF-16).
pub fn js_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// `new Date().toISOString()`.
pub fn iso_now() -> String {
    mz_core::time::to_js_iso(&chrono::Utc::now())
}

/// Query Express (`qs`): satu sumber di `mz_core::qs`.
pub use mz_core::qs::QVal;

/// Ambil satu kunci dari query mentah (`req.query[name]`).
pub fn query_param(raw: &str, name: &str) -> Option<QVal> {
    mz_core::qs::get(&mz_core::qs::parse(raw), name).cloned()
}

/// `validator.isURL(v, { require_protocol: true, host_whitelist: ['cdn.discordapp.com'] })`
/// (validator 13.15; opsi lain bawaan).
pub fn is_discord_cdn_url(url: &str) -> bool {
    if url.is_empty()
        || url
            .chars()
            .any(|c| c.is_whitespace() || c == '<' || c == '>')
        || url.starts_with("mailto:")
        || js_len(url) > 2084
    {
        return false;
    }
    let url = url.split('#').next().unwrap_or("");
    let url = url.split('?').next().unwrap_or("");
    // `/^([a-z][a-z0-9+\-.]*):/i`
    let proto_len = {
        let mut chars = url.char_indices();
        match chars.next() {
            Some((_, c)) if c.is_ascii_alphabetic() => {
                let mut end = None;
                for (i, c) in chars {
                    if c == ':' {
                        end = Some(i);
                        break;
                    }
                    if !(c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.') {
                        break;
                    }
                }
                end
            }
            _ => None,
        }
    };
    let Some(plen) = proto_len else {
        return false; // require_protocol
    };
    let protocol = url.get(..plen).unwrap_or("").to_ascii_lowercase();
    let after = url.get(plen + 1..).unwrap_or("");
    if !after.starts_with("//") {
        let before_slash = after.split('/').next().unwrap_or("");
        if let Some(at) = before_slash.find('@') {
            let before_at = before_slash.get(..at).unwrap_or("");
            let valid_auth = before_at
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.%:".contains(c));
            let encoded = before_at.as_bytes().windows(3).any(|w| {
                w.first() == Some(&b'%')
                    && w.get(1).is_some_and(u8::is_ascii_hexdigit)
                    && w.get(2).is_some_and(u8::is_ascii_hexdigit)
            });
            if valid_auth && !encoded {
                return false;
            }
        } else if after.starts_with(|c: char| c.is_ascii_digit()) {
            return false;
        }
    }
    if !["http", "https", "ftp"].contains(&protocol.as_str()) {
        return false;
    }
    let mut rest = after;
    if let Some(r) = rest.strip_prefix("//") {
        rest = r;
    }
    if rest.is_empty() {
        return false;
    }
    let authority = rest.split('/').next().unwrap_or("");
    let mut parts: Vec<&str> = authority.split('@').collect();
    if parts.len() > 1 {
        if parts.first() == Some(&"") {
            return false;
        }
        let auth = parts.remove(0);
        if auth.split(':').count() > 2 {
            return false;
        }
        let mut up = auth.split(':');
        let (user, password) = (up.next(), up.next());
        if user == Some("") && password == Some("") {
            return false;
        }
    }
    let hostname = parts.join("@");
    if hostname.starts_with('[') {
        // IPv6 terbungkus: host '' → tidak ada di whitelist.
        return false;
    }
    let mut hp = hostname.split(':');
    let host = hp.next().unwrap_or("");
    let port: Vec<&str> = hp.collect();
    if !port.is_empty() {
        let port_str = port.join(":");
        if !port_str.is_empty() {
            let ok = port_str.bytes().all(|b| b.is_ascii_digit())
                && port_str
                    .parse::<u64>()
                    .is_ok_and(|p| (1..=65535).contains(&p));
            if !ok {
                return false;
            }
        }
    }
    host == "cdn.discordapp.com"
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn snowflake() {
        assert!(is_snowflake("12345678901234567"));
        assert!(is_snowflake("12345678901234567890"));
        assert!(!is_snowflake("1234567890123456"));
        assert!(!is_snowflake("123456789012345678901"));
        assert!(!is_snowflake("1234567890123456a"));
        assert!(!is_snowflake("12345678901234567\n"));
    }

    #[test]
    fn uri_component() {
        assert_eq!(
            encode_uri_component("http://a.b/x?y=1&z=é ()"),
            "http%3A%2F%2Fa.b%2Fx%3Fy%3D1%26z%3D%C3%A9%20()"
        );
        assert_eq!(encode_uri_component("a.b-c_d!~*'"), "a.b-c_d!~*'");
    }

    #[test]
    fn js_semantics() {
        assert_eq!(js_string(None), "undefined");
        assert_eq!(js_string(Some(&json!(5.0))), "5");
        assert_eq!(js_string(Some(&json!(["a", null, 1]))), "a,,1");
        assert!(!truthy(Some(&json!(""))));
        assert!(truthy(Some(&json!([]))));
        assert!(!truthy(Some(&json!(0))));
        assert_eq!(validator_len("a\u{FE0F}😀"), 2);
        assert_eq!(js_len("😀"), 2);
    }

    #[test]
    fn qs_params() {
        assert_eq!(
            query_param("guild_id=1&state=x", "guild_id"),
            Some(QVal::One("1".into()))
        );
        assert_eq!(
            query_param("error=a+b%20c&error=d", "error"),
            Some(QVal::Many(vec!["a b c".into(), "d".into()]))
        );
        assert_eq!(
            query_param("e[]=x", "e"),
            Some(QVal::Many(vec!["x".into()]))
        );
        assert_eq!(query_param("e[k]=x", "e"), Some(QVal::Obj));
        assert_eq!(
            query_param("error=", "error"),
            Some(QVal::One(String::new()))
        );
        assert_eq!(
            query_param("error", "error"),
            Some(QVal::One(String::new()))
        );
        assert_eq!(query_param("x=1", "error"), None);
        assert_eq!(
            query_param("m=%E0%A4%A", "m"),
            Some(QVal::One("%E0%A4%A".into()))
        );
    }

    #[test]
    fn cdn_url() {
        assert!(is_discord_cdn_url(
            "https://cdn.discordapp.com/avatars/1/a.png"
        ));
        assert!(is_discord_cdn_url("HTTP://cdn.discordapp.com:443/x?y#z"));
        assert!(is_discord_cdn_url("ftp://user:pw@cdn.discordapp.com/x"));
        assert!(!is_discord_cdn_url("cdn.discordapp.com/x"));
        assert!(!is_discord_cdn_url("https://evil.com/x"));
        assert!(!is_discord_cdn_url("https://CDN.discordapp.com/x"));
        assert!(!is_discord_cdn_url("javascript://cdn.discordapp.com/x"));
        assert!(!is_discord_cdn_url("https://cdn.discordapp.com:0/x"));
        assert!(!is_discord_cdn_url("https://cdn.discordapp.com /x"));
        assert!(!is_discord_cdn_url("https://"));
    }
}
