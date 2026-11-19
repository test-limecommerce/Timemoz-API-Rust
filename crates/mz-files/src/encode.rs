//! Padanan `encodeURIComponent` JS dan varian `fixedEncodeURIComponent`
//! (`@google-cloud/storage` `util.js`).

fn hex_digit(n: u8) -> char {
    char::from_digit(u32::from(n), 16).map_or('0', |c| c.to_ascii_uppercase())
}

fn encode_with(input: &str, keep: impl Fn(u8) -> bool) -> String {
    let mut out = String::with_capacity(input.len());
    for &b in input.as_bytes() {
        if keep(b) {
            out.push(char::from(b));
        } else {
            out.push('%');
            out.push(hex_digit(b >> 4));
            out.push(hex_digit(b & 0x0f));
        }
    }
    out
}

/// `encodeURIComponent`: tidak meng-encode `A-Za-z0-9 - _ . ! ~ * ' ( )`.
pub fn encode_uri_component(input: &str) -> String {
    encode_with(input, |b| {
        b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
            )
    })
}

/// `fixedEncodeURIComponent`: seperti di atas, tapi `! * ' ( )` ikut di-encode.
/// Hanya `A-Za-z0-9 - _ . ~` yang lolos.
pub fn fixed_encode_uri_component(input: &str) -> String {
    encode_with(input, |b| {
        b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~')
    })
}

/// `util.encodeURI(uri, encodeSlash)` milik `@google-cloud/storage`.
pub fn gcs_encode_uri(input: &str, encode_slash: bool) -> String {
    let sep = if encode_slash { "%2F" } else { "/" };
    input
        .split('/')
        .map(fixed_encode_uri_component)
        .collect::<Vec<_>>()
        .join(sep)
}

/// `decodeURIComponent`: galat bila `%` tidak diikuti dua heksadesimal atau
/// hasil bukan UTF-8 sah.
pub fn decode_uri_component(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        if b == b'%' {
            let hi = bytes.get(i + 1).and_then(|c| char::from(*c).to_digit(16))?;
            let lo = bytes.get(i + 2).and_then(|c| char::from(*c).to_digit(16))?;
            out.push(u8::try_from(hi * 16 + lo).ok()?);
            i += 3;
        } else {
            out.push(b);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn component_keeps_js_unreserved() {
        assert_eq!(
            encode_uri_component("a b/c!'()*~-_.é"),
            "a%20b%2Fc!'()*~-_.%C3%A9"
        );
    }

    #[test]
    fn fixed_encodes_sub_delims() {
        assert_eq!(fixed_encode_uri_component("a!'()*b"), "a%21%27%28%29%2Ab");
    }

    #[test]
    fn gcs_encode_slash_modes() {
        assert_eq!(gcs_encode_uri("a/b c", false), "a/b%20c");
        assert_eq!(gcs_encode_uri("a/b c", true), "a%2Fb%20c");
    }

    #[test]
    fn decode_rejects_malformed() {
        assert_eq!(decode_uri_component("a%20b").as_deref(), Some("a b"));
        assert_eq!(decode_uri_component("%E0%A4%A"), None);
        assert_eq!(decode_uri_component("%E0%A4"), None);
        assert_eq!(decode_uri_component("%zz"), None);
        assert_eq!(decode_uri_component("%"), None);
    }
}
