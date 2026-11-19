//! Fungsi murni modul produk: konversi UOM (`uom-conversion.util.ts`), rentang qty
//! (`qty-range.util.ts`), price list aktif (`price-list-active.util.ts`), URL gambar produk
//! (`product-image-url.ts`) dan SSRF guard (`ssrf-guard.ts`).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::str::FromStr;
use std::sync::LazyLock;

use chrono::{DateTime, NaiveDate, Utc};
use regex::Regex;
use rust_decimal::{Decimal, RoundingStrategy};

use crate::dto::re;

// ───────────────────────── UOM ─────────────────────────

/// `UomOperator`.
pub const UOM_OPERATORS: &[&str] = &["multiply", "divide"];
/// `UomType`.
pub const UOM_TYPES: &[&str] = &["unit", "weight", "volume", "length"];
/// `productUom.conversionRate`/`factor` = `Decimal(18,6)`.
pub const CONVERSION_SCALE: u32 = 6;

/// `computeBaseEquivalent(operator, factor)` (uom-conversion.util.ts:25): `multiply` → factor,
/// `divide` → `1 / factor` dibulatkan half-up 6 desimal. `None` bila factor ≤ 0 atau hasil
/// pembulatan nol (pemanggil menjawab 422).
pub fn compute_base_equivalent(operator: &str, factor: &str) -> Option<String> {
    let value = Decimal::from_str(factor.trim()).ok()?;
    if value <= Decimal::ZERO {
        return None;
    }
    let result = if operator == "divide" {
        Decimal::ONE
            .checked_div(value)?
            .round_dp_with_strategy(CONVERSION_SCALE, RoundingStrategy::MidpointAwayFromZero)
    } else {
        value.round_dp_with_strategy(CONVERSION_SCALE, RoundingStrategy::MidpointAwayFromZero)
    };
    if result.is_zero() {
        None
    } else {
        Some(result.normalize().to_string())
    }
}

/// Pesan 422 `PRODUCT_UOM_INVALID_FACTOR` / `UOM_INVALID_DEFAULT_FACTOR` (`field` = `factor`
/// atau `defaultFactor`).
pub fn invalid_factor_message(operator: &str, field: &str) -> String {
    if operator == "divide" {
        format!("{field} is too large for divide — 1 / {field} rounds to 0 at 6 decimal places")
    } else {
        format!("{field} must be greater than 0")
    }
}

// ───────────────────────── qty range ─────────────────────────

/// `compareDecimalStrings(a, b)` (qty-range.util.ts:17) untuk desimal non-negatif.
pub fn compare_decimal_strings(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (a_int, a_frac) = a.split_once('.').unwrap_or((a, ""));
    let (b_int, b_frac) = b.split_once('.').unwrap_or((b, ""));
    let strip = |s: &str| -> String {
        let t = s.trim_start_matches('0');
        if t.is_empty() && !s.is_empty() {
            "0".to_owned()
        } else {
            t.to_owned()
        }
    };
    let (ai, bi) = (strip(a_int), strip(b_int));
    match ai.len().cmp(&bi.len()) {
        Ordering::Equal => {}
        other => return other,
    }
    match ai.cmp(&bi) {
        Ordering::Equal => {}
        other => return other,
    }
    let len = a_frac.len().max(b_frac.len());
    let pad = |s: &str| format!("{s:0<len$}");
    pad(a_frac).cmp(&pad(b_frac))
}

/// `QtyRange` — `max = None` = tak terbatas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QtyRange {
    pub min: String,
    pub max: Option<String>,
}

/// `qtyRangesOverlap(a, b)` (qty-range.util.ts:43), batas inklusif.
pub fn qty_ranges_overlap(a: &QtyRange, b: &QtyRange) -> bool {
    let a_ok = a
        .max
        .as_deref()
        .is_none_or(|m| compare_decimal_strings(m, &b.min).is_ge());
    let b_ok = b
        .max
        .as_deref()
        .is_none_or(|m| compare_decimal_strings(m, &a.min).is_ge());
    a_ok && b_ok
}

// ───────────────────────── price list aktif ─────────────────────────

/// `isPriceListActiveToday(priceList, timeZone, now)` (price-list-active.util.ts:15): "hari ini"
/// menurut `company.timeZone`, tanggal kolom `@db.Date` dibaca apa adanya.
pub fn is_price_list_active_today(
    is_active: bool,
    start: Option<NaiveDate>,
    end: Option<NaiveDate>,
    time_zone: &str,
    now: DateTime<Utc>,
) -> bool {
    if !is_active {
        return false;
    }
    let tz = mz_core::tz::zone_or_utc(Some(time_zone));
    let today = now.with_timezone(&tz).date_naive();
    if start.is_some_and(|s| today < s) {
        return false;
    }
    if end.is_some_and(|e| today > e) {
        return false;
    }
    true
}

// ───────────────────────── URL gambar ─────────────────────────

/// `MAX_PRODUCT_IMAGE_BYTES` (upload manual & unduhan import sama).
pub const MAX_PRODUCT_IMAGE_BYTES: u64 = 5 * 1024 * 1024;

