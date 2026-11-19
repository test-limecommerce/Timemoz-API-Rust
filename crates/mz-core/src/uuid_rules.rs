//! Tiga aturan UUID Node yang BERBEDA (diverifikasi terhadap `node_modules` Mudah-API):
//!
//! | Node | Aturan | Fungsi |
//! |---|---|---|
//! | `ParseUUIDPipe` (Nest 10.4, tanpa `version`) | bentuk `8-4-4-4-12` hex, tak peka huruf | [`is_uuid_shape`], [`parse_uuid_pipe`] |
//! | `@IsUUID()` / `@IsUUID('all')` (class-validator 0.14 → validator 13.15 `all`) | digit versi `1-8`, varian `8/9/a/b`, atau nil/max | [`is_uuid`] |
//! | `@IsUUID('4')` / `ParseUUIDPipe({ version: '4' })` | versi `4`, varian `8/9/a/b` | [`is_uuid_v4`] |
//!
//! Jadi `12345678-1234-1234-1234-123456789012` lolos `ParseUUIDPipe` tetapi ditolak `@IsUUID()`.
//! Kolom Prisma `@db.Uuid` memakai aturan lain lagi (juga menerima bentuk tanpa tanda hubung,
//! `{...}`, `urn:uuid:`): lihat [`crate::prisma`].

use crate::ApiError;

const NIL: &str = "00000000-0000-0000-0000-000000000000";
const MAX: &str = "ffffffff-ffff-ffff-ffff-ffffffffffff";

/// `8-4-4-4-12` heksadesimal; `pos(i, c)` memeriksa posisi selain tanda hubung.
fn shaped(s: &str, pos: impl Fn(usize, u8) -> bool) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, &c)| match i {
            8 | 13 | 18 | 23 => c == b'-',
            _ => c.is_ascii_hexdigit() && pos(i, c.to_ascii_lowercase()),
        })
}

/// `ParseUUIDPipe` tanpa versi (regex `all` Nest = `loose` validator.js).
pub fn is_uuid_shape(s: &str) -> bool {
    shaped(s, |_, _| true)
}

/// class-validator `@IsUUID()` / `@IsUUID('all')`.
pub fn is_uuid(s: &str) -> bool {
    s.eq_ignore_ascii_case(NIL)
        || s.eq_ignore_ascii_case(MAX)
        || shaped(s, |i, c| match i {
            14 => (b'1'..=b'8').contains(&c),
            19 => matches!(c, b'8' | b'9' | b'a' | b'b'),
            _ => true,
        })
}

/// `@IsUUID('4')`.
pub fn is_uuid_v4(s: &str) -> bool {
    shaped(s, |i, c| match i {
        14 => c == b'4',
        19 => matches!(c, b'8' | b'9' | b'a' | b'b'),
        _ => true,
    })
}

/// `ParseUUIDPipe`: 400 `Validation failed (uuid is expected)`. Bentuk yang lolos selalu
/// terbaca sebagai `Uuid`.
pub fn parse_uuid_pipe(raw: &str) -> Result<uuid::Uuid, ApiError> {
    is_uuid_shape(raw)
        .then(|| uuid::Uuid::parse_str(raw).ok())
        .flatten()
        .ok_or_else(|| ApiError::bad_request("Validation failed (uuid is expected)"))
}

/// [`parse_uuid_pipe`] untuk param path `String` yang diteruskan apa adanya.
pub fn uuid_pipe(raw: String) -> Result<String, ApiError> {
    parse_uuid_pipe(&raw).map(|_| raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (input, ParseUUIDPipe, isUUID(all), isUUID(4)) dari Node (2026-11-16).
    const NODE: &[(&str, bool, bool, bool)] = &[
        ("12345678-1234-1234-1234-123456789012", true, false, false),
        ("12345678123412341234123456789012", false, false, false),
        (
            "{12345678-1234-1234-1234-123456789012}",
            false,
            false,
            false,
        ),
        (
            "urn:uuid:12345678-1234-1234-1234-123456789012",
            false,
            false,
            false,
        ),
        ("ABCDEF01-2345-4789-8abc-def012345678", true, true, true),
        ("00000000-0000-0000-0000-000000000000", true, true, false),
        ("ffffffff-ffff-ffff-ffff-ffffffffffff", true, true, false),
        ("12345678-1234-1234-1234-12345678901g", false, false, false),
    ];

    #[test]
    fn matches_node() {
        for &(s, pipe, all, v4) in NODE {
            assert_eq!(is_uuid_shape(s), pipe, "pipe {s}");
            assert_eq!(parse_uuid_pipe(s).is_ok(), pipe, "pipe {s}");
            assert_eq!(is_uuid(s), all, "all {s}");
            assert_eq!(is_uuid_v4(s), v4, "v4 {s}");
        }
        // Versi 1-8 lolos `all`; 0 dan 9 tidak. Varian `c` ditolak. Huruf besar sah.
        assert!(is_uuid("12345678-1234-8234-a234-123456789012"));
        assert!(!is_uuid("12345678-1234-9234-a234-123456789012"));
        assert!(!is_uuid("12345678-1234-0234-a234-123456789012"));
        assert!(!is_uuid("12345678-1234-4234-c234-123456789012"));
        assert!(is_uuid("FFFFFFFF-FFFF-FFFF-FFFF-FFFFFFFFFFFF"));
    }
}
