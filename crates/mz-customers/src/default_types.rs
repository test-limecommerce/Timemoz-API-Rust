//! Port `src/customers/default-customer-types.ts` — tipe customer bawaan tiap company.
//!
//! Daftar tipe = `mz_account::customer_types` (seed registrasi; satu sumber). Di sini tambahan
//! `defaultCustomerTypeRows`; `DEFAULT_CUSTOMER_TYPE_CODE` dipakai `POST /v1/admin/customers` tanpa
//! `typeId`.

/// Daftar tipe bawaan, satu sumber di `mz_account::customer_types` (juga dipakai seed registrasi).
pub use mz_account::customer_types::{
    DEFAULT_CUSTOMER_TYPE_CODE, DEFAULT_CUSTOMER_TYPES, DefaultCustomerType,
};

/// Baris `customerType.createMany` untuk satu company (`defaultCustomerTypeRows`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultCustomerTypeRow {
    pub company_id: i32,
    pub code: &'static str,
    pub name: &'static str,
    pub created_by_id: i32,
}

/// `defaultCustomerTypeRows(companyId, createdById)`.
pub fn default_customer_type_rows(
    company_id: i32,
    created_by_id: i32,
) -> Vec<DefaultCustomerTypeRow> {
    DEFAULT_CUSTOMER_TYPES
        .iter()
        .map(|t| DefaultCustomerTypeRow {
            company_id,
            code: t.code,
            name: t.name,
            created_by_id,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dto::code_pattern_ok as master_code_ok;

    #[test]
    fn five_agreed_types_in_order() {
        let names: Vec<&str> = DEFAULT_CUSTOMER_TYPES.iter().map(|t| t.name).collect();
        assert_eq!(
            names,
            [
                "Distributor",
                "Wholesaler",
                "Retailer",
                "End Customer",
                "Project"
            ]
        );
    }

    #[test]
    fn every_code_passes_master_code_pattern() {
        for t in DEFAULT_CUSTOMER_TYPES {
            assert!(master_code_ok(t.code), "{}", t.code);
        }
    }

    #[test]
    fn codes_are_unique() {
        let mut codes: Vec<&str> = DEFAULT_CUSTOMER_TYPES.iter().map(|t| t.code).collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), DEFAULT_CUSTOMER_TYPES.len());
    }

    #[test]
    fn default_code_is_in_list_and_is_distributor() {
        assert!(
            DEFAULT_CUSTOMER_TYPES
                .iter()
                .any(|t| t.code == DEFAULT_CUSTOMER_TYPE_CODE)
        );
        assert_eq!(DEFAULT_CUSTOMER_TYPE_CODE, "DISTRIBUTOR");
    }

    #[test]
    fn rows_carry_company_and_creator() {
        let rows = default_customer_type_rows(10, 7);
        assert_eq!(rows.len(), DEFAULT_CUSTOMER_TYPES.len());
        assert!(
            rows.iter()
                .all(|r| r.company_id == 10 && r.created_by_id == 7)
        );
    }
}
