//! `res.redirect([status,] url)` Express 4.22.1 — satu-satunya implementasi di repo.
//! Dipakai semua route yang di Node memanggil `res.redirect` / `@Redirect()` Nest.
//!
//! Express:
//! ```js
//! address = this.location(address).get('Location');   // encodeurl 2.0.0
//! this.format({
//!   text:    () => body = statuses.message[status] + '. Redirecting to ' + address,
//!   html:    () => body = '<p>' + statuses.message[status] + '. Redirecting to ' + escapeHtml(address) + '</p>',
//!   default: () => body = '',
//! });
//! ```
//! `res.format` memilih lewat `req.accepts(['text', 'html'])` (pustaka `negotiator`), selalu
//! menambah `Vary: Accept`, dan hanya menulis `Content-Type` untuk cabang text/html.
//! Tanpa header `Accept` → `text`. `Accept` yang tidak menerima keduanya (mis.
//! `application/json`) → body kosong tanpa `Content-Type`.

use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

/// `escape-html` (dipakai Express untuk body `text/html` redirect).
pub fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// `encodeurl` 2.0.0 (dipakai `res.location`): byte di luar himpunan URL aman
/// di-percent-encode (UTF-8, huruf besar); `%XX` yang sah dibiarkan, `%` lain → `%25`.
pub fn encode_url(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'%' {
            let valid = bytes
                .get(i + 1..i + 3)
                .is_some_and(|h| h.iter().all(u8::is_ascii_hexdigit));
            out.push_str(if valid { "%" } else { "%25" });
        } else if matches!(b, 0x21 | 0x23..=0x3B | 0x3D | 0x3F..=0x5F | 0x61..=0x7A | 0x7C | 0x7E) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Pilihan `res.format({ text, html, default })`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextHtml {
    Text,
    Html,
    /// Tidak ada yang diterima → cabang `default`.
    Neither,
}

/// `req.accepts(['text', 'html'])` (accepts + negotiator) untuk nilai header `Accept`.
/// `None`/kosong = tanpa header → kandidat pertama (`text`).
pub fn negotiate_text_html(accept: Option<&str>) -> TextHtml {
    let Some(accept) = accept.filter(|a| !a.is_empty()) else {
        return TextHtml::Text;
    };
    let specs = parse_accept(accept);
    // Kandidat dalam urutan Express: text/plain lalu text/html.
    let mut best: Option<(Priority, TextHtml)> = None;
    for (i, (sub, which)) in [("plain", TextHtml::Text), ("html", TextHtml::Html)]
        .into_iter()
        .enumerate()
    {
        let p = priority(&specs, sub, i);
        // `isQuality`: q > 0 (NaN tidak lolos).
        if p.q.is_nan() || p.q <= 0.0 {
            continue;
        }
        // `compareSpecs(p, best) < 0` → `p` diurutkan lebih dulu:
        // (b.q - a.q) || (b.s - a.s) || (a.o - b.o) || (a.i - b.i), dengan a = p, b = best.
        if best.as_ref().is_none_or(|(b, _)| {
            js_or(&[
                b.q - p.q,
                f64::from(b.s) - f64::from(p.s),
                p.o - b.o,
                p.i - b.i,
            ]) < 0.0
        }) {
            best = Some((p, which));
        }
    }
    best.map_or(TextHtml::Neither, |(_, w)| w)
}

#[derive(Debug, Clone, Copy)]
struct Priority {
    q: f64,
    s: u8,
    o: f64,
    i: f64,
}

struct Spec {
    ty: String,
    subtype: String,
    params: Vec<(String, String)>,
    q: f64,
    i: f64,
}

/// `a || b || c` JavaScript pada angka: nilai pertama yang truthy (bukan 0/NaN), selain itu
/// nilai terakhir.
fn js_or(vals: &[f64]) -> f64 {
    for &v in vals {
        if v != 0.0 && !v.is_nan() {
            return v;
        }
    }
    vals.last().copied().unwrap_or(0.0)
}

/// `parseFloat`: awalan angka desimal; tidak ada → NaN.
fn parse_float(s: &str) -> f64 {
    let t = s.trim_start();
    let mut end = 0;
    let mut seen_dot = false;
    let mut seen_digit = false;
    let mut seen_exp = false;
    let b = t.as_bytes();
    while let Some(&c) = b.get(end) {
        match c {
            b'+' | b'-' if end == 0 => {}
            b'+' | b'-' if seen_exp && matches!(b.get(end - 1), Some(b'e' | b'E')) => {}
            b'0'..=b'9' => seen_digit = true,
            b'.' if !seen_dot && !seen_exp => seen_dot = true,
            b'e' | b'E' if seen_digit && !seen_exp => {
                // `1e` tanpa digit eksponen → berhenti sebelum `e`.
                if !b.get(end + 1..).is_some_and(|r| {
                    r.first().is_some_and(u8::is_ascii_digit)
                        || (matches!(r.first(), Some(b'+' | b'-'))
                            && r.get(1).is_some_and(u8::is_ascii_digit))
                }) {
                    break;
                }
                seen_exp = true;
            }
            _ => break,
        }
        end += 1;
    }
    if !seen_digit {
        return if t.starts_with("Infinity") || t.starts_with("+Infinity") {
            f64::INFINITY
        } else if t.starts_with("-Infinity") {
            f64::NEG_INFINITY
        } else {
            f64::NAN
        };
    }
    t.get(..end)
        .and_then(|x| x.parse::<f64>().ok())
        .unwrap_or(f64::NAN)
}

/// Pisah pada `sep` di luar tanda kutip (`splitMediaTypes`/`splitParameters`).
fn split_quoted(s: &str, sep: char) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut quoted = false;
    for (i, c) in s.char_indices() {
        if c == '"' {
            quoted = !quoted;
        } else if c == sep && !quoted {
            out.push(s.get(start..i).unwrap_or_default());
            start = i + c.len_utf8();
        }
    }
    out.push(s.get(start..).unwrap_or_default());
    out
}

