//! Pesan galat Prisma yang bisa ditiru persis.
//!
//! Filter Prisma pada kolom `@db.Uuid` dengan string non-UUID melempar P2023
//! `Inconsistent column data: Error creating UUID, <galat>`. `<galat>` berasal dari crate
//! `uuid` versi lama di query engine Prisma 5. Crate `uuid` 1.29 di workspace ini memakai
//! teks lain (mis. "found `z` at 0"), jadi `InvalidUuid::into_err` versi lama di-port di sini.
//! Diverifikasi terhadap Prisma Mudah-API (19 kasus di test bawah).
//!
//! Node membungkusnya dengan prefiks "Invalid \`prisma.x.findFirst()\` invocation in <file>..."
//! yang tidak ditiru; yang dikembalikan hanya baris terakhir (`e.message` baris terakhir).

/// `None` bila `input` UUID sah (simple 32 hex, hyphenated, `{...}`, `urn:uuid:...`), selain itu
/// pesan P2023 lengkap.
pub fn uuid_error(input: &str) -> Option<String> {
    if uuid::Uuid::try_parse(input).is_ok() {
        return None;
    }
    Some(format!(
        "Inconsistent column data: Error creating UUID, {}",
        legacy_uuid_error(input)
    ))
}

/// `uuid::Uuid::parse_str(input)` → `Ok` bila sah, `Err(pesan P2023)` bila tidak.
pub fn parse_uuid(input: &str) -> Result<uuid::Uuid, String> {
    match uuid_error(input) {
        None => uuid::Uuid::try_parse(input).map_err(|e| e.to_string()),
        Some(msg) => Err(msg),
    }
}

/// Port `InvalidUuid::into_err` + `Display for ErrorKind` (crate `uuid` versi Prisma).
fn legacy_uuid_error(input: &str) -> String {
    const URN: &str = "urn:uuid:";
    let (body, offset, simple) =
        if let Some(s) = input.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
            // Terverifikasi: posisi karakter TIDAK menghitung `{` pembuka (beda dengan `urn:uuid:`).
            (s, 0, false)
        } else if let Some(s) = input.strip_prefix(URN) {
            (s, URN.len(), false)
        } else {
            (input, 0, true)
        };

    let mut hyphens = 0usize;
    let mut bounds = [0usize; 4];
    for (index, c) in body.char_indices() {
        if c == '-' {
            if let Some(slot) = bounds.get_mut(hyphens) {
                *slot = index;
            }
            hyphens += 1;
        } else if !c.is_ascii_hexdigit() {
            return format!(
                "invalid character: expected an optional prefix of `urn:uuid:` followed by \
                 [0-9a-fA-F-], found `{c}` at {}",
                index + offset + 1
            );
        }
    }
    if hyphens == 0 && simple {
        return format!(
            "invalid length: expected length 32 for simple format, found {}",
            input.len()
        );
    }
    if hyphens != 4 {
        return format!("invalid group count: expected 5, found {}", hyphens + 1);
    }
    const STARTS: [usize; 5] = [0, 9, 14, 19, 24];
    const EXPECTED: [usize; 5] = [8, 4, 4, 4, 12];
    for (group, ((bound, start), next)) in bounds
        .iter()
        .zip(STARTS.iter())
        .zip(STARTS.iter().skip(1))
        .enumerate()
    {
        if *bound != next - 1 {
            let expected = EXPECTED.get(group).copied().unwrap_or_default();
            return format!(
                "invalid group length in group {group}: expected {expected}, found {}",
                bound.saturating_sub(*start)
            );
        }
    }
    // Grup terakhir terlalu panjang (versi lama memakai panjang input UTUH).
    format!(
        "invalid group length in group 4: expected 12, found {}",
        input.len().saturating_sub(24)
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const CHAR: &str = "invalid character: expected an optional prefix of `urn:uuid:` followed by [0-9a-fA-F-], found";

    /// Keluaran Prisma 5 Mudah-API (`project.findFirst({ where: { uuid } })`), 2026-11-16.
    #[test]
    fn matches_prisma() {
        let cases: &[(&str, String)] = &[
            ("not-a-uuid", format!("{CHAR} `n` at 1")),
            ("zz", format!("{CHAR} `z` at 1")),
            (
                "",
                "invalid length: expected length 32 for simple format, found 0".into(),
            ),
            ("abc-def", "invalid group count: expected 5, found 2".into()),
            (
                "1234567-1234-1234-1234-123456789012",
                "invalid group length in group 0: expected 8, found 7".into(),
            ),
            (
                "12345678-1234-1234-1234-1234567890123",
                "invalid group length in group 4: expected 12, found 13".into(),
            ),
            (
                "12345678-123-1234-1234-123456789012",
                "invalid group length in group 1: expected 4, found 3".into(),
            ),
            (
                "{12345678-1234-1234-1234-12345678901z}",
                format!("{CHAR} `z` at 36"),
            ),
            ("urn:uuid:zz", format!("{CHAR} `z` at 10")),
            (
                "0123456789abcdef0123456789abcdef0",
                "invalid length: expected length 32 for simple format, found 33".into(),
            ),
            (
                "0123456789abcdef0123456789abcdeg",
                format!("{CHAR} `g` at 32"),
            ),
            (
                "12345678-1234-1234-1234-12345678901é",
                format!("{CHAR} `é` at 36"),
            ),
            ("x日本", format!("{CHAR} `x` at 1")),
            (
                "12345678-1234-1234-1234-123456789012-ab",
                "invalid group count: expected 5, found 6".into(),
            ),
            (
                "a-b-c-d-e-f",
                "invalid group count: expected 5, found 6".into(),
            ),
            (
                "----",
                "invalid group length in group 0: expected 8, found 0".into(),
            ),
            ("{}", "invalid group count: expected 5, found 1".into()),
            (
                "urn:uuid:",
                "invalid group count: expected 5, found 1".into(),
            ),
        ];
        for (input, want) in cases {
            assert_eq!(
                uuid_error(input).as_deref(),
                Some(format!("Inconsistent column data: Error creating UUID, {want}").as_str()),
                "input {input:?}"
            );
        }
        assert_eq!(uuid_error("12345678123412341234123456789012"), None);
        assert_eq!(uuid_error("12345678-1234-1234-1234-123456789012"), None);
        assert!(parse_uuid("{12345678-1234-1234-1234-123456789012}").is_ok());
        assert!(parse_uuid("zz").unwrap_err().ends_with("found `z` at 1"));
    }
}
