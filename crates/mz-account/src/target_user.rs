//! Port `src/users/resolve-target-user.ts` — SATU aturan "`:uuid` di URL ini boleh disentuh
//! pelaku atau tidak" (Aturan A). Company diambil dari PELAKU; target di luar company = 404
//! `User not found` (bukan 403).

use mz_core::company::find_company_id;
use mz_core::time::JsDate;
use mz_core::{ApiError, ApiResult};
use serde::Serialize;
use sqlx::PgPool;

/// Kode error `403` yang dibaca dashboard.
pub use mz_core::company::TARGET_SHARED_ACCOUNT;

/// Baris `users` TANPA kolom rahasia (`password`, token Google Calendar). Bentuk JSON =
/// kolom skalar Prisma yang tersisa.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct UserRow {
    pub id: i32,
    pub uuid: String,
    pub username: String,
    pub name: String,
    #[sqlx(rename = "createdAt")]
    pub created_at: JsDate,
    #[sqlx(rename = "updatedAt")]
    pub updated_at: Option<JsDate>,
    #[sqlx(rename = "sentAt")]
    pub sent_at: Option<JsDate>,
    pub status: i32,
    #[sqlx(rename = "tokensValidAfter")]
    pub tokens_valid_after: Option<JsDate>,
    #[sqlx(rename = "googleId")]
    pub google_id: Option<String>,
    #[sqlx(rename = "googleCalendarTokenExpiresAt")]
    pub google_calendar_token_expires_at: Option<JsDate>,
    #[sqlx(rename = "googleCalendarEmail")]
    pub google_calendar_email: Option<String>,
    #[sqlx(rename = "emailVerifiedAt")]
    pub email_verified_at: Option<JsDate>,
    #[sqlx(rename = "gaClientId")]
    pub ga_client_id: Option<String>,
    #[sqlx(rename = "productTourStatus")]
    pub product_tour_status: Option<String>,
    #[sqlx(rename = "productTourStep")]
    pub product_tour_step: Option<String>,
    #[sqlx(rename = "productTourUpdatedAt")]
    pub product_tour_updated_at: Option<JsDate>,
    #[sqlx(rename = "salesOnboardingEmailEligible")]
    pub sales_onboarding_email_eligible: bool,
    #[sqlx(rename = "salesOnboardingEmailSentAt")]
    pub sales_onboarding_email_sent_at: Option<JsDate>,
}

/// Daftar kolom [`UserRow`] (tanpa alias tabel) — untuk `SELECT`/`RETURNING`.
pub const USER_ROW_COLUMNS: &str = r#"id, uuid::text AS "uuid", username, name, "createdAt", "updatedAt", "sentAt",
    status, "tokensValidAfter", "googleId", "googleCalendarTokenExpiresAt", "googleCalendarEmail",
    "emailVerifiedAt", "gaClientId", "productTourStatus", "productTourStep", "productTourUpdatedAt",
    "salesOnboardingEmailEligible", "salesOnboardingEmailSentAt""#;