/// `parseAccept` negotiator: entri yang tidak cocok `type/subtype` dibuang.
fn parse_accept(accept: &str) -> Vec<Spec> {
    let mut out = Vec::new();
    for (i, raw) in split_quoted(accept, ',').into_iter().enumerate() {
        let raw = raw.trim();
        let (media, rest) = match raw.split_once(';') {
            Some((m, r)) => (m.trim_end(), Some(r)),
            None => (raw, None),
        };
        let Some((ty, subtype)) = media.split_once('/') else {
            continue;
        };
        // /^\s*([^\s\/;]+)\/([^;\s]+)\s*(?:;(.*))?$/
        if ty.is_empty()
            || subtype.is_empty()
            || ty.chars().any(|c| c.is_whitespace() || c == '/')
            || subtype.chars().any(char::is_whitespace)
        {
            continue;
        }
        let mut q = 1.0;
        let mut params = Vec::new();
        if let Some(rest) = rest {
            for kv in split_quoted(rest, ';') {
                let (k, v) = match kv.find('=') {
                    Some(p) => (
                        kv.get(..p).unwrap_or_default(),
                        kv.get(p + 1..).unwrap_or_default(),
                    ),
                    None => (kv, ""),
                };
                let key = k.trim().to_ascii_lowercase();
                let v = v.trim();
                let value = v
                    .strip_prefix('"')
                    .and_then(|x| x.strip_suffix('"'))
                    .unwrap_or(v);
                if key == "q" {
                    q = parse_float(value);
                    break;
                }
                params.push((key, value.to_owned()));
            }
        }
        #[allow(clippy::cast_precision_loss)]
        out.push(Spec {
            ty: ty.to_owned(),
            subtype: subtype.to_owned(),
            params,
            q,
            i: i as f64,
        });
    }
    out
}

/// `getMediaTypePriority` untuk `text/<sub>` (tanpa parameter).
fn priority(specs: &[Spec], sub: &str, index: usize) -> Priority {
    #[allow(clippy::cast_precision_loss)]
    let index = index as f64;
    let mut best = Priority {
        q: 0.0,
        s: 0,
        o: -1.0,
        i: index,
    };
    for spec in specs {
        let mut s = 0u8;
        if spec.ty.eq_ignore_ascii_case("text") {
            s |= 4;
        } else if spec.ty != "*" {
            continue;
        }
        if spec.subtype.eq_ignore_ascii_case(sub) {
            s |= 2;
        } else if spec.subtype != "*" {
            continue;
        }
        if !spec.params.is_empty() {
            // Kandidat tanpa parameter: hanya nilai `*` atau kosong yang cocok.
            if spec.params.iter().all(|(_, v)| v == "*" || v.is_empty()) {
                s |= 1;
            } else {
                continue;
            }
        }
        // (priority.s - spec.s || priority.q - spec.q || priority.o - spec.o) < 0
        if js_or(&[
            f64::from(best.s) - f64::from(s),
            best.q - spec.q,
            best.o - spec.i,
        ]) < 0.0
        {
            best = Priority {
                q: spec.q,
                s,
                o: spec.i,
                i: index,
            };
        }
    }
    best
}