/// `EXTENSION_BY_CONTENT_TYPE`.
pub fn extension_for_content_type(content_type: &str) -> Option<&'static str> {
    match content_type {
        "image/jpeg" => Some("jpg"),
        "image/png" => Some("png"),
        "image/webp" => Some("webp"),
        _ => None,
    }
}

static FILE_NAME: LazyLock<Regex> = LazyLock::new(|| {
    re(r"/([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})\.(jpg|png|webp)$")
});

/// `productImageFileId(objectKey)`: segmen nama file (uuid) bila key berbentuk key gambar produk.
pub fn product_image_file_id(object_key: Option<&str>) -> Option<String> {
    let key = object_key.filter(|k| !k.is_empty())?;
    FILE_NAME
        .captures(key)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_owned())
}

/// `productImagePath(productUuid, objectKey)` — bentuk yang disimpan ke kolom `imageUrl`.
pub fn product_image_path(product_uuid: &str, object_key: Option<&str>) -> Option<String> {
    let file_id = product_image_file_id(object_key)?;
    Some(format!(
        "/products/{}/image/{file_id}",
        mz_files::encode::encode_uri_component(product_uuid)
    ))
}

/// `productImageUrl(productUuid, objectKey)` = `HOST_BE` + path.
pub fn product_image_url(
    state: &mz_core::AppState,
    product_uuid: &str,
    object_key: Option<&str>,
) -> Option<String> {
    product_image_path(product_uuid, object_key).map(|p| mz_files::public_api_url(state, &p))
}

// ───────────────────────── SSRF guard ─────────────────────────

fn in_v4(ip: Ipv4Addr, base: [u8; 4], bits: u32) -> bool {
    let mask = if bits == 0 {
        0
    } else {
        u32::MAX << (32 - bits)
    };
    (u32::from(ip) & mask) == (u32::from(Ipv4Addr::from(base)) & mask)
}

/// §8.2: `0/8`, `10/8`, `100.64/10`, `127/8`, `169.254/16`, `172.16/12`, `192.168/16`.
fn is_blocked_v4(ip: Ipv4Addr) -> bool {
    const RANGES: &[([u8; 4], u32)] = &[
        ([0, 0, 0, 0], 8),
        ([10, 0, 0, 0], 8),
        ([100, 64, 0, 0], 10),
        ([127, 0, 0, 0], 8),
        ([169, 254, 0, 0], 16),
        ([172, 16, 0, 0], 12),
        ([192, 168, 0, 0], 16),
    ];
    RANGES.iter().any(|(b, bits)| in_v4(ip, *b, *bits))
}

/// `::1`, `::`, `::ffff:a.b.c.d` (diperiksa sebagai IPv4), `fc00::/7`, `fe80::/10`.
fn is_blocked_v6(ip: Ipv6Addr) -> bool {
    if ip.is_loopback() || ip.is_unspecified() {
        return true;
    }
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_blocked_v4(v4);
    }
    let first = ip.segments().first().copied().unwrap_or(0);
    (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80
}

/// `isBlockedIp(ip)` (ssrf-guard.ts:76): `true` = koneksi DITOLAK; teks yang bukan IP juga ditolak.
pub fn is_blocked_ip(ip: &str) -> bool {
    match IpAddr::from_str(ip.trim_start_matches('[').trim_end_matches(']')) {
        Ok(addr) => is_blocked_addr(addr),
        Err(_) => true,
    }
}

