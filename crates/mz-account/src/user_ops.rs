//! Port fungsi `UsersService` (`src/users/users.service.ts`) yang dipanggil alur auth/users:
//! `confirm`, `findToken`, `forgotPassword`, `changePassword`, `sendEmailForgotPassword`,
//! `enable`, `disable`, dan `findOne` (lihat [`crate::find_one`]).
//!
//! Fungsi yang di Node mengembalikan objek ke controller mengembalikan `serde_json::Value`
//! dengan bentuk identik. Cache pencabutan token diinvalidasi SESUDAH commit.

use axum::http::StatusCode;
use mz_audit::{AuditHandle, Lookup, Model, Row};
use mz_core::enums::{BaseRole, effective_role_code};
use mz_core::jwt::revoke_all_user_tokens;
use mz_core::time::{JsDate, now_naive};
use mz_core::user_status::{RESET_TOKEN_USED, STATUS_ACTIVE, STATUS_DISABLED, STATUS_INVITED};
use mz_core::{ApiError, ApiResult, AppState};
use serde::Serialize;
use serde_json::{Value, json};

use crate::division_membership::remove_users_from_divisions_in_tx;
use crate::error_codes::{CONFIRM_LINK_INVALID, CONFIRM_WRONG_PASSWORD};
pub use crate::find_one::find_one;
use crate::invite_token::{is_invite_token_format, verify_invite_token_now};
use crate::mail;
use crate::manual_seat_limit::assert_manual_seat_available;
use crate::password::{compare_password_safely, hash_password, verify_password};
use crate::target_user::{
    SharedAccountDto, USER_ROW_COLUMNS, UserRow, assert_no_shared_account_change,
    require_company_member,
};

/// Hasil [`find_token`]: `{ id, activated }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct TokenClaim {
    pub id: i32,
    pub activated: bool,
}

/// Siapa yang otomatis dimasukkan ke SELURUH project company saat konfirmasi akun
/// (daftar putih; role custom setingkat tetap lolos lewat `baseRole`).
const PROJECT_MEMBER_ROLE_CODES: [&str; 5] =
    ["SYSTEM_ADMIN", "SUPER_ADMIN", "ADMIN", "MANAGER", "STAFF"];

fn link_invalid() -> ApiError {
    ApiError::body(
        StatusCode::NOT_FOUND,
        json!({ "code": CONFIRM_LINK_INVALID, "message": "Confirmation link is invalid." }),
    )
}

// ── confirm ──────────────────────────────────────────────────────────────────

#[derive(sqlx::FromRow)]
struct ConfirmUser {
    id: i32,
    uuid: String,
    username: String,
    name: String,
    #[sqlx(rename = "createdAt")]
    created_at: JsDate,
    status: i32,
}

#[derive(sqlx::FromRow)]
struct ConfirmMembership {
    #[sqlx(rename = "companyId")]
    company_id: i32,
    register_email: Option<String>,
    name: String,
}

#[derive(sqlx::FromRow)]
struct ConfirmRole {
    code: String,
    #[sqlx(rename = "isSystem")]
    is_system: bool,
    #[sqlx(rename = "baseRole")]
    base_role: Option<BaseRole>,
}