/// `res.redirect(status, url)` Express 4.22.1.
pub fn express_redirect(headers: &HeaderMap, status: StatusCode, url: &str) -> Response {
    let address = encode_url(url);
    let message = status.canonical_reason().unwrap_or_default();
    let accept = headers.get(header::ACCEPT).and_then(|v| v.to_str().ok());
    let (ct, body) = match negotiate_text_html(accept) {
        TextHtml::Text => (
            Some("text/plain; charset=utf-8"),
            format!("{message}. Redirecting to {address}"),
        ),
        TextHtml::Html => (
            Some("text/html; charset=utf-8"),
            format!("<p>{message}. Redirecting to {}</p>", escape_html(&address)),
        ),
        TextHtml::Neither => (None, String::new()),
    };
    let mut res = (status, body).into_response();
    let h = res.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&address) {
        h.insert(header::LOCATION, v);
    }
    h.insert(header::VARY, HeaderValue::from_static("Accept"));
    match ct {
        Some(ct) => {
            h.insert(header::CONTENT_TYPE, HeaderValue::from_static(ct));
        }
        None => {
            h.remove(header::CONTENT_TYPE);
        }
    }
    res
}

/// `res.redirect(url)` / `res.redirect(302, url)` Express.
pub fn redirect_302(headers: &HeaderMap, url: &str) -> Response {
    express_redirect(headers, StatusCode::FOUND, url)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn neg(a: &str) -> TextHtml {
        negotiate_text_html(Some(a))
    }

    // Nilai harapan = `req.accepts(['text','html'])` Express 4.22.1 (lihat tools/parity).
    #[test]
    fn negotiation_matches_express() {
        assert_eq!(negotiate_text_html(None), TextHtml::Text);
        assert_eq!(neg(""), TextHtml::Text);
        assert_eq!(neg("*/*"), TextHtml::Text);
        assert_eq!(neg("application/json"), TextHtml::Neither);
        assert_eq!(neg("application/json, text/plain, */*"), TextHtml::Text);
        assert_eq!(
            neg("text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"),
            TextHtml::Html
        );
        assert_eq!(neg("text/*"), TextHtml::Text);
        assert_eq!(neg("text/html;q=0.5, text/plain;q=0.4"), TextHtml::Html);
        assert_eq!(neg("text/plain;q=0"), TextHtml::Neither);
        assert_eq!(neg("text/html;level=1"), TextHtml::Neither);
        assert_eq!(neg("text/html;q=abc"), TextHtml::Neither);
        assert_eq!(neg("text, html"), TextHtml::Neither);
        assert_eq!(neg("text/html;level=*"), TextHtml::Html);
        assert_eq!(neg("*/*;q=0.5, text/html;q=0.5"), TextHtml::Html);
        assert_eq!(neg("text/html, text/plain"), TextHtml::Html);
        assert_eq!(neg("text/plain;charset=utf-8"), TextHtml::Neither);
        assert_eq!(neg("TEXT/HTML"), TextHtml::Html);
        // Dulu di mz-server (redirect `/`).
        assert_eq!(neg("text/html, */*"), TextHtml::Html);
        assert_eq!(neg("text/plain, text/html"), TextHtml::Text);
        assert_eq!(neg("text/plain;q=0.5, text/html;q=0.9"), TextHtml::Html);
    }

    #[test]
    fn encodes_like_encodeurl() {
        assert_eq!(
            encode_url("http://x/a b?c=%41&d=%zz"),
            "http://x/a%20b?c=%41&d=%25zz"
        );
        assert_eq!(encode_url("/é<>\"`{}"), "/%C3%A9%3C%3E%22%60%7B%7D");
    }

    #[test]
    fn bodies() {
        let mut h = HeaderMap::new();
        let r = redirect_302(&h, "http://fe/x?a=1&b=<");
        assert_eq!(r.headers()[header::LOCATION], "http://fe/x?a=1&b=%3C");
        assert_eq!(
            r.headers()[header::CONTENT_TYPE],
            "text/plain; charset=utf-8"
        );
        h.insert(header::ACCEPT, HeaderValue::from_static("application/json"));
        let r = redirect_302(&h, "/y");
        assert!(r.headers().get(header::CONTENT_TYPE).is_none());
        assert_eq!(r.headers()[header::VARY], "Accept");
    }
}
