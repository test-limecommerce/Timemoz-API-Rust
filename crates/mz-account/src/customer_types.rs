//! Port `src/customers/default-customer-types.ts` — tipe customer bawaan company baru.

use mz_core::ApiResult;
use sqlx::PgConnection;

/// Satu tipe bawaan (juga diekspor ulang `mz_customers::default_types`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DefaultCustomerType {
    pub code: &'static str,
    pub name: &'static str,
}

pub const DEFAULT_CUSTOMER_TYPES: &[DefaultCustomerType] = &[
    DefaultCustomerType {
        code: "DISTRIBUTOR",
        name: "Distributor",
    },
    DefaultCustomerType {
        code: "WHOLESALER",
        name: "Wholesaler",
    },
    DefaultCustomerType {
        code: "RETAILER",
        name: "Retailer",
    },
    DefaultCustomerType {
        code: "END_CUSTOMER",
        name: "End Customer",
    },
    DefaultCustomerType {
        code: "PROJECT",
        name: "Project",
    },
];

/// Tipe yang dipasang otomatis kalau customer dibuat tanpa `typeId`. HARUS ada di daftar di atas.
pub const DEFAULT_CUSTOMER_TYPE_CODE: &str = "DISTRIBUTOR";

/// `defaultCustomerTypeRows(companyId, createdById)` + `customerType.createMany`.
/// Dipakai di dalam transaksi registrasi. `ON CONFLICT DO NOTHING` (unique `companyId, code`);
/// mengembalikan jumlah baris yang tertulis.
pub async fn seed_default_customer_types(
    conn: &mut PgConnection,
    company_id: i32,
    created_by_id: i32,
) -> ApiResult<u64> {
    let codes: Vec<&str> = DEFAULT_CUSTOMER_TYPES.iter().map(|t| t.code).collect();
    let names: Vec<&str> = DEFAULT_CUSTOMER_TYPES.iter().map(|t| t.name).collect();
    let res = sqlx::query(
        r#"INSERT INTO "customerType" ("companyId", code, name, "createdById")
           SELECT $1, t.code, t.name, $2 FROM UNNEST($3::text[], $4::text[]) AS t(code, name)
           ON CONFLICT DO NOTHING"#,
    )
    .bind(company_id)
    .bind(created_by_id)
    .bind(&codes)
    .bind(&names)
    .execute(&mut *conn)
    .await?;
    Ok(res.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_code_is_in_list() {
        assert!(
            DEFAULT_CUSTOMER_TYPES
                .iter()
                .any(|t| t.code == DEFAULT_CUSTOMER_TYPE_CODE)
        );
        assert_eq!(DEFAULT_CUSTOMER_TYPES.len(), 5);
    }
}
