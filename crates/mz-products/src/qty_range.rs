//! Port `src/products/qty-range.util.ts` — perbandingan string desimal non-negatif & overlap tier.

use std::cmp::Ordering;

/// Node: src/products/qty-range.util.ts:18 `compareDecimalStrings`. String desimal non-negatif
/// polos (tanpa tanda, tanpa eksponen).
pub fn compare_decimal_strings(a: &str, b: &str) -> Ordering {
    let (a_int, a_frac) = a.split_once('.').unwrap_or((a, ""));
    let (b_int, b_frac) = b.split_once('.').unwrap_or((b, ""));
    let norm = |s: &str| -> String {
        let t = s.trim_start_matches('0');
        if t.is_empty() && !s.is_empty() {
            "0".to_owned()
        } else {
            t.to_owned()
        }
    };
    let (ai, bi) = (norm(a_int), norm(b_int));
    if ai.len() != bi.len() {
        return ai.len().cmp(&bi.len());
    }
    if ai != bi {
        return ai.cmp(&bi);
    }
    let len = a_frac.len().max(b_frac.len());
    let pad = |s: &str| format!("{s:0<len$}");
    pad(a_frac).cmp(&pad(b_frac))
}

/// `{ minQty, maxQty }` (`None` = tak terbatas).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QtyRange {
    pub min_qty: String,
    pub max_qty: Option<String>,
}

/// Node: src/products/qty-range.util.ts:44 `qtyRangesOverlap` (batas inklusif).
pub fn qty_ranges_overlap(a: &QtyRange, b: &QtyRange) -> bool {
    let a_ok = a
        .max_qty
        .as_deref()
        .is_none_or(|mx| compare_decimal_strings(mx, &b.min_qty) != Ordering::Less);
    let b_ok = b
        .max_qty
        .as_deref()
        .is_none_or(|mx| compare_decimal_strings(mx, &a.min_qty) != Ordering::Less);
    a_ok && b_ok
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(min: &str, max: Option<&str>) -> QtyRange {
        QtyRange {
            min_qty: min.into(),
            max_qty: max.map(Into::into),
        }
    }

    // Node: src/products/qty-range.util.spec.ts
    #[test]
    fn compare_decimal_strings_cases() {
        assert_eq!(compare_decimal_strings("10", "9"), Ordering::Greater);
        assert_eq!(compare_decimal_strings("9", "10"), Ordering::Less);
        assert_eq!(compare_decimal_strings("2", "11"), Ordering::Less);
        assert_eq!(compare_decimal_strings("21", "11"), Ordering::Greater);
        assert_eq!(compare_decimal_strings("1.5", "1.50"), Ordering::Equal);
        assert_eq!(compare_decimal_strings("1.05", "1.5"), Ordering::Less);
        assert_eq!(compare_decimal_strings("12", "12.0000"), Ordering::Equal);
        assert_eq!(compare_decimal_strings("01", "1"), Ordering::Equal);
        assert_eq!(compare_decimal_strings("0", "0.0"), Ordering::Equal);
    }

    #[test]
    fn overlap_cases() {
        assert!(!qty_ranges_overlap(&r("1", Some("10")), &r("11", Some("20"))));
        assert!(qty_ranges_overlap(&r("1", Some("10")), &r("10", Some("20"))));
        assert!(qty_ranges_overlap(&r("1", Some("100")), &r("10", Some("20"))));
        assert!(qty_ranges_overlap(&r("1", None), &r("1000", Some("2000"))));
        assert!(!qty_ranges_overlap(&r("2000", None), &r("1", Some("10"))));
        assert!(qty_ranges_overlap(&r("1", None), &r("5000", None)));
    }
}