/// `UsersService.confirm(userId, activated)` (`users.service.ts:280`): bagian kedua
/// `POST /auth/confirm`. `activated = false` = akun sudah aktif (klik ulang), tidak ada efek samping.
pub async fn confirm(state: &AppState, user_id: i32, activated: bool) -> ApiResult<Value> {
    let db = &state.db;
    let user = sqlx::query_as::<_, ConfirmUser>(
        r#"SELECT id, uuid::text AS "uuid", username, name, "createdAt", status FROM users WHERE id = $1"#,
    )
    .bind(user_id)
    .fetch_optional(db)
    .await?;
    let Some(user) = user else {
        return Ok(json!({ "success": false, "message": "Invalid or expired token" }));
    };

    let memberships = sqlx::query_as::<_, ConfirmMembership>(
        r#"SELECT uc."companyId", c.register_email, c.name FROM "userCompany" uc
           JOIN company c ON c.id = uc."companyId" WHERE uc."userId" = $1 ORDER BY uc.id"#,
    )
    .bind(user_id)
    .fetch_all(db)
    .await?;
    let role = sqlx::query_as::<_, ConfirmRole>(
        r#"SELECT al.code, al."isSystem", al."baseRole"::text AS "baseRole"
           FROM "userAccessLevel" ual JOIN "accessLevel" al ON al.id = ual."accessLevelId"
           WHERE ual."userId" = $1 ORDER BY ual.id DESC LIMIT 1"#,
    )
    .bind(user_id)
    .fetch_optional(db)
    .await?;

    let data = json!({
        "id": user.id,
        "uuid": user.uuid,
        "username": user.username,
        "name": user.name,
        "createdAt": user.created_at,
        "status": user.status,
        "userCompany": memberships.iter().map(|m| json!({
            "companyId": m.company_id,
            "company": { "register_email": m.register_email, "name": m.name },
        })).collect::<Vec<_>>(),
        "userAccessLevel": role.iter().map(|r| json!({
            "accessLevel": { "code": r.code, "isSystem": r.is_system, "baseRole": r.base_role },
        })).collect::<Vec<_>>(),
    });

    if !activated {
        return Ok(json!({ "success": true, "message": "confirm success", "data": data }));
    }

    // Node melempar TypeError (500) bila `userAccessLevel[0]`/`userCompany[0]` tidak ada.
    let role = role
        .ok_or_else(|| ApiError::internal(anyhow::anyhow!("confirm: user without access level")))?;
    let company_id = memberships
        .first()
        .map(|m| m.company_id)
        .ok_or_else(|| ApiError::internal(anyhow::anyhow!("confirm: user without company")))?;

    if PROJECT_MEMBER_ROLE_CODES
        .contains(&effective_role_code(&role.code, role.is_system, role.base_role).as_str())
    {
        // Node: loop `projectUser.create` per `projectCompany`; satu INSERT..SELECT setara.
        sqlx::query(
            r#"INSERT INTO "projectUser" ("projectId", "userId", status)
               SELECT "projectId", $1, 1 FROM "projectCompany" WHERE "companyId" = $2"#,
        )
        .bind(user.id)
        .bind(company_id)
        .execute(db)
        .await?;
    }

    // Hasil email diabaikan (Node meng-await tapi tidak memeriksanya).
    let _ = mail::send_confirmation_success_email(state, &user.username, &user.name).await;

    Ok(json!({ "success": true, "message": "confirm success", "data": data }))
}

// ── findToken ────────────────────────────────────────────────────────────────

#[derive(sqlx::FromRow)]
struct TokenUser {
    id: i32,
    uuid: String,
    password: Option<String>,
    status: i32,
}

