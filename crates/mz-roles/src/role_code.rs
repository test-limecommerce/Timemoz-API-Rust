//! Port `src/permissions/role-code.ts`: `code` role custom = `c{companyId}_{slug}`,
//! digenerate server-side (§0.11, §9.13). Pemanggil WAJIB sudah mengunci baris company (§9.8).

use axum::http::StatusCode;
use sqlx::PgConnection;

use crate::error::{RolesError, RolesResult};

const MAX_CODE_ATTEMPTS: usize = 200;
const CODE_MAX_LENGTH: usize = 255;
/// `` `_${MAX_CODE_ATTEMPTS}`.length ``.
const LONGEST_SUFFIX: usize = 4;

/// `slugify(input, maxLength)`: `toLowerCase` → `[^a-z0-9]+` jadi `_` → potong → buang `_` tepi.
pub fn slugify(input: &str, max_length: usize) -> String {
    let lower = input.to_lowercase();
    let mut out = String::with_capacity(lower.len());
    let mut in_run = false;
    for c in lower.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
            in_run = false;
        } else if !in_run {
            out.push('_');
            in_run = true;
        }
    }
    // Hasilnya ASCII, jadi potong per byte = per unit UTF-16 (`.slice`).
    out.truncate(max_length);
    let slug = out.trim_matches('_');
    if slug.is_empty() {
        "role".to_owned()
    } else {
        slug.to_owned()
    }
}

/// Kandidat dasar `c{companyId}_{slug}`.
pub fn base_code(company_id: i32, name: &str) -> String {
    let prefix = format!("c{company_id}_");
    let max = CODE_MAX_LENGTH.saturating_sub(prefix.len() + LONGEST_SUFFIX);
    format!("{prefix}{}", slugify(name, max))
}

/// `generateRoleCode(tx, companyId, name)` — check-then-generate di dalam transaksi pemanggil.
pub async fn generate_role_code(
    conn: &mut PgConnection,
    company_id: i32,
    name: &str,
) -> RolesResult<String> {
    let base = base_code(company_id, name);
    let mut candidate = base.clone();
    for suffix in 2..=MAX_CODE_ATTEMPTS + 1 {
        let existing: Option<i32> = sqlx::query_scalar(
            r#"SELECT id FROM "accessLevel" WHERE "companyId" = $1 AND code = $2 AND status = 1 LIMIT 1"#,
        )
        .bind(company_id)
        .bind(&candidate)
        .fetch_optional(&mut *conn)
        .await?;
        if existing.is_none() {
            return Ok(candidate);
        }
        candidate = format!("{base}_{suffix}");
    }
    Err(RolesError::coded(
        StatusCode::UNPROCESSABLE_ENTITY,
        "ROLE_CODE_EXHAUSTED",
        format!("Too many roles named like '{name}' in this company. Pick a different name."),
    ))
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// `role-code.spec.ts`.
    #[test]
    fn slug_rules() {
        assert_eq!(slugify("Supervisor Sales", 100), "supervisor_sales");
        assert_eq!(slugify("  --Hello__World!! ", 100), "hello_world");
        assert_eq!(slugify("!!!", 100), "role");
        assert_eq!(
            slugify("Kepala Cabang (Jakarta)", 100),
            "kepala_cabang_jakarta"
        );
        assert_eq!(slugify("Ünïcode Ä", 100), "n_code");
        assert_eq!(base_code(12, "Spv Sales"), "c12_spv_sales");
    }

    #[test]
    fn long_names_fit_varchar_255() {
        let name = "a".repeat(255);
        let base = base_code(123_456, &name);
        assert_eq!(base.len() + LONGEST_SUFFIX, CODE_MAX_LENGTH);
        // Potong di tengah run `_` lalu buang `_` di tepi.
        assert_eq!(slugify("ab cd", 3), "ab");
    }
}