pub struct CompanyMember {
    /// Baris `users` target.
    pub user: UserRow,
    /// Company PELAKU (= company target, karena itu syaratnya).
    pub company_id: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompanyMemberMiss {
    /// Pelaku tidak punya keanggotaan aktif.
    NoCompany,
    /// Target di luar company pelaku, atau tidak ada.
    NotMember,
}

/// `findCompanyMember(prisma, actorId, targetUuid)` — tidak melempar. `Ok(Err(miss))` = tidak ketemu.
pub async fn find_company_member(
    db: &PgPool,
    actor_id: i32,
    target_uuid: &str,
) -> ApiResult<Result<CompanyMember, CompanyMemberMiss>> {
    let Some(company_id) = find_company_id(db, actor_id).await? else {
        return Ok(Err(CompanyMemberMiss::NoCompany));
    };
    let sql = format!(
        r#"SELECT {USER_ROW_COLUMNS} FROM users u
           WHERE uuid = $1::uuid
             AND EXISTS (SELECT 1 FROM "userCompany" uc
                         WHERE uc."userId" = u.id AND uc."companyId" = $2 AND uc.status = 1)
           LIMIT 1"#
    );
    let user = sqlx::query_as::<_, UserRow>(&sql)
        .bind(target_uuid)
        .bind(company_id)
        .fetch_optional(db)
        .await?;
    Ok(user
        .map(|user| CompanyMember { user, company_id })
        .ok_or(CompanyMemberMiss::NotMember))
}

/// `requireCompanyMember` — 403 `No company found for user` / 404 `User not found`.
/// `status` target sengaja TIDAK disaring (`enable` bekerja pada user nonaktif).
pub async fn require_company_member(
    db: &PgPool,
    actor_id: i32,
    target_uuid: &str,
) -> ApiResult<CompanyMember> {
    match find_company_member(db, actor_id, target_uuid).await? {
        Ok(member) => Ok(member),
        Err(CompanyMemberMiss::NoCompany) => Err(ApiError::forbidden("No company found for user")),
        Err(CompanyMemberMiss::NotMember) => Err(ApiError::not_found("User not found")),
    }
}

/// Perubahan yang diminta; `None` = field tidak dikirim = tidak berubah.
#[derive(Debug, Clone, Copy, Default)]
pub struct SharedAccountDto<'a> {
    pub username: Option<&'a str>,
    pub name: Option<&'a str>,
    pub status: Option<i32>,
}

/// Murni: apakah dto benar-benar mengubah nilai akun target (`!==`).
pub fn changes_shared_account(target: &UserRow, dto: &SharedAccountDto<'_>) -> bool {
    dto.username.is_some_and(|v| v != target.username)
        || dto.name.is_some_and(|v| v != target.name)
        || dto.status.is_some_and(|v| v != target.status)
}

/// `assertNoSharedAccountChange`: `username`/`name`/`status` milik AKUN, bukan company. Perubahan
/// ditolak 403 `TARGET_SHARED_ACCOUNT` bila target juga anggota aktif company lain. Mengubah akun
/// sendiri boleh.
pub async fn assert_no_shared_account_change(
    db: &PgPool,
    actor_user_id: i32,
    target: &UserRow,
    company_id: i32,
    dto: &SharedAccountDto<'_>,
) -> ApiResult<()> {
    if actor_user_id == target.id || !changes_shared_account(target, dto) {
        return Ok(());
    }
    mz_core::company::reject_if_shared_account(db, target.id, company_id).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> UserRow {
        UserRow {
            id: 5,
            uuid: "u".into(),
            username: "a@b.c".into(),
            name: "A".into(),
            created_at: JsDate::now(),
            updated_at: None,
            sent_at: None,
            status: 1,
            tokens_valid_after: None,
            google_id: None,
            google_calendar_token_expires_at: None,
            google_calendar_email: None,
            email_verified_at: None,
            ga_client_id: None,
            product_tour_status: Some("NOT_STARTED".into()),
            product_tour_step: None,
            product_tour_updated_at: None,
            sales_onboarding_email_eligible: false,
            sales_onboarding_email_sent_at: None,
        }
    }

    #[test]
    fn serializes_all_prisma_columns_except_secrets() {
        let v = serde_json::to_value(target()).unwrap_or_default();
        for k in [
            "emailVerifiedAt",
            "gaClientId",
            "productTourStatus",
            "productTourStep",
            "productTourUpdatedAt",
            "salesOnboardingEmailEligible",
            "salesOnboardingEmailSentAt",
        ] {
            assert!(v.get(k).is_some(), "kolom {k} hilang");
        }
        assert!(v.get("password").is_none());
    }

    #[test]
    fn only_real_changes_count() {
        let t = target();
        assert!(!changes_shared_account(&t, &SharedAccountDto::default()));
        assert!(!changes_shared_account(
            &t,
            &SharedAccountDto {
                username: Some("a@b.c"),
                name: Some("A"),
                status: Some(1)
            }
        ));
        assert!(changes_shared_account(
            &t,
            &SharedAccountDto {
                status: Some(3),
                ..Default::default()
            }
        ));
        assert!(changes_shared_account(
            &t,
            &SharedAccountDto {
                username: Some("x@y.z"),
                ..Default::default()
            }
        ));
    }
}
