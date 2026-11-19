//! Port `src/orders/order-cursor.ts` — kursor keyset `(updatedAt, id)` §10.3, dipakai
//! `GET /v1/mobile/orders` dan `GET /v1/mobile/order-deliveries`.

use base64::Engine;
use base64::engine::DecodePaddingMode;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};
use chrono::NaiveDateTime;
use mz_core::ApiError;

use crate::common::bad_request;

/// §10.3 — jendela delta tidak pernah mencapai "sekarang".
pub const ORDER_DELTA_SAFETY_LAG_MS: i64 = 5_000;

const B64URL: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::URL_SAFE,
    GeneralPurposeConfig::new()
        .with_encode_padding(false)
        .with_decode_padding_mode(DecodePaddingMode::Indifferent)
        .with_decode_allow_trailing_bits(true),
);

/// Node: src/orders/order-cursor.ts:14 `encodeOrderCursor`.
pub fn encode_order_cursor(updated_at: NaiveDateTime, id: i32) -> String {
    B64URL.encode(format!("{}:{id}", updated_at.and_utc().timestamp_millis()))
}

/// `Buffer.from(s, 'base64url').toString('utf8')` (toleran).
fn decode_lenient(raw: &str) -> String {
    let mut cleaned: String = raw
        .chars()
        .take_while(|c| *c != '=')
        .filter_map(|c| match c {
            '+' => Some('-'),
            '/' => Some('_'),
            c if c.is_ascii_alphanumeric() || c == '-' || c == '_' => Some(c),
            _ => None,
        })
        .collect();
    if cleaned.len() % 4 == 1 {
        cleaned.pop();
    }
    B64URL
        .decode(cleaned.as_bytes())
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default()
}

/// Node: src/orders/order-cursor.ts:18 `decodeOrderCursor` → `(updatedAt, id)`.
pub fn decode_order_cursor(cursor: &str) -> Result<(NaiveDateTime, i64), ApiError> {
    let decoded = decode_lenient(cursor);
    let mut parts = decoded.split(':');
    let ms = parts.next().map_or(f64::NAN, mz_core::jsfmt::number);
    let id = parts.next().map_or(f64::NAN, mz_core::jsfmt::number);
    let invalid = || bad_request("INVALID_CURSOR", "cursor is not a valid pagination token");
    if !ms.is_finite() || !id.is_finite() {
        return Err(invalid());
    }
    #[allow(clippy::cast_possible_truncation)]
    let at = chrono::DateTime::from_timestamp_millis(ms.trunc() as i64)
        .ok_or_else(invalid)?
        .naive_utc();
    #[allow(clippy::cast_possible_truncation)]
    Ok((at, id.trunc() as i64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_invalid() {
        let at = chrono::DateTime::from_timestamp_millis(1_791_187_200_123)
            .unwrap()
            .naive_utc();
        let c = encode_order_cursor(at, 42);
        assert_eq!(c, B64URL.encode("1791187200123:42"));
        assert_eq!(decode_order_cursor(&c).unwrap(), (at, 42));
        assert!(decode_order_cursor("!!!").is_err());
        assert!(decode_order_cursor(&B64URL.encode("123")).is_err());
        assert!(decode_order_cursor(&B64URL.encode("abc:1")).is_err());
    }
}