/// `UsersService.findToken(token, username, password)` (`users.service.ts:383`): memeriksa token dan,
/// bila sah, mengaktifkan akun. Semua kegagalan = 404 `CONFIRM_LINK_INVALID`; satu-satunya
/// pengecualian 400 `CONFIRM_WRONG_PASSWORD` (jalur aktivasi pendaftar, token sudah terbukti sah).
pub async fn find_token(
    state: &AppState,
    audit: &AuditHandle,
    token: &str,
    username: &str,
    password: &str,
) -> ApiResult<TokenClaim> {
    if token.is_empty() {
        return Err(link_invalid());
    }
    let user = sqlx::query_as::<_, TokenUser>(
        r#"SELECT id, uuid::text AS "uuid", password, status FROM users WHERE username = $1 ORDER BY id LIMIT 1"#,
    )
    .bind(username)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(link_invalid)?;

    if is_invite_token_format(token) {
        if user.status != STATUS_INVITED || !verify_invite_token_now(state, token, &user.uuid) {
            return Err(link_invalid());
        }
        // Hash di luar transaksi: bcrypt lambat.
        let hashed = hash_password(state, password).await?;
        let mut tx = state.db.begin().await?;
        // Audit `users.updateMany` (pra-baca lewat pool, `where` yang sama).
        let user_id = user.id;
        let pre = audit
            .pre_read_many(
                Model::Users,
                Lookup::filter(move |qb| {
                    qb.push("id = ").push_bind(user_id);
                    qb.push(" AND status = ").push_bind(STATUS_INVITED);
                    qb.push(r#" AND EXISTS (SELECT 1 FROM "userCompany" uc WHERE uc."userId" = "users".id AND uc.status = 1)"#);
                }),
            )
            .await;
        let claimed = sqlx::query(
            r#"UPDATE users SET password = $2, status = $3
               WHERE id = $1 AND status = $4
                 AND EXISTS (SELECT 1 FROM "userCompany" uc WHERE uc."userId" = users.id AND uc.status = 1)"#,
        )
        .bind(user.id)
        .bind(&hashed)
        .bind(STATUS_ACTIVE)
        .bind(STATUS_INVITED)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        // `password` tidak pernah masuk snapshot (§3.4), jadi tidak ikut `data`.
        audit
            .record_update_many(
                Model::Users,
                pre,
                claimed,
                &Row::new().with("status", STATUS_ACTIVE),
                json!({ "id": user.id, "status": STATUS_INVITED, "userCompany": { "some": { "status": 1 } } }),
            )
            .await;
        if claimed != 1 {
            return Err(link_invalid());
        }
        // Password berubah: token lama user ini ikut dicabut di transaksi yang sama.
        revoke_all_user_tokens_audited(&mut tx, audit, user.id).await?;
        tx.commit().await?;
        // Sesudah commit, supaya cache tidak memegang status lama hingga 60 detik.
        state.invalidate_token_revocation(user.id);
        return Ok(TokenClaim {
            id: user.id,
            activated: true,
        });
    }

    // Aktivasi pendaftar mandiri: token = bcrypt(uuid).
    if !verify_password(&user.uuid, token).await {
        return Err(link_invalid());
    }
    if !compare_password_safely(state, password, user.password.as_deref()).await {
        return Err(ApiError::body(
            StatusCode::BAD_REQUEST,
            json!({ "code": CONFIRM_WRONG_PASSWORD, "message": "Wrong password." }),
        ));
    }
    if user.status == STATUS_ACTIVE {
        return Ok(TokenClaim {
            id: user.id,
            activated: false,
        });
    }
    if user.status != STATUS_INVITED {
        return Err(link_invalid());
    }
    let user_id = user.id;
    let pre = audit
        .pre_read_many(
            Model::Users,
            Lookup::filter(move |qb| {
                qb.push("id = ").push_bind(user_id);
                qb.push(" AND status = ").push_bind(STATUS_INVITED);
            }),
        )
        .await;
    let claimed = sqlx::query("UPDATE users SET status = $2 WHERE id = $1 AND status = $3")
        .bind(user.id)
        .bind(STATUS_ACTIVE)
        .bind(STATUS_INVITED)
        .execute(&state.db)
        .await?
        .rows_affected();
    audit
        .record_update_many(
            Model::Users,
            pre,
            claimed,
            &Row::new().with("status", STATUS_ACTIVE),
            json!({ "id": user.id, "status": STATUS_INVITED }),
        )
        .await;
    if claimed != 1 {
        // Kalah balapan dengan klik lain yang baru saja mengaktifkannya.
        return Err(link_invalid());
    }
    state.invalidate_token_revocation(user.id);
    Ok(TokenClaim {
        id: user.id,
        activated: true,
    })
}

// ── forgotPassword ───────────────────────────────────────────────────────────

#[derive(sqlx::FromRow)]
struct ResetUser {
    id: i32,
    username: String,
    name: String,
    #[sqlx(rename = "createdAt")]
    created_at: JsDate,
    #[sqlx(rename = "updatedAt")]
    updated_at: Option<JsDate>,
    #[sqlx(rename = "sentAt")]
    sent_at: Option<JsDate>,
    status: i32,
    uuid: String,
}

fn reset_user_json(u: &ResetUser) -> Value {
    json!({
        "id": u.id, "username": u.username, "name": u.name, "createdAt": u.created_at,
        "updatedAt": u.updated_at, "sentAt": u.sent_at, "status": u.status, "uuid": u.uuid,
    })
}

/// `UsersService.forgotPassword({ forgotPasswordUuid, password, username })` (`users.service.ts:458`).
/// Satu respons `{success:false, message:'Invalid or expired token'}` untuk semua kegagalan
/// (kata "expired" dipertahankan: dashboard mendeteksinya). Respons HTTP tetap 201.
pub async fn forgot_password(
    state: &AppState,
    audit: &AuditHandle,
    forgot_password_uuid: &str,
    password: &str,
    username: &str,
) -> ApiResult<Value> {
    let failure = json!({ "success": false, "message": "Invalid or expired token" });
    let user_id: Option<i32> =
        sqlx::query_scalar("SELECT id FROM users WHERE username = $1 ORDER BY id LIMIT 1")
            .bind(username)
            .fetch_optional(&state.db)
            .await?;
    let Some(user_id) = user_id else {
        return Ok(failure);
    };

    // Hash dihitung SEBELUM transaksi: bcrypt lambat.
    let hashed = hash_password(state, password).await?;
    let now = now_naive();

    let mut tx = state.db.begin().await?;
    // Klaim atomik: cek status+kedaluwarsa dan penandaan "terpakai" dalam satu UPDATE (TOCTOU).
    let claim = sqlx::query(
        r#"UPDATE "userForgotPassword" SET status = $4, "updatedAt" = $3
           WHERE uuid = $1::uuid AND "userId" = $2 AND status = 0 AND "expiredAt" > $3"#,
    )
    .bind(forgot_password_uuid)
    .bind(user_id)
    .bind(now)
    .bind(RESET_TOKEN_USED)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if claim != 1 {
        tx.rollback().await?;
        return Ok(failure);
    }
    // Audit `users.update` (tanpa `select`): hanya `password` berubah → normalnya tanpa baris.
    let pre = audit.pre_read(Model::Users, Lookup::Id(user_id)).await;
    sqlx::query("UPDATE users SET password = $2 WHERE id = $1")
        .bind(user_id)
        .bind(&hashed)
        .execute(&mut *tx)
        .await?;
    if let Some(after) = audit
        .result_row(&mut *tx, Model::Users, Lookup::Id(user_id))
        .await
    {
        audit.record_update(Model::Users, pre, &after).await;
    }
    // Reset biasanya karena akun dicurigai bocor: semua token lama mati bersama password lamanya.
    revoke_all_user_tokens(&mut tx, user_id).await?;
    // Matikan tautan lama yang masih beredar di email.
    sqlx::query(
        r#"UPDATE "userForgotPassword" SET status = $3, "updatedAt" = $2 WHERE "userId" = $1 AND status IN (0, 1)"#,
    )
    .bind(user_id)
    .bind(now)
    .bind(RESET_TOKEN_USED)
    .execute(&mut *tx)
    .await?;
    let data = sqlx::query_as::<_, ResetUser>(
        r#"SELECT id, username, name, "createdAt", "updatedAt", "sentAt", status, uuid::text AS "uuid"
           FROM users WHERE id = $1"#,
    )
    .bind(user_id)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    // Sesudah commit, dan hanya bila reset benar-benar terjadi.
    state.invalidate_token_revocation(user_id);
    Ok(json!({
        "success": true,
        "message": "Reset password success",
        "data": data.as_ref().map(reset_user_json),
    }))
}

/// Baris audit dari hasil `select` [`ResetUser`] (bentuk hasil Prisma).
fn reset_user_row(u: &ResetUser) -> Row {
    Row::new()
        .with("id", u.id)
        .with("username", u.username.as_str())
        .with("name", u.name.as_str())
        .with("uuid", u.uuid.as_str())
        .with("createdAt", json!(u.created_at))
        .with("updatedAt", json!(u.updated_at))
        .with("sentAt", json!(u.sent_at))
        .with("status", u.status)
}

// ── changePassword ───────────────────────────────────────────────────────────

/// `UsersService.changePassword(userId, old, new, confirm)` (`users.service.ts:531`). Respons gagal
/// berupa `{success:false, message}` (HTTP 201), bukan galat. Pemanggil (controller auth) wajib
/// menerbitkan token pengganti karena semua token lama dicabut.
pub async fn change_password(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    old_password: &str,
    new_password: &str,
    confirm_new_password: &str,
) -> ApiResult<Value> {
    if new_password != confirm_new_password {
        return Ok(
            json!({ "success": false, "message": "New password and confirmation do not match" }),
        );
    }
    let row: Option<Option<String>> =
        sqlx::query_scalar("SELECT password FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_optional(&state.db)
            .await?;
    let Some(current) = row else {
        return Ok(json!({ "success": false, "message": "User not found" }));
    };
    let current = current.unwrap_or_default();

    if !verify_password(old_password, &current).await {
        return Ok(json!({ "success": false, "message": "Old password is incorrect" }));
    }
    if verify_password(new_password, &current).await {
        return Ok(json!({
            "success": false,
            "message": "New password must be different from the old password",
        }));
    }

    let hashed = hash_password(state, new_password).await?;
    let now = now_naive();
    let mut tx = state.db.begin().await?;
    let pre = audit.pre_read(Model::Users, Lookup::Id(user_id)).await;
    let updated = sqlx::query_as::<_, ResetUser>(
        r#"UPDATE users SET password = $2, "updatedAt" = $3 WHERE id = $1
           RETURNING id, username, name, "createdAt", "updatedAt", "sentAt", status, uuid::text AS "uuid""#,
    )
    .bind(user_id)
    .bind(&hashed)
    .bind(now)
    .fetch_one(&mut *tx)
    .await?;
    // Hasil `update` ber-`select` (id, username, name, uuid, createdAt, updatedAt, sentAt, status).
    audit
        .record_update(Model::Users, pre, &reset_user_row(&updated))
        .await;
    // Semua token lama mati bersama password lamanya.
    revoke_all_user_tokens(&mut tx, user_id).await?;
    tx.commit().await?;
    state.invalidate_token_revocation(user_id);
    Ok(
        json!({ "success": true, "message": "Change password success", "data": reset_user_json(&updated) }),
    )
}

// ── sendEmailForgotPassword ──────────────────────────────────────────────────

/// `UsersService.sendEmailForgotPassword(username)` (`users.service.ts:610`). Respons identik untuk semua
/// hasil (endpoint anonim: tidak boleh membocorkan terdaftarnya email). Email dikirim di latar
/// belakang (`mz_core::bg::spawn`, ditunggu saat shutdown) — lihat `plan/deviations/mz-account.md`.
pub async fn send_email_forgot_password(state: &AppState, username: &str) -> ApiResult<Value> {
    let response = json!({
        "success": true,
        "message": "If the email is registered, a password reset link has been sent.",
    });
    let user: Option<(i32, String)> =
        sqlx::query_as("SELECT id, name FROM users WHERE username = $1 ORDER BY id LIMIT 1")
            .bind(username)
            .fetch_optional(&state.db)
            .await?;
    let Some((user_id, name)) = user else {
        return Ok(response);
    };

    // `parseInt(process.env.EXPIRED_AT)`; NaN di Node berakhir 500 di Prisma.
    let minutes: i64 = state
        .config
        .var("EXPIRED_AT")
        .and_then(|v| v.trim().parse().ok())
        .ok_or_else(|| ApiError::internal(anyhow::anyhow!("EXPIRED_AT is not a valid integer")))?;
    let now = now_naive();
    let expired_at = now + chrono::Duration::minutes(minutes);

    // Token yang belum dikirim (status 1) dan belum kedaluwarsa: tidak membuat baru.
    let pending: Option<Option<JsDate>> = sqlx::query_scalar(
        r#"SELECT "expiredAt" FROM "userForgotPassword" WHERE "userId" = $1 AND status = $2 ORDER BY id LIMIT 1"#,
    )
    .bind(user_id)
    .bind(STATUS_ACTIVE)
    .fetch_optional(&state.db)
    .await?;
    if let Some(Some(expiry)) = pending
        && expiry.0 > now_naive()
    {
        return Ok(response);
    }

    let token: String = sqlx::query_scalar(
        r#"INSERT INTO "userForgotPassword" ("userId", "createdAt", status, "expiredAt")
           VALUES ($1, $2, $3, $4) RETURNING uuid::text"#,
    )
    .bind(user_id)
    .bind(now)
    .bind(STATUS_ACTIVE)
    .bind(expired_at)
    .fetch_one(&state.db)
    .await?;

    // Event 'forgot.password' -> listener -> MailService (fire-and-forget di Node).
    let (state, username) = (state.clone(), username.to_owned());
    mz_core::bg::spawn(async move {
        let _ = mail::send_email_forgot_password(&state, &username, &name, &token).await;
    });
    Ok(response)
}

/// `revokeAllUserTokens(tx, userId)` + baris audit `users.update` yang ditulis middleware Node untuk
/// `tx.users.update({ tokensValidAfter })`-nya: pra-baca lewat pool (belum melihat tulisan transaksi
/// ini) vs baris sesudah tulisan; hanya `status`/`username`/`name` (allowlist) yang bisa berselisih.
pub async fn revoke_all_user_tokens_audited(
    tx: &mut sqlx::PgConnection,
    audit: &AuditHandle,
    user_id: i32,
) -> ApiResult<()> {
    let pre = audit.pre_read(Model::Users, Lookup::Id(user_id)).await;
    revoke_all_user_tokens(&mut *tx, user_id).await?;
    if let Some(after) = audit
        .result_row(&mut *tx, Model::Users, Lookup::Id(user_id))
        .await
    {
        audit.record_update(Model::Users, pre, &after).await;
    }
    Ok(())
}

// ── enable / disable ─────────────────────────────────────────────────────────

/// `UsersService.enable(uuid, requesterId)` (`users.service.ts:2171`) -> `{success, message:'enable users success', data}`.
/// `data` = baris `users` tanpa kolom rahasia (lihat deviasi).
pub async fn enable(
    state: &AppState,
    audit: &AuditHandle,
    uuid: &str,
    requester_id: i32,
) -> ApiResult<Value> {
    let member = require_company_member(&state.db, requester_id, uuid).await?;
    assert_no_shared_account_change(
        &state.db,
        requester_id,
        &member.user,
        member.company_id,
        &SharedAccountDto {
            status: Some(STATUS_ACTIVE),
            ..Default::default()
        },
    )
    .await?;

    // R-18: cek kursi + tulis dalam satu transaksi di bawah lock kursi (TOCTOU).
    let mut tx = state.db.begin().await?;
    assert_manual_seat_available(&mut tx, member.company_id, 1, Some(member.user.id)).await?;
    let row = update_status(&mut tx, audit, uuid, STATUS_ACTIVE).await?;
    tx.commit().await?;
    // Tanpa ini user yang diaktifkan lagi tetap 401 hingga 60 detik.
    state.invalidate_token_revocation(row.id);
    Ok(json!({ "success": true, "message": "enable users success", "data": row }))
}

/// `UsersService.disable(uuid, requesterId)` (`users.service.ts:2210`) -> `{success, message:'disable users success', data}`.
/// Mencabut semua token dan mengeluarkan user dari semua divisi.
pub async fn disable(
    state: &AppState,
    audit: &AuditHandle,
    uuid: &str,
    requester_id: i32,
) -> ApiResult<Value> {
    let member = require_company_member(&state.db, requester_id, uuid).await?;
    assert_no_shared_account_change(
        &state.db,
        requester_id,
        &member.user,
        member.company_id,
        &SharedAccountDto {
            status: Some(STATUS_DISABLED),
            ..Default::default()
        },
    )
    .await?;

    let mut tx = state.db.begin().await?;
    let row = update_status(&mut tx, audit, uuid, STATUS_DISABLED).await?;
    revoke_all_user_tokens_audited(&mut tx, audit, row.id).await?;
    // specs/006 R11: keluar dari divisi saat tidak aktif (enable TIDAK memulihkan).
    remove_users_from_divisions_in_tx(&mut tx, audit, &[row.id], None).await?;
    tx.commit().await?;
    state.invalidate_token_revocation(row.id);
    Ok(json!({ "success": true, "message": "disable users success", "data": row }))
}

/// `tx.users.update({ where: { uuid }, data: { status } })` + audit-nya (hasil tanpa `select`).
async fn update_status(
    conn: &mut sqlx::PgConnection,
    audit: &AuditHandle,
    uuid: &str,
    status: i32,
) -> ApiResult<UserRow> {
    let pre = audit
        .pre_read(Model::Users, Lookup::Uuid(uuid.to_owned()))
        .await;
    let sql =
        format!("UPDATE users SET status = $2 WHERE uuid = $1::uuid RETURNING {USER_ROW_COLUMNS}");
    let row = sqlx::query_as::<_, UserRow>(&sql)
        .bind(uuid)
        .bind(status)
        .fetch_one(&mut *conn)
        .await?;
    if let Some(after) = audit
        .result_row(&mut *conn, Model::Users, Lookup::Id(row.id))
        .await
    {
        audit.record_update(Model::Users, pre, &after).await;
    }
    Ok(row)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_invalid_body_matches_node() {
        let ApiError::Body { status, body } = link_invalid() else {
            return assert_eq!(0, 1);
        };
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(
            body,
            json!({"code":"CONFIRM_LINK_INVALID","message":"Confirmation link is invalid."})
        );
    }

    #[test]
    fn project_member_codes_exclude_guest() {
        assert!(!PROJECT_MEMBER_ROLE_CODES.contains(&"GUEST"));
        assert!(PROJECT_MEMBER_ROLE_CODES.contains(&"STAFF"));
    }
}
