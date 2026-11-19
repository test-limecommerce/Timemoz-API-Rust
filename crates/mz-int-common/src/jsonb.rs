//! Kolom `Json` Prisma (jsonb) di respons dengan urutan kunci yang SAMA dengan Node.
//!
//! Prisma membaca jsonb sebagai teks lalu `JSON.parse`, jadi urutan kunci objek = urutan
//! penyimpanan jsonb (kunci pendek dulu, lalu bytewise). Kolom dibaca sebagai teks
//! (`"col"::text`) dan dikirim lewat [`raw`] (`RawValue` hasil minify: spasi pemisah jsonb
//! dibuang, isi string tidak diubah). Sejak workspace memakai `serde_json` dengan
//! `preserve_order`, membaca kolom sebagai `serde_json::Value` juga menjaga urutan jsonb;
//! helper ini tetap dipakai karena tidak perlu parse ulang.

use serde_json::value::RawValue;

/// Teks jsonb → `RawValue` tanpa spasi di luar string (= `JSON.stringify` Node untuk
/// nilai yang sama). `None` bila teks bukan JSON valid.
pub fn raw(text: &str) -> Option<Box<RawValue>> {
    RawValue::from_string(minify(text)).ok()
}

/// `Option<String>` dari `"col"::text` → `Option<Box<RawValue>>` (`NULL` → `None`).
pub fn raw_opt(text: Option<&str>) -> Option<Box<RawValue>> {
    text.and_then(raw)
}

/// Buang whitespace di luar literal string JSON.
pub fn minify(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_str = false;
    let mut escaped = false;
    for c in text.chars() {
        if in_str {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_str = false;
            }
        } else if c == '"' {
            in_str = true;
            out.push(c);
        } else if !c.is_whitespace() {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minify_keeps_strings_and_order() {
        let t = r#"{"id": 1, "url": "a b", "name": "x \" y", "list": [1, {"k": null}]}"#;
        assert_eq!(
            minify(t),
            r#"{"id":1,"url":"a b","name":"x \" y","list":[1,{"k":null}]}"#
        );
        assert_eq!(raw(t).map(|r| r.get().len()), Some(minify(t).len()));
        assert!(raw("{bad").is_none());
        assert!(raw_opt(None).is_none());
    }
}
