//! `companies/invite-code.ts` + `AuthService.getInviteInfo` / `registerViaInvite`.

use axum::http::StatusCode;
use chrono::Utc;
use mz_account::assignable_role::{
    ACCESS_LEVEL_COLUMNS, AccessLevel, redirect_to_company_override,
};
use mz_account::error_codes::{ALREADY_MEMBER, COMPANY_FULL, EMAIL_TAKEN, INVITE_INVALID};
use mz_account::invite_method;
use mz_account::manual_seat_limit::assert_manual_seat_available;
use mz_account::new_employee_default::apply_new_employee_manual_time_default;
use mz_account::password::hash_password;
use mz_account::signup_tracking::{
    JOIN_LINK, SignUpEventInput, build_sign_up_properties, product_type_from_modules,
};
use mz_account::username_conflict::is_username_conflict;
use mz_core::user_status::STATUS_INVITED;
use mz_core::{ApiError, ApiResult, AppState};
use rand::Rng;
use serde_json::{Value, json};

use crate::analytics::emit_sign_up;
use crate::dto::{RegisterInviteDto, text};
use crate::js;
use crate::service::confirm_url;

/// 55 karakter tanpa yang mirip (I, O, l, 0, 1). Sama persis dengan Node.
pub const INVITE_CODE_ALPHABET: &str = "ABCDEFGHJKLMNPQRSTUVWXYZabcdefghjkmnpqrstuvwxyz23456789";
pub const INVITE_CODE_LENGTH: usize = 10;

/// `generateInviteCode` — CSPRNG, tanpa modulo bias (`random_range`).
pub fn generate_invite_code() -> String {
    let alphabet = INVITE_CODE_ALPHABET.as_bytes();
    let mut rng = rand::rng();
    (0..INVITE_CODE_LENGTH)
        .filter_map(|_| {
            alphabet
                .get(rng.random_range(0..alphabet.len()))
                .map(|b| char::from(*b))
        })
        .collect()
}

