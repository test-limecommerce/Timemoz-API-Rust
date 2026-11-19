//! Konfirmasi email pendaftar: `confirmRegister`, `resendConfirmationEmail`,
//! `checkEmailVerification` (`auth.service.ts:1591-1950`).

use mz_account::password::{hash_password, verify_password};
use mz_core::time::now_naive;
use mz_core::user_status::{STATUS_ACTIVE, STATUS_INVITED};
use mz_core::{ApiResult, AppState};
use serde_json::{Value, json};

use crate::analytics::spawn_email_verified;
use crate::service::confirm_url;
use crate::service::register::error_message;

#[derive(sqlx::FromRow)]
struct PendingUser {
    id: i32,
    uuid: String,
    status: i32,
    username: String,
    name: String,
}

const INVALID_TOKEN: &str = "Invalid or expired confirmation token";

/// `AuthService.confirmRegister(username, token)` — semua galat menjadi
/// `{ success:false, message: err.message || 'Confirmation failed' }`.
pub async fn confirm_register(
    state: &AppState,
    audit: &mz_audit::AuditHandle,
    username: &str,
    token: &str,
) -> Value {
    match confirm_register_inner(state, audit, username, token).await {
        Ok(v) => v,
        Err(err) => {
            let message = error_message(&err);
            json!({
                "success": false,
                "message": if message.is_empty() { "Confirmation failed".to_owned() } else { message },
            })
        }
    }
}

async fn confirm_register_inner(
    state: &AppState,
    audit: &mz_audit::AuditHandle,
    username: &str,
    token: &str,
) -> ApiResult<Value> {
    // `username` dipakai apa adanya (Node tidak me-lowercase di jalur ini).
    let user = sqlx::query_as::<_, PendingUser>(
        r#"SELECT id, uuid::text AS "uuid", status, username, name FROM users WHERE username = $1 ORDER BY id LIMIT 1"#,
    )
    .bind(username)
    .fetch_optional(&state.db)
    .await?;
    let Some(user) = user else {
        return Ok(json!({ "success": false, "message": "User not found" }));
    };
    if user.status == STATUS_ACTIVE {
        return Ok(json!({ "success": false, "message": "Account is already confirmed" }));
    }
    // Audit C3: link konfirmasi tidak menghidupkan akun nonaktif/terhapus.
    if user.status != STATUS_INVITED {
        return Ok(json!({ "success": false, "message": INVALID_TOKEN }));
    }
    if !verify_password(&user.uuid, token).await {
        return Ok(json!({ "success": false, "message": INVALID_TOKEN }));
    }
    // Bersyarat (bukan cek-lalu-update): dua klik bersamaan → satu pemenang.
    let user_id = user.id;
    let pre = audit
        .pre_read_many(
            mz_audit::Model::Users,
            mz_audit::Lookup::filter(move |qb| {
                qb.push("id = ").push_bind(user_id);
                qb.push(" AND status = ").push_bind(STATUS_INVITED);
            }),
        )
        .await;
    let verified_at = now_naive();
    let count = sqlx::query(
        r#"UPDATE users SET status = $2, "emailVerifiedAt" = $3 WHERE id = $1 AND status = $4"#,
    )
    .bind(user.id)
    .bind(STATUS_ACTIVE)
    .bind(verified_at)
    .bind(STATUS_INVITED)
    .execute(&state.db)
    .await?
    .rows_affected();
    audit
        .record_update_many(
            mz_audit::Model::Users,
            pre,
            count,
            &mz_audit::Row::new()
                .with("status", STATUS_ACTIVE)
                .with("emailVerifiedAt", json!(mz_core::time::JsDate(verified_at))),
            json!({ "id": user.id, "status": STATUS_INVITED }),
        )
        .await;
    if count == 0 {
        return Ok(json!({ "success": false, "message": "Account is already confirmed" }));
    }
    state.invalidate_token_revocation(user.id);
    spawn_email_verified(state, user.id, user.uuid.clone());

    let mut payload = serde_json::Map::new();
    payload.insert(
        "subject".into(),
        json!("Selamat Datang di Timemoz - Dari Tebak-tebakan Jadi Kerja Hebat"),
    );
    payload.insert("username".into(), json!(user.username));
    payload.insert("name".into(), json!(user.name));
    if let Some(v) = state.config.var("MAIL_FOUNDER_NAME") {
        payload.insert("founderName".into(), json!(v));
    }
    if let Some(v) = state.config.var("MAIL_START_GUIDE_URL") {
        payload.insert("quickStartGuideUrl".into(), json!(v));
    }
    let _ = mz_account::mail::send_personal_welcome(state, Value::Object(payload)).await;

    Ok(json!({ "success": true, "message": "Account confirmed successfully. You can now log in." }))
}

/// `AuthService.resendConfirmationEmail(username)` — jawaban SELALU sama (H3);
/// hash + kirim email di luar jalur respons.
pub async fn resend_confirmation(state: &AppState, username: &str) -> Value {
    let response = json!({
        "success": true,
        "message": "If this account is awaiting confirmation, a confirmation email has been sent.",
    });
    let user = sqlx::query_as::<_, PendingUser>(
        r#"SELECT id, uuid::text AS "uuid", status, username, name FROM users WHERE username = $1 ORDER BY id LIMIT 1"#,
    )
    .bind(username.to_lowercase())
    .fetch_optional(&state.db)
    .await;
    let user = match user {
        Ok(u) => u,
        Err(err) => {
            tracing::error!("resendConfirmationEmail lookup failed: {err}");
            return response;
        }
    };
    let Some(target) = user.filter(|u| u.status == STATUS_INVITED) else {
        return response;
    };
    let state = state.clone();
    mz_core::bg::spawn(async move {
        let hashed = match hash_password(&state, &target.uuid).await {
            Ok(h) => h,
            Err(err) => {
                tracing::error!("resendConfirmationEmail send failed: {err}");
                return;
            }
        };
        let url = confirm_url(&state, &target.username, &hashed);
        let r = mz_account::mail::send_account_confirmation(
            &state,
            &target.username,
            &target.name,
            &url,
        )
        .await;
        if !r.success {
            tracing::error!("resendConfirmationEmail send failed: {}", r.message);
        }
    });
    response
}

/// `AuthService.checkEmailVerification(username)` — email tak terdaftar dijawab
/// sama dengan akun yang belum terverifikasi (H3).
pub async fn check_email_verification(state: &AppState, username: &str) -> Value {
    let status: Result<Option<i32>, _> =
        sqlx::query_scalar("SELECT status FROM users WHERE username = $1 ORDER BY id LIMIT 1")
            .bind(username.to_lowercase())
            .fetch_optional(&state.db)
            .await;
    match status {
        Ok(status) => {
            json!({ "success": true, "data": { "isVerified": status == Some(STATUS_ACTIVE) } })
        }
        Err(err) => {
            tracing::error!("checkEmailVerification failed: {err}");
            json!({ "success": false, "message": "Failed to check verification status" })
        }
    }
}
