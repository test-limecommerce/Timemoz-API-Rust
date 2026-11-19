//! Utilitas kecil yang meniru perilaku JavaScript.

/// `encodeURIComponent`: yang TIDAK di-encode hanya `A-Z a-z 0-9 - _ . ! ~ * ' ( )`.
pub fn encode_uri_component(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
            )
        {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// `${process.env.X}` — variabel yang tidak ada tercetak sebagai teks `undefined`
/// di Node (kita meniru supaya URL yang dihasilkan identik, termasuk kasus salah konfigurasi).
pub fn env_or_undefined(value: Option<String>) -> String {
    value.unwrap_or_else(|| "undefined".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_like_js() {
        assert_eq!(encode_uri_component("a b+c@x.com"), "a%20b%2Bc%40x.com");
        assert_eq!(encode_uri_component("v1.2.a-_b"), "v1.2.a-_b");
        assert_eq!(encode_uri_component("é"), "%C3%A9");
        assert_eq!(encode_uri_component("!~*'()"), "!~*'()");
    }
}