pub fn is_blocked_addr(addr: IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => is_blocked_v4(v4),
        IpAddr::V6(v6) => is_blocked_v6(v6),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // uom-conversion.util.spec.ts
    #[test]
    fn base_equivalent() {
        assert_eq!(
            compute_base_equivalent("multiply", "24").as_deref(),
            Some("24")
        );
        assert_eq!(
            compute_base_equivalent("multiply", "1").as_deref(),
            Some("1")
        );
        assert_eq!(
            compute_base_equivalent("multiply", "2.5").as_deref(),
            Some("2.5")
        );
        assert_eq!(
            compute_base_equivalent("divide", "1000").as_deref(),
            Some("0.001")
        );
        assert_eq!(
            compute_base_equivalent("divide", "4").as_deref(),
            Some("0.25")
        );
        assert_eq!(compute_base_equivalent("divide", "1").as_deref(), Some("1"));
        assert_eq!(
            compute_base_equivalent("divide", "3").as_deref(),
            Some("0.333333")
        );
        assert_eq!(
            compute_base_equivalent("divide", "1.5").as_deref(),
            Some("0.666667")
        );
        assert_eq!(
            compute_base_equivalent("divide", "1000000").as_deref(),
            Some("0.000001")
        );
        assert_eq!(compute_base_equivalent("divide", "3000000"), None);
        assert_eq!(compute_base_equivalent("multiply", "0"), None);
        assert_eq!(compute_base_equivalent("divide", "0"), None);
        assert_eq!(
            compute_base_equivalent("multiply", "24.000000").as_deref(),
            Some("24")
        );
    }

    // qty-range.util.spec.ts
    #[test]
    fn decimal_strings() {
        use std::cmp::Ordering::*;
        assert_eq!(compare_decimal_strings("10", "9"), Greater);
        assert_eq!(compare_decimal_strings("9", "10"), Less);
        assert_eq!(compare_decimal_strings("2", "11"), Less);
        assert_eq!(compare_decimal_strings("21", "11"), Greater);
        assert_eq!(compare_decimal_strings("1.5", "1.50"), Equal);
        assert_eq!(compare_decimal_strings("1.05", "1.5"), Less);
        assert_eq!(compare_decimal_strings("12", "12.0000"), Equal);
        assert_eq!(compare_decimal_strings("01", "1"), Equal);
        assert_eq!(compare_decimal_strings("0", "0.0001"), Less);
    }

    fn r(min: &str, max: Option<&str>) -> QtyRange {
        QtyRange {
            min: min.into(),
            max: max.map(Into::into),
        }
    }

    #[test]
    fn overlap() {
        assert!(!qty_ranges_overlap(
            &r("1", Some("10")),
            &r("11", Some("20"))
        ));
        assert!(qty_ranges_overlap(
            &r("1", Some("10")),
            &r("10", Some("20"))
        ));
        assert!(qty_ranges_overlap(
            &r("1", Some("100")),
            &r("10", Some("20"))
        ));
        assert!(qty_ranges_overlap(&r("1", None), &r("1000", Some("2000"))));
        assert!(!qty_ranges_overlap(&r("2000", None), &r("1", Some("10"))));
        assert!(qty_ranges_overlap(&r("1", None), &r("5000", None)));
    }

    // price-list-active.util.spec.ts
    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }
    fn d(s: &str) -> Option<NaiveDate> {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()
    }

    #[test]
    fn active_today() {
        const JKT: &str = "Asia/Jakarta";
        assert!(!is_price_list_active_today(
            false,
            None,
            None,
            JKT,
            at("2026-03-10T00:00:00Z")
        ));
        assert!(is_price_list_active_today(
            true,
            None,
            None,
            JKT,
            at("2026-03-10T00:00:00Z")
        ));
        let now = at("2026-03-10T20:00:00Z");
        assert!(!is_price_list_active_today(
            true,
            None,
            d("2026-03-10"),
            JKT,
            now
        ));
        assert!(is_price_list_active_today(
            true,
            None,
            d("2026-03-10"),
            "UTC",
            now
        ));
        assert!(is_price_list_active_today(
            true,
            d("2026-03-11"),
            None,
            JKT,
            now
        ));
        assert!(!is_price_list_active_today(
            true,
            d("2026-03-11"),
            None,
            "UTC",
            now
        ));
        let now = at("2026-03-15T12:00:00Z");
        assert!(!is_price_list_active_today(
            true,
            None,
            d("2026-03-10"),
            JKT,
            now
        ));
        assert!(is_price_list_active_today(
            true,
            d("2026-03-01"),
            d("2026-03-31"),
            JKT,
            now
        ));
    }

    // product-image-url.spec.ts
    #[test]
    fn image_file_id() {
        let key = "products/10/9f1c2d3e-4a5b-4c7d-8e9f-0a1b2c3d4e5f/7c2a1b30-9d44-4e21-8f77-1b2c3d4e5f60.jpg";
        assert_eq!(
            product_image_file_id(Some(key)).as_deref(),
            Some("7c2a1b30-9d44-4e21-8f77-1b2c3d4e5f60")
        );
        assert_eq!(
            product_image_path("9f1c2d3e-4a5b-4c7d-8e9f-0a1b2c3d4e5f", Some(key)).as_deref(),
            Some(
                "/products/9f1c2d3e-4a5b-4c7d-8e9f-0a1b2c3d4e5f/image/7c2a1b30-9d44-4e21-8f77-1b2c3d4e5f60"
            )
        );
        assert_eq!(product_image_file_id(None), None);
        assert_eq!(product_image_file_id(Some("")), None);
        assert_eq!(
            product_image_file_id(Some("https://storage.googleapis.com/b/x.jpg")),
            None
        );
        assert_eq!(
            product_image_file_id(Some(
                "products/1/x/7c2a1b30-9d44-4e21-8f77-1b2c3d4e5f60.gif"
            )),
            None
        );
    }

    // ssrf-guard.spec.ts
    #[test]
    fn ssrf() {
        for ip in [
            "169.254.169.254",
            "127.0.0.1",
            "127.0.0.55",
            "10.0.0.1",
            "172.16.5.4",
            "172.31.255.255",
            "192.168.1.1",
            "0.0.0.0",
            "100.64.1.1",
            "::1",
            "::",
            "fc00::1",
            "fd12:3456::1",
            "fe80::1",
            "::ffff:169.254.169.254",
            "::ffff:127.0.0.1",
            "not-an-ip",
        ] {
            assert!(is_blocked_ip(ip), "{ip} harus diblok");
        }
        for ip in [
            "8.8.8.8",
            "1.1.1.1",
            "203.0.113.10",
            "172.32.0.1",
            "172.15.255.255",
            "2606:4700:4700::1111",
        ] {
            assert!(!is_blocked_ip(ip), "{ip} harus lolos");
        }
    }
}