/// `AuthService.generateUniqueInviteCode`: ulangi sampai kode belum dipakai company mana pun.
pub async fn generate_unique_invite_code(state: &AppState) -> ApiResult<String> {
    loop {
        let code = generate_invite_code();
        let exists: Option<i32> =
            sqlx::query_scalar(r#"SELECT id FROM company WHERE "inviteCode" = $1"#)
                .bind(&code)
                .fetch_optional(&state.db)
                .await?;
        if exists.is_none() {
            return Ok(code);
        }
    }
}

fn invite_invalid() -> ApiError {
    ApiError::coded(
        StatusCode::NOT_FOUND,
        INVITE_INVALID,
        "Invite link tidak valid atau sudah tidak aktif.",
    )
}

fn email_taken() -> ApiError {
    ApiError::coded(
        StatusCode::CONFLICT,
        EMAIL_TAKEN,
        "Email sudah terdaftar. Silakan login.",
    )
}

#[derive(sqlx::FromRow)]
struct InviteCompany {
    id: i32,
    uuid: String,
    name: String,
    #[sqlx(rename = "maxUsers")]
    max_users: i32,
    #[sqlx(rename = "moduleTimeTrackingEnabled")]
    module_time_tracking_enabled: bool,
    #[sqlx(rename = "moduleSalesTrackerEnabled")]
    module_sales_tracker_enabled: bool,
}

async fn find_company(state: &AppState, invite_code: &str) -> ApiResult<InviteCompany> {
    sqlx::query_as::<_, InviteCompany>(
        r#"SELECT id, uuid::text AS "uuid", name, "maxUsers", "moduleTimeTrackingEnabled", "moduleSalesTrackerEnabled"
           FROM company WHERE "inviteCode" = $1 AND status = 1 ORDER BY id LIMIT 1"#,
    )
    .bind(invite_code)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(invite_invalid)
}

/// `isCompanyFull`: `maxUsers <= 0` = tanpa batas.
async fn is_company_full(state: &AppState, company_id: i32, max_users: i32) -> ApiResult<bool> {
    if max_users <= 0 {
        return Ok(false);
    }
    let count: i64 = sqlx::query_scalar(
        r#"SELECT COUNT(*) FROM "userCompany" WHERE "companyId" = $1 AND status = 1"#,
    )
    .bind(company_id)
    .fetch_one(&state.db)
    .await?;
    Ok(count >= i64::from(max_users))
}

/// `AuthService.getInviteInfo(inviteCode)`.
pub async fn get_invite_info(state: &AppState, invite_code: &str) -> ApiResult<Value> {
    let company = find_company(state, invite_code).await?;
    let is_full = is_company_full(state, company.id, company.max_users).await?;
    // Logo company ada di bucket private: field tetap ada (null) untuk dashboard lama.
    Ok(json!({ "companyName": company.name, "companyLogo": null, "isFull": is_full }))
}

/// `AuthService.registerViaInvite(dto)`.
pub async fn register_via_invite(
    state: &AppState,
    audit: &mz_audit::AuditHandle,
    dto: &RegisterInviteDto,
) -> ApiResult<Value> {
    let company = find_company(state, text(&dto.invite_code)).await?;
    let username = text(&dto.username).to_lowercase();
    let name = text(&dto.name);

    let existing: Option<i32> = sqlx::query_scalar("SELECT id FROM users WHERE username = $1")
        .bind(&username)
        .fetch_optional(&state.db)
        .await?;
    if existing.is_some() {
        return Err(email_taken());
    }
    if is_company_full(state, company.id, company.max_users).await? {
        return Err(ApiError::coded(
            StatusCode::FORBIDDEN,
            COMPANY_FULL,
            "Company sudah mencapai batas maksimal pengguna.",
        ));
    }
    let member: Option<i32> = sqlx::query_scalar(
        r#"SELECT uc.id FROM "userCompany" uc JOIN users u ON u.id = uc."userId"
           WHERE uc."companyId" = $1 AND u.username = $2 LIMIT 1"#,
    )
    .bind(company.id)
    .bind(&username)
    .fetch_optional(&state.db)
    .await?;
    if member.is_some() {
        return Err(ApiError::coded(
            StatusCode::CONFLICT,
            ALREADY_MEMBER,
            "User sudah terdaftar di company ini.",
        ));
    }

    // Role tetap STAFF bawaan (`isSystem: true`), bukan pilihan pemanggil.
    let sql = format!(
        r#"SELECT {ACCESS_LEVEL_COLUMNS} FROM "accessLevel"
           WHERE code = 'STAFF' AND "isSystem" = true AND status = 1 ORDER BY id LIMIT 1"#
    );
    let global_staff = sqlx::query_as::<_, AccessLevel>(&sql)
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(|| {
            ApiError::message(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Konfigurasi role tidak ditemukan.",
            )
        })?;
    let mut conn = state.db.acquire().await?;
    let access_level = redirect_to_company_override(&mut conn, global_staff, company.id).await?;
    drop(conn);

    let default_project: Option<i32> = sqlx::query_scalar(
        r#"SELECT "projectId" FROM "projectCompany" WHERE "companyId" = $1 AND status = 1 ORDER BY id LIMIT 1"#,
    )
    .bind(company.id)
    .fetch_optional(&state.db)
    .await?;

    let hashed = hash_password(state, text(&dto.password)).await?;
    let idle = js::parse_int(state.config.var("IDLETIME_IN_SECOND").as_deref())
        .and_then(|v| i32::try_from(v).ok())
        .filter(|v| *v != 0)
        .unwrap_or(300);

    let created = async {
        let mut tx = state.db.begin().await?;
        // R-18: batas kursi company MANUAL, di bawah lock kursi di tx yang sama.
        assert_manual_seat_available(&mut tx, company.id, 1, None).await?;
        let user: (i32, String, String, String) = sqlx::query_as(
            r#"INSERT INTO users (username, name, password, status) VALUES ($1, $2, $3, $4)
               RETURNING id, uuid::text, username, name"#,
        )
        .bind(&username)
        .bind(name)
        .bind(&hashed)
        .bind(STATUS_INVITED)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| if is_username_conflict(&e) { email_taken() } else { ApiError::from(e) })?;
        // Audit `users.create` tanpa `select` (semua kolom).
        if let Some(row) = audit
            .result_row(&mut *tx, mz_audit::Model::Users, mz_audit::Lookup::Id(user.0))
            .await
        {
            audit.record_create(mz_audit::Model::Users, &row).await;
        }
        let membership_id: i32 = sqlx::query_scalar(
            r#"INSERT INTO "userCompany" ("userId", "companyId", status, "isDefault", "idleTimeInSecond", "invitedById", "inviteMethod")
               VALUES ($1, $2, 1, true, $3, NULL, $4) RETURNING id"#,
        )
        .bind(user.0)
        .bind(company.id)
        .bind(idle)
        .bind(invite_method::LINK)
        .fetch_one(&mut *tx)
        .await?;
        if let Some(row) = audit
            .result_row(&mut *tx, mz_audit::Model::UserCompany, mz_audit::Lookup::Id(membership_id))
            .await
        {
            audit.record_create(mz_audit::Model::UserCompany, &row).await;
        }
        apply_new_employee_manual_time_default(&mut tx, audit, company.id, user.0, None).await?;
        sqlx::query(
            r#"INSERT INTO "userAccessLevel" ("userId", "accessLevelId", "companyId", status) VALUES ($1, $2, $3, 1)"#,
        )
        .bind(user.0)
        .bind(access_level.id)
        .bind(company.id)
        .execute(&mut *tx)
        .await?;
        audit
            .record_create(
                mz_audit::Model::UserAccessLevel,
                &crate::service::register::user_access_level_row(user.0, access_level.id, company.id, 1),
            )
            .await;
        if let Some(project_id) = default_project {
            sqlx::query(r#"INSERT INTO "projectUser" ("userId", "projectId", status) VALUES ($1, $2, 1)"#)
                .bind(user.0)
                .bind(project_id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok::<_, ApiError>(user)
    }
    .await?;
    let (_, uuid, username, name) = created;

    let joined_at = Utc::now();
    emit_sign_up(
        state,
        &uuid,
        build_sign_up_properties(&SignUpEventInput {
            signup_type: JOIN_LINK,
            signup_method: "email",
            signup_platform: "web".into(),
            product_type: product_type_from_modules(
                company.module_time_tracking_enabled,
                company.module_sales_tracker_enabled,
            ),
            role: "STAFF".into(),
            company_uuid: company.uuid.clone(),
            is_workspace_owner: false,
            plan: None,
            invited_by_uuid: None,
            attribution: None,
            occurred_at: joined_at,
        }),
        joined_at,
    );

    let hashed_uuid = hash_password(state, &uuid).await?;
    let url = confirm_url(state, &username, &hashed_uuid);
    let _ = mz_account::mail::send_account_confirmation(state, &username, &name, &url).await;

    Ok(json!({
        "success": true,
        "message": "Registrasi berhasil. Silakan cek email untuk konfirmasi akun.",
        "data": { "uuid": uuid, "username": username, "name": name, "companyName": company.name },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invite_code_shape() {
        for _ in 0..200 {
            let code = generate_invite_code();
            assert_eq!(code.len(), INVITE_CODE_LENGTH);
            assert!(code.chars().all(|c| INVITE_CODE_ALPHABET.contains(c)));
            assert!(!code.contains(['I', 'O', 'l', '0', '1']));
        }
        assert_eq!(INVITE_CODE_ALPHABET.len(), 55);
    }

    #[test]
    fn invite_errors_match_node() {
        let ApiError::Body { status, body } = invite_invalid() else {
            return assert_eq!(0, 1);
        };
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(
            body,
            json!({"code":"INVITE_INVALID","message":"Invite link tidak valid atau sudah tidak aktif."})
        );
        let ApiError::Body { status, body } = email_taken() else {
            return assert_eq!(0, 1);
        };
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(
            body,
            json!({"code":"EMAIL_TAKEN","message":"Email sudah terdaftar. Silakan login."})
        );
    }
}
