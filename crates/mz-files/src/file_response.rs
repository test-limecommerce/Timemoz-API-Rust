//! Port `src/files/file-response.ts`: aturan header untuk menyajikan byte file.

use unicode_normalization::UnicodeNormalization;

use crate::encode::encode_uri_component;

/// Content-Type yang boleh diteruskan apa adanya; sisanya `application/octet-stream`.
const INLINE_CONTENT_TYPES: &[&str] = &[
    "image/jpeg",
    "image/png",
    "image/webp",
    "image/gif",
    "image/heic",
    "image/heif",
    "application/pdf",
];

pub const FALLBACK_CONTENT_TYPE: &str = "application/octet-stream";
pub const DEFAULT_PRIVATE_CACHE_CONTROL: &str = "private, max-age=300";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafeContentType {
    pub content_type: String,
    pub inline: bool,
}

/// `safeContentType`: parameter (`; charset=...`) dibuang, tipe di luar whitelist
/// menjadi `application/octet-stream` + `attachment`.
pub fn safe_content_type(raw: Option<&str>) -> SafeContentType {
    let base = raw
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_lowercase();
    if INLINE_CONTENT_TYPES.contains(&base.as_str()) {
        SafeContentType {
            content_type: base,
            inline: true,
        }
    } else {
        SafeContentType {
            content_type: FALLBACK_CONTENT_TYPE.to_owned(),
            inline: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispositionType {
    Inline,
    Attachment,
}

impl DispositionType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inline => "inline",
            Self::Attachment => "attachment",
        }
    }
}

/// `contentDisposition`: fallback `filename` ASCII tanpa kutip/backslash/kontrol,
/// nama asli lewat `filename*` (RFC 6266/5987).
pub fn content_disposition(kind: DispositionType, filename: Option<&str>) -> String {
    let Some(filename) = filename.filter(|f| !f.is_empty()) else {
        return kind.as_str().to_owned();
    };

    let fallback: String = filename
        .nfkd()
        .filter(|c| ('\u{20}'..='\u{7e}').contains(c))
        .filter(|c| *c != '"' && *c != '\\')
        .collect();
    let fallback = fallback.trim();

    let mut encoded = String::new();
    for c in encode_uri_component(filename).chars() {
        if matches!(c, '\'' | '(' | ')' | '*') {
            encoded.push_str(&format!("%{:02X}", u32::from(c)));
        } else {
            encoded.push(c);
        }
    }

    let mut parts = vec![kind.as_str().to_owned()];
    if !fallback.is_empty() {
        parts.push(format!("filename=\"{fallback}\""));
    }
    parts.push(format!("filename*=UTF-8''{encoded}"));
    parts.join("; ")
}

/// ETag kuat dari `generation`.
pub fn etag_for(generation: &str) -> String {
    format!("\"{generation}\"")
}

