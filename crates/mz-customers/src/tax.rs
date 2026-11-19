//! Port `src/customers/customer-tax-number.util.ts`.

/// `normalizeCustomerTaxNumber`: buang semua karakter kecuali digit ASCII, titik, dan tanda
/// hubung (`/[^\d.-]/g`; `\d` JS = ASCII). Tidak pernah divalidasi checksum. Hasil kosong = `None`.
pub fn normalize_customer_tax_number(value: Option<&str>) -> Option<String> {
    let cleaned: String = value?
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
        .collect();
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_everything_except_digits_dots_hyphens() {
        assert_eq!(
            normalize_customer_tax_number(Some(" NPWP: 01.234.567.8-901.000 ")).as_deref(),
            Some("01.234.567.8-901.000")
        );
        assert_eq!(
            normalize_customer_tax_number(Some("NPWP 09.876")).as_deref(),
            Some("09.876")
        );
    }

    #[test]
    fn keeps_canonical_npwp_hyphen() {
        assert_eq!(
            normalize_customer_tax_number(Some("01.234.567.8-901.000")).as_deref(),
            Some("01.234.567.8-901.000")
        );
    }

    #[test]
    fn nothing_left_is_none() {
        assert_eq!(normalize_customer_tax_number(Some("n/a")), None);
        assert_eq!(normalize_customer_tax_number(Some("")), None);
        assert_eq!(normalize_customer_tax_number(None), None);
    }

    #[test]
    fn non_ascii_digits_are_stripped() {
        // `\d` JS hanya ASCII: angka Arab-Indic dibuang.
        assert_eq!(
            normalize_customer_tax_number(Some("١٢٣1")).as_deref(),
            Some("1")
        );
    }
}
