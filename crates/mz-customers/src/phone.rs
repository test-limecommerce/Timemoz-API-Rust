//! Port `src/store-visits/store-phone.util.ts` (`normalizeStorePhone`) — satu-satunya tempat aturan
//! normalisasi telepon toko/customer ditulis. `pub` supaya `mz-store-visits` (tahap S3) memakainya
//! (jalur tulis toko admin & mobile).

/// `\s` JS: `[\t\n\v\f\r    -     　﻿]`.
fn is_js_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{000B}' | '\u{000C}' | '\r' | ' ' | '\u{00A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

/// `normalizeStorePhone`: spasi & tanda hubung dibuang; `08xx` -> `+628xx`, `62xx` -> `+62xx`,
/// yang sudah berawalan `+` dibiarkan, selain itu diberi `+62`. `None`/`null` -> `None`.
pub fn normalize_store_phone(phone: Option<&str>) -> Option<String> {
    let phone = phone?;
    let digits: String = phone
        .chars()
        .filter(|c| !is_js_space(*c) && *c != '-')
        .collect();
    if digits.starts_with('+') {
        return Some(digits);
    }
    if digits.starts_with("62") {
        return Some(format!("+{digits}"));
    }
    if let Some(rest) = digits.strip_prefix('0') {
        return Some(format!("+62{rest}"));
    }
    Some(format!("+62{digits}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(s: &str) -> Option<String> {
        normalize_store_phone(Some(s))
    }

    #[test]
    fn local_zero_prefix() {
        assert_eq!(n("081234567890").as_deref(), Some("+6281234567890"));
        assert_eq!(n("0812-3456-7890").as_deref(), Some("+6281234567890"));
        assert_eq!(n("0812 3456 7890").as_deref(), Some("+6281234567890"));
    }

    #[test]
    fn country_code_without_plus() {
        assert_eq!(n("6281234567890").as_deref(), Some("+6281234567890"));
    }

    #[test]
    fn plus_prefix_is_kept() {
        assert_eq!(n("+6281234567890").as_deref(), Some("+6281234567890"));
        assert_eq!(n("+1 415-555").as_deref(), Some("+1415555"));
    }

    #[test]
    fn bare_digits_get_plus_62() {
        assert_eq!(n("81234").as_deref(), Some("+6281234"));
    }

    #[test]
    fn null_stays_null_empty_gets_prefix() {
        assert_eq!(normalize_store_phone(None), None);
        assert_eq!(n("").as_deref(), Some("+62"));
    }

    #[test]
    fn js_whitespace_includes_nbsp_and_bom() {
        assert_eq!(n("0812\u{00A0}34\u{FEFF}5").as_deref(), Some("+62812345"));
    }
}
