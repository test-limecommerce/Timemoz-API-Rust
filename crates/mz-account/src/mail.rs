//! Port fungsi `MailService` (`src/mail/mail.service.ts`) yang dipakai alur akun, sebagai
//! fungsi tipis di atas [`mz_core::mail::send`]. Semuanya menelan galat dan mengembalikan
//! `MailResult { success, message, data }` seperti Node (pemanggil biasanya mengabaikannya).
//!
//! Catatan: pesan gagal `sendPersonalWelcome` di Node ('send email personal welcome failed')
//! tidak sejalan dengan pesan suksesnya; di sini keduanya memakai label yang sama
//! ('send email confirmation'). Pesan hanya dilog, tidak pernah dikirim ke klien.

use mz_core::AppState;
use mz_core::mail::{Mail, MailResult, send};
use mz_core::time::now_naive;
use serde_json::{Value, json};

use crate::util::{encode_uri_component, env_or_undefined};

fn host_fe(state: &AppState) -> String {
    env_or_undefined(state.config.var("HOST_FE"))
}

/// `sendUserConfirmation(user, token)` — email aktivasi pendaftar. Juga menandai
/// `users.sentAt = now` (updateMany by username) sebelum mengirim.
pub async fn send_user_confirmation(
    state: &AppState,
    audit: &mz_audit::AuditHandle,
    username: &str,
    name: &str,
    token: &str,
) -> MailResult {
    let url = format!(
        "{}/confirm?username={}&token={token}&type=signup",
        host_fe(state),
        encode_uri_component(username)
    );
    // Audit `users.updateMany`: `sentAt` di luar allowlist → normalnya tanpa baris.
    let owned = username.to_owned();
    let pre = audit
        .pre_read_many(
            mz_audit::Model::Users,
            mz_audit::Lookup::filter(move |qb| {
                qb.push("username = ").push_bind(owned.clone());
            }),
        )
        .await;
    let sent_at = now_naive();
    match sqlx::query(r#"UPDATE users SET "sentAt" = $2 WHERE username = $1"#)
        .bind(username)
        .bind(sent_at)
        .execute(&state.db)
        .await
    {
        Ok(done) => {
            audit
                .record_update_many(
                    mz_audit::Model::Users,
                    pre,
                    done.rows_affected(),
                    &mz_audit::Row::new().with("sentAt", json!(mz_core::time::JsDate(sent_at))),
                    json!({ "username": username }),
                )
                .await;
        }
        Err(err) => {
            tracing::error!(error = %err, "sendUserConfirmation: gagal update sentAt");
            return MailResult {
                success: false,
                message: "send email confirmation failed".into(),
                data: None,
            };
        }
    }
    let subject = "Selamat Datang di Timemoz! Konfirmasi Email Kamu";
    send(
        state,
        Mail {
            to_email: username,
            to_name: name,
            subject,
            text: None,
            template: "confirmation",
            context: json!({ "name": name, "email": username, "url": url }),
            category: "Invite Account",
            label: "send email confirmation",
        },
    )
    .await
}

/// `sendEmailForgotPassword(user, token)` — hanya mengirim bila token (`userForgotPassword.uuid`)
/// masih berstatus 1; lalu menandainya `sentAt = now, status = 0` (bisa dipakai).
pub async fn send_email_forgot_password(
    state: &AppState,
    username: &str,
    name: &str,
    token: &str,
) -> MailResult {
    let failed = || MailResult {
        success: false,
        message: "send email forgot password failed".into(),
        data: None,
    };
    let url = format!(
        "{}/forgot-password?token={token}&username={}",
        host_fe(state),
        encode_uri_component(username)
    );

    let found: Result<Option<i32>, sqlx::Error> = sqlx::query_scalar(
        r#"SELECT id FROM "userForgotPassword" WHERE uuid = $1::uuid AND status = 1 LIMIT 1"#,
    )
    .bind(token)
    .fetch_optional(&state.db)
    .await;
    match found {
        Ok(Some(_)) => {}
        Ok(None) => return failed(),
        Err(err) => {
            tracing::error!(error = %err, "sendEmailForgotPassword: query gagal");
            return failed();
        }
    }
    if let Err(err) = sqlx::query(
        r#"UPDATE "userForgotPassword" SET "sentAt" = $2, status = 0 WHERE uuid = $1::uuid"#,
    )
    .bind(token)
    .bind(now_naive())
    .execute(&state.db)
    .await
    {
        tracing::error!(error = %err, "sendEmailForgotPassword: update gagal");
        return failed();
    }
    send(
        state,
        Mail {
            to_email: username,
            to_name: name,
            subject: "Reset Password",
            text: None,
            template: "reset-password",
            context: json!({ "name": name, "url": url }),
            category: "Reset Password",
            label: "send email forgot password",
        },
    )
    .await
}

/// `sendInviteEmail` — undangan lewat link (`POST /users/invite`).
pub async fn send_invite_email(
    state: &AppState,
    email: &str,
    name: &str,
    manager_name: &str,
    company_name: &str,
    url: &str,
) -> MailResult {
    let subject = format!("{manager_name} invited you to join {company_name} on Timemoz");
    send(
        state,
        Mail {
            to_email: email,
            to_name: name,
            subject: &subject,
            text: None,
            template: "invite",
            context: json!({ "managerName": manager_name, "companyName": company_name, "url": url }),
            category: "Invite",
            label: "send invite email",
        },
    )
    .await
}

/// `sendInviteCredentialsEmail` — undangan dengan password yang digenerate (`generatePassword`).
pub async fn send_invite_credentials_email(
    state: &AppState,
    email: &str,
    name: &str,
    manager_name: &str,
    company_name: &str,
    password: &str,
    url: &str,
) -> MailResult {
    let subject = format!("Your Timemoz account for {company_name} is ready");
    send(
        state,
        Mail {
            to_email: email,
            to_name: name,
            subject: &subject,
            text: None,
            template: "invite-credentials",
            context: json!({
                "name": name, "email": email, "managerName": manager_name,
                "companyName": company_name, "password": password, "url": url,
            }),
            category: "Invite Credentials",
            label: "send invite credentials email",
        },
    )
    .await
}

/// `sendConfirmationSuccessEmail` — setelah akun terkonfirmasi (`url` = `DOWNLOAD_URL`).
pub async fn send_confirmation_success_email(
    state: &AppState,
    email: &str,
    name: &str,
) -> MailResult {
    let subject = "Selamat Datang di Timemoz - Akun Kamu Sudah Terkonfirmasi!";
    send(
        state,
        Mail {
            to_email: email,
            to_name: name,
            subject,
            text: None,
            template: "confirmation",
            context: json!({ "email": email, "url": state.config.var("DOWNLOAD_URL") }),
            category: "Confirmation",
            label: "send confirmation success email",
        },
    )
    .await
}

/// `sendAccountConfirmation` — verifikasi email pendaftar (`HOST_FE_COMPRO/verify-email`).
pub async fn send_account_confirmation(
    state: &AppState,
    email: &str,
    name: &str,
    url: &str,
) -> MailResult {
    send(
        state,
        Mail {
            to_email: email,
            to_name: name,
            subject: "Confirm your Timemoz account",
            text: None,
            template: "account-confirmation",
            context: json!({ "url": url }),
            category: "Account Confirmation",
            label: "send account confirmation email",
        },
    )
    .await
}

/// `sendAdminRegisterInfo` — notifikasi ke `EMAIL_ADMIN` tiap ada registrasi baru.
pub async fn send_admin_register_info(
    state: &AppState,
    name: &str,
    email: &str,
    registration_date: &str,
) -> MailResult {
    let subject = "New User Registration on Timemoz";
    let admin = state.config.var("EMAIL_ADMIN").unwrap_or_default();
    send(
        state,
        Mail {
            to_email: &admin,
            to_name: "Admin",
            subject,
            text: None,
            template: "admin-register-info",
            context: json!({ "name": name, "email": email, "registrationDate": registration_date, "subject": subject }),
            category: "Admin Notification",
            label: "send admin register info email",
        },
    )
    .await
}

/// `sendPersonalWelcome(payload)` — `payload` diteruskan apa adanya sebagai konteks template
/// `personal-welcome` (`subject`, `username`, `name`, `founderName`, `quickStartGuideUrl`, ...).
pub async fn send_personal_welcome(state: &AppState, payload: Value) -> MailResult {
    let field = |k: &str| {
        payload
            .get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let (username, name, subject) = (field("username"), field("name"), field("subject"));
    send(
        state,
        Mail {
            to_email: &username,
            to_name: &name,
            subject: &subject,
            text: None,
            template: "personal-welcome",
            context: payload,
            category: "Personal Welcome",
            label: "send email confirmation",
        },
    )
    .await
}
