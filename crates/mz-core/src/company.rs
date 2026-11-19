//! `src/company-context/resolve-company-id.ts` — SATU aturan "company mana yang
//! berlaku untuk user ini": keanggotaan aktif yang ditandai default; kalau tidak
//! ada yang ditandai, yang tertua.
//!
//! JANGAN menulis query `"userCompany"` berkunci `userId` di tempat lain —
//! panggil fungsi di sini (alasan lengkap ada di berkas Node-nya).

use sqlx::PgExecutor;

use crate::{ApiError, ApiResult};

/// Potongan SQL `WHERE`/`ORDER BY` untuk dipakai query lain yang butuh kolom
/// tambahan. Alias tabel wajib `uc`, parameter user = `$1`.
pub const USER_COMPANY_SCOPE_SQL: &str =
    r#"uc."userId" = $1 AND uc.status = 1 ORDER BY uc."isDefault" DESC, uc.id ASC"#;

/// `null` kalau user tidak punya keanggotaan aktif.
pub async fn find_company_id<'e>(
    db: impl PgExecutor<'e>,
    user_id: i32,
) -> sqlx::Result<Option<i32>> {
    sqlx::query_scalar(
        r#"SELECT uc."companyId" FROM "userCompany" uc
           WHERE uc."userId" = $1 AND uc.status = 1
           ORDER BY uc."isDefault" DESC, uc.id ASC
           LIMIT 1"#,
    )
    .bind(user_id)
    .fetch_optional(db)
    .await
}

/// Sama, tapi `403 'No company found for user'` (bunyi pesan lama).
pub async fn require_company_id<'e>(db: impl PgExecutor<'e>, user_id: i32) -> ApiResult<i32> {
    find_company_id(db, user_id)
        .await?
        .ok_or_else(|| ApiError::forbidden("No company found for user"))
}

/// Aturan yang sama, dipersempit ke kandidat tertentu.
pub async fn find_company_id_among<'e>(
    db: impl PgExecutor<'e>,
    user_id: i32,
    company_ids: &[i32],
) -> sqlx::Result<Option<i32>> {
    if company_ids.is_empty() {
        return Ok(None);
    }
    sqlx::query_scalar(
        r#"SELECT uc."companyId" FROM "userCompany" uc
           WHERE uc."userId" = $1 AND uc.status = 1 AND uc."companyId" = ANY($2)
           ORDER BY uc."isDefault" DESC, uc.id ASC
           LIMIT 1"#,
    )
    .bind(user_id)
    .bind(company_ids)
    .fetch_optional(db)
    .await
}

/// Kode error `403` yang dibaca dashboard (`TARGET_SHARED_ACCOUNT`).
pub const TARGET_SHARED_ACCOUNT: &str = "TARGET_SHARED_ACCOUNT";

/// Bagian bersama `assertNoSharedAccountChange` (`users/resolve-target-user.ts`): akun
/// (`username`/`name`/`status` global) yang juga anggota aktif company LAIN tidak boleh
/// diubah dari company ini → 403 `TARGET_SHARED_ACCOUNT`. Dipakai `mz_account::target_user`
/// dan restore audit (mz-audit tidak bisa bergantung pada mz-account: siklus).
pub async fn reject_if_shared_account<'e>(
    db: impl PgExecutor<'e>,
    user_id: i32,
    company_id: i32,
) -> ApiResult<()> {
    let other: Option<i32> = sqlx::query_scalar(
        r#"SELECT id FROM "userCompany" WHERE "userId" = $1 AND "companyId" <> $2 AND status = 1 LIMIT 1"#,
    )
    .bind(user_id)
    .bind(company_id)
    .fetch_optional(db)
    .await?;
    match other {
        None => Ok(()),
        Some(_) => Err(ApiError::coded(
            http::StatusCode::FORBIDDEN,
            TARGET_SHARED_ACCOUNT,
            "This account also belongs to another company; its login, name and status can only be changed by the account owner.",
        )),
    }
}