/// `If-None-Match` cocok dengan ETag kita (daftar, `*`, prefiks `W/` diabaikan).
pub fn if_none_match_hits(header: Option<&str>, etag: &str) -> bool {
    let Some(header) = header.filter(|h| !h.is_empty()) else {
        return false;
    };
    let strip = |t: &str| t.trim().trim_start_matches("W/").to_owned();
    let wanted = strip(etag);
    header.split(',').any(|t| {
        let tag = t.trim();
        tag == "*" || strip(tag) == wanted
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    pub start: u64,
    pub end: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeParse {
    /// Abaikan Range, kirim utuh.
    Ignore,
    /// 416.
    Unsatisfiable,
    Range(ByteRange),
}

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// `Number.isSafeInteger(Number(digits))` untuk string digit ASCII.
fn safe_int(digits: &str) -> Option<u64> {
    digits
        .parse::<u64>()
        .ok()
        .filter(|n| *n <= MAX_SAFE_INTEGER)
}

/// `parseRange`: satu rentang (`bytes=a-b`, `bytes=a-`, `bytes=-n`).
pub fn parse_range(header: Option<&str>, size: u64) -> RangeParse {
    let Some(header) = header.filter(|h| !h.is_empty()) else {
        return RangeParse::Ignore;
    };
    let Some(spec) = header.trim().strip_prefix("bytes=") else {
        return RangeParse::Ignore;
    };
    let Some((raw_start, raw_end)) = spec.split_once('-') else {
        return RangeParse::Ignore;
    };
    let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    if !digits(raw_start) || !digits(raw_end) {
        return RangeParse::Ignore;
    }
    if raw_start.is_empty() && raw_end.is_empty() {
        return RangeParse::Ignore;
    }

    if raw_start.is_empty() {
        // Sufiks: n byte terakhir.
        return match safe_int(raw_end) {
            Some(suffix) if suffix != 0 && size != 0 => RangeParse::Range(ByteRange {
                start: size.saturating_sub(suffix),
                end: size - 1,
            }),
            _ => RangeParse::Unsatisfiable,
        };
    }

    let Some(start) = safe_int(raw_start).filter(|s| *s < size) else {
        return RangeParse::Unsatisfiable;
    };
    let mut end = if raw_end.is_empty() {
        size - 1
    } else {
        match safe_int(raw_end) {
            Some(e) => e,
            None => return RangeParse::Ignore,
        }
    };
    if end < start {
        return RangeParse::Ignore;
    }
    if end > size - 1 {
        end = size - 1;
    }
    RangeParse::Range(ByteRange { start, end })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_type_whitelist() {
        let ok = safe_content_type(Some("Image/JPEG; charset=utf-8"));
        assert_eq!(
            ok,
            SafeContentType {
                content_type: "image/jpeg".into(),
                inline: true
            }
        );
        for raw in [Some("text/html"), Some("image/svg+xml"), Some(""), None] {
            let r = safe_content_type(raw);
            assert_eq!(r.content_type, FALLBACK_CONTENT_TYPE);
            assert!(!r.inline);
        }
    }

    #[test]
    fn disposition_without_filename() {
        assert_eq!(content_disposition(DispositionType::Inline, None), "inline");
        assert_eq!(
            content_disposition(DispositionType::Attachment, Some("")),
            "attachment"
        );
    }

    #[test]
    fn disposition_ascii() {
        assert_eq!(
            content_disposition(DispositionType::Attachment, Some("report.pdf")),
            "attachment; filename=\"report.pdf\"; filename*=UTF-8''report.pdf"
        );
    }

    #[test]
    fn disposition_cannot_inject_headers() {
        let h = content_disposition(DispositionType::Inline, Some("a\"b\\c\r\nX-Evil: 1.pdf"));
        assert!(!h.contains('\r') && !h.contains('\n'));
        assert!(h.starts_with("inline; filename=\"abcX-Evil: 1.pdf\""));
        assert!(h.contains("filename*=UTF-8''a%22b%5Cc%0D%0AX-Evil%3A%201.pdf"));
    }

    #[test]
    fn disposition_non_ascii_and_specials() {
        // NFKD melepas aksen: "é" -> "e".
        let h = content_disposition(DispositionType::Attachment, Some("café (1)*.jpg"));
        assert_eq!(
            h,
            "attachment; filename=\"cafe (1)*.jpg\"; filename*=UTF-8''caf%C3%A9%20%281%29%2A.jpg"
        );
        // Tanpa fallback ASCII sama sekali.
        let h = content_disposition(DispositionType::Attachment, Some("日本.pdf"));
        assert_eq!(
            h,
            "attachment; filename=\".pdf\"; filename*=UTF-8''%E6%97%A5%E6%9C%AC.pdf"
        );
        let h = content_disposition(DispositionType::Attachment, Some("日本"));
        assert_eq!(h, "attachment; filename*=UTF-8''%E6%97%A5%E6%9C%AC");
    }

    #[test]
    fn if_none_match() {
        let etag = etag_for("123");
        assert_eq!(etag, "\"123\"");
        assert!(if_none_match_hits(Some("\"123\""), &etag));
        assert!(if_none_match_hits(Some("W/\"123\""), &etag));
        assert!(if_none_match_hits(Some("\"1\", \"123\""), &etag));
        assert!(if_none_match_hits(Some("*"), &etag));
        assert!(!if_none_match_hits(Some("\"124\""), &etag));
        assert!(!if_none_match_hits(Some(""), &etag));
        assert!(!if_none_match_hits(None, &etag));
    }

    fn r(start: u64, end: u64) -> RangeParse {
        RangeParse::Range(ByteRange { start, end })
    }

    #[test]
    fn range_forms() {
        assert_eq!(parse_range(Some("bytes=0-9"), 100), r(0, 9));
        assert_eq!(parse_range(Some("bytes=10-"), 100), r(10, 99));
        assert_eq!(parse_range(Some("bytes=-10"), 100), r(90, 99));
        assert_eq!(parse_range(Some("bytes=-500"), 100), r(0, 99));
        assert_eq!(parse_range(Some("bytes=50-5000"), 100), r(50, 99));
        assert_eq!(parse_range(Some(" bytes=0-0 "), 100), r(0, 0));
    }

    #[test]
    fn range_ignored() {
        for h in [
            None,
            Some(""),
            Some("items=0-1"),
            Some("bytes=-"),
            Some("bytes=5-2"),
            Some("bytes=0-1,5-6"),
            Some("bytes=a-b"),
        ] {
            assert_eq!(parse_range(h, 100), RangeParse::Ignore, "{h:?}");
        }
    }

    #[test]
    fn range_unsatisfiable() {
        assert_eq!(
            parse_range(Some("bytes=100-"), 100),
            RangeParse::Unsatisfiable
        );
        assert_eq!(
            parse_range(Some("bytes=-0"), 100),
            RangeParse::Unsatisfiable
        );
        assert_eq!(parse_range(Some("bytes=0-1"), 0), RangeParse::Unsatisfiable);
        assert_eq!(parse_range(Some("bytes=-5"), 0), RangeParse::Unsatisfiable);
        assert_eq!(
            parse_range(Some("bytes=99999999999999999999-"), 100),
            RangeParse::Unsatisfiable
        );
    }
}
