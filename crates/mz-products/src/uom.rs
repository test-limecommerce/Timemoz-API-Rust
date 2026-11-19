//! Port `src/products/uom-conversion.util.ts` + enum `UomOperator`/`UomType`.

use std::str::FromStr;

use rust_decimal::{Decimal, RoundingStrategy};

/// `productUom.conversionRate`/`factor` = `Decimal(18,6)`.
pub const CONVERSION_SCALE: u32 = 6;

pub const UOM_OPERATORS: &[&str] = &["multiply", "divide"];
pub const UOM_TYPES: &[&str] = &["unit", "weight", "volume", "length"];

/// Node: src/products/uom-conversion.util.ts:25 `computeBaseEquivalent`.
///
/// `multiply` → factor, `divide` → 1 / factor (pembulatan half-up 6 desimal). `None` bila
/// factor ≤ 0, tidak terbaca, atau hasil pembulatan nol. Teks hasil = `Decimal.toFixed()`
/// (tanpa nol di belakang koma, tanpa notasi eksponen).
pub fn compute_base_equivalent(operator: &str, factor: &str) -> Option<String> {
    let value = Decimal::from_str(factor).ok()?;
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
        return None;
    }
    Some(result.normalize().to_string())
}

/// `new Prisma.Decimal(a).equals(b)`.
pub fn dec_eq(a: &str, b: &str) -> bool {
    match (Decimal::from_str(a), Decimal::from_str(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Node: src/products/uom-conversion.util.spec.ts
    #[test]
    fn multiply_returns_the_factor_itself() {
        assert_eq!(compute_base_equivalent("multiply", "24").as_deref(), Some("24"));
        assert_eq!(compute_base_equivalent("multiply", "1").as_deref(), Some("1"));
        assert_eq!(compute_base_equivalent("multiply", "2.5").as_deref(), Some("2.5"));
    }

    #[test]
    fn divide_returns_one_over_factor() {
        assert_eq!(compute_base_equivalent("divide", "1000").as_deref(), Some("0.001"));
        assert_eq!(compute_base_equivalent("divide", "4").as_deref(), Some("0.25"));
        assert_eq!(compute_base_equivalent("divide", "1").as_deref(), Some("1"));
    }

    #[test]
    fn divide_rounds_half_up_to_6_decimals() {
        assert_eq!(compute_base_equivalent("divide", "3").as_deref(), Some("0.333333"));
        assert_eq!(compute_base_equivalent("divide", "1.5").as_deref(), Some("0.666667"));
    }

    #[test]
    fn never_exponent_notation_for_smallest_value() {
        assert_eq!(
            compute_base_equivalent("divide", "1000000").as_deref(),
            Some("0.000001")
        );
    }

    #[test]
    fn null_when_divide_rounds_to_zero() {
        assert_eq!(compute_base_equivalent("divide", "3000000"), None);
    }

    #[test]
    fn null_for_non_positive_factor() {
        assert_eq!(compute_base_equivalent("multiply", "0"), None);
        assert_eq!(compute_base_equivalent("divide", "0"), None);
    }

    #[test]
    fn decimal_equality() {
        assert!(dec_eq("1", "1.000000"));
        assert!(!dec_eq("1", "2"));
    }
}
