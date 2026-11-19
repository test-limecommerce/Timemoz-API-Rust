//! Port `src/files/signed-read-url.ts` (+ `signed-read-url.entity.ts`).

use serde::Serialize;

/// URL yang diterbitkan dalam satu jendela ini identik (bisa di-cache browser).
pub const SIGNED_READ_WINDOW_MS: i64 = 10 * 60 * 1000;
/// Sisa masa berlaku minimal sejak URL diterbitkan.
pub const SIGNED_READ_MIN_VALIDITY_MS: i64 = 15 * 60 * 1000;
/// `Cache-Control` respons JSON `{ url }`.
pub const SIGNED_URL_RESPONSE_CACHE_CONTROL: &str = "private, max-age=300";

/// Respons endpoint `…/url`: `{ url, expiresAt }`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SignedReadUrl {
    pub url: String,
    pub expires_at: String,
}

/// `(accessibleAt, expires)` dalam epoch ms; `accessibleAt` dibulatkan ke awal jendela.
pub fn signed_read_window(now_ms: i64) -> (i64, i64) {
    let accessible_at = now_ms.div_euclid(SIGNED_READ_WINDOW_MS) * SIGNED_READ_WINDOW_MS;
    (
        accessible_at,
        accessible_at + SIGNED_READ_WINDOW_MS + SIGNED_READ_MIN_VALIDITY_MS,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_within_window_and_min_validity() {
        let base = 1_800_000_000_000_i64.div_euclid(SIGNED_READ_WINDOW_MS) * SIGNED_READ_WINDOW_MS;
        let a = signed_read_window(base);
        let b = signed_read_window(base + SIGNED_READ_WINDOW_MS - 1);
        assert_eq!(a, b);
        assert_ne!(a, signed_read_window(base + SIGNED_READ_WINDOW_MS));
        for now in [base, base + 1, base + SIGNED_READ_WINDOW_MS - 1] {
            let (_, expires) = signed_read_window(now);
            assert!(expires - now >= SIGNED_READ_MIN_VALIDITY_MS);
            assert!(expires - now <= SIGNED_READ_MIN_VALIDITY_MS + SIGNED_READ_WINDOW_MS);
        }
    }

    #[test]
    fn json_is_camel_case() {
        let v = serde_json::to_value(SignedReadUrl {
            url: "u".into(),
            expires_at: "t".into(),
        })
        .ok();
        assert_eq!(v, Some(serde_json::json!({"url":"u","expiresAt":"t"})));
    }
}
