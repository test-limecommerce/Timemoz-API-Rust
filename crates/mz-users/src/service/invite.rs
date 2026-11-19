//! `UsersService.invite` (`users.service.ts:1839`) + `emitInvitedSignUp` (:1795) dan loop
//! `inviteBatch` (`users.controller.ts:287`).

use mz_account::assignable_role::{
    AssignableRoleActor, RoleRef, require_actor_role, resolve_assignable_role,
};
use mz_account::invite_method;
use mz_account::invite_token::{build_invite_url, sign_invite_token_now};
use mz_account::mail;
use mz_account::manual_seat_limit::assert_manual_seat_available;
use mz_account::new_employee_default::apply_new_employee_manual_time_default;
use mz_account::password::hash_password;
use mz_account::random_password::generate_default_random_password;
use mz_account::signup_tracking::{
    ADMIN_INVITED, SignUpEventInput, build_sign_up_properties, product_type_from_modules,
};
use mz_account::username_conflict::is_username_conflict_api;
use mz_core::company::find_company_id;
use mz_core::enums::SubscriptionBillingSource;
use mz_core::mixpanel::{MixpanelEvent, TrackInput, track};
use mz_core::subscription_lock::current_subscription;
use mz_core::time::JsDate;
use mz_core::user_status::{STATUS_ACTIVE, STATUS_INVITED};
use mz_core::{ApiError, ApiResult, AppState};
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::dto::js_parse_int;
use crate::model::{InviteInput, http_exception_message, is_http_exception};

const INVITE_USER_ALREADY_REGISTERED: &str = "User already registered";
const INVITE_RESENT: &str = "invitation resent";

/// Baris `users` yang baru dibuat TANPA `password`, seperti `{ ...user, password: undefined }`.
/// Token Google Calendar selalu `null` untuk akun baru (ditambahkan saat serialisasi).
#[derive(Debug, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
struct InvitedUserRow {
    id: i32,
    uuid: String,
    username: String,
    name: String,
    #[sqlx(rename = "createdAt")]
    created_at: JsDate,
    #[sqlx(rename = "updatedAt")]
    updated_at: Option<JsDate>,
    #[sqlx(rename = "sentAt")]
    sent_at: Option<JsDate>,
    status: i32,
    #[sqlx(rename = "tokensValidAfter")]
    tokens_valid_after: Option<JsDate>,
    #[sqlx(rename = "googleId")]
    google_id: Option<String>,
    #[sqlx(rename = "googleCalendarTokenExpiresAt")]
    google_calendar_token_expires_at: Option<JsDate>,
    #[sqlx(rename = "googleCalendarEmail")]
    google_calendar_email: Option<String>,
    #[sqlx(rename = "productTourStatus")]
    product_tour_status: Option<String>,
    #[sqlx(rename = "productTourStep")]
    product_tour_step: Option<String>,
    #[sqlx(rename = "productTourUpdatedAt")]
    product_tour_updated_at: Option<JsDate>,
    #[sqlx(rename = "gaClientId")]
    ga_client_id: Option<String>,
    #[sqlx(rename = "emailVerifiedAt")]
    email_verified_at: Option<JsDate>,
    #[sqlx(rename = "salesOnboardingEmailEligible")]
    sales_onboarding_email_eligible: bool,
    #[sqlx(rename = "salesOnboardingEmailSentAt")]
    sales_onboarding_email_sent_at: Option<JsDate>,
}

impl InvitedUserRow {
    fn to_json(&self) -> Value {
        let mut value = serde_json::to_value(self).unwrap_or(Value::Null);
        if let Value::Object(map) = &mut value {
            for key in [
                "googleCalendarAccessToken",
                "googleCalendarRefreshToken",
                "googleCalendarSyncToken",
            ] {
                map.insert(key.into(), Value::Null);
            }
        }
        value
    }
}

#[derive(sqlx::FromRow)]
struct ExistingUser {
    id: i32,
    uuid: String,
    name: String,
    username: String,
    status: i32,
    member: bool,
}

enum MailPlan {
    Invite { url: String },
    Credentials { password: String },
}

fn failure(message: &str) -> Value {
    json!({ "success": false, "message": message, "data": null })
}

/// `UsersService.invite(dto, userId)`. `Err` hanya untuk `HttpException` (422 plafon jabatan,
/// 403, 409 kursi) — semua galat lain menjadi `200 { success: false }` seperti `catch` Node.
pub async fn invite(
    state: &AppState,
    audit: &mz_audit::AuditHandle,
    input: &InviteInput,
    user_id: i32,
) -> ApiResult<Value> {
    match invite_inner(state, audit, input, user_id).await {
        Ok(v) => Ok(v),
        Err(err) if is_http_exception(&err) => Err(err),
        // TOCTOU: dua invite bersamaan untuk email yang sama; yang kalah menabrak unique `users.username`.
        Err(err) if is_username_conflict_api(&err) => Ok(failure(INVITE_USER_ALREADY_REGISTERED)),
        Err(err) => {
            tracing::error!(error = %err, "invite gagal");
            Ok(failure("invitation failed"))
        }
    }
}

async fn invite_inner(
    state: &AppState,
    audit: &mz_audit::AuditHandle,
    input: &InviteInput,
    user_id: i32,
) -> ApiResult<Value> {
    // 1. Validasi pengundang.
    let Some(company_id) = find_company_id(&state.db, user_id).await? else {
        return Ok(failure("Inviter company not found"));
    };
    let inviter: Option<(String, String, bool)> = sqlx::query_as(
        r#"SELECT u.name, c.name, c."dontChargeInvite" FROM users u, company c
           WHERE u.id = $1 AND c.id = $2"#,
    )
    .bind(user_id)
    .bind(company_id)
    .fetch_optional(&state.db)
    .await?;
    let Some((inviter_name, company_name, dont_charge_invite)) = inviter else {
        return Ok(failure("Inviter company not found"));
    };

    // 2. Blokir invite bila ada invoice PRORATION belum dibayar. Company MANUAL tidak pernah
    //    ditagih proration (R-18): cek dilewati.
    let current = current_subscription(&state.db, company_id).await?;
    let is_manual_billing = current
        .as_ref()
        .is_some_and(|s| s.billing_source == SubscriptionBillingSource::MANUAL);
    let active_subscription: Option<i32> = if is_manual_billing {
        None
    } else {
        sqlx::query_scalar(
            r#"SELECT id FROM subscriptions
               WHERE "companyId" = $1 AND status = 'ACTIVE'::"SubscriptionStatus" ORDER BY id LIMIT 1"#,
        )
        .bind(company_id)
        .fetch_optional(&state.db)
        .await?
    };
    if let (Some(subscription_id), false) = (active_subscription, dont_charge_invite) {
        let unpaid: Option<String> = sqlx::query_scalar(
            r#"SELECT "invoiceNumber" FROM invoices
               WHERE "subscriptionId" = $1 AND "invoiceType" = 'PRORATION'::"InvoiceType"
                 AND status = 'UNPAID'::"InvoiceStatus" ORDER BY id LIMIT 1"#,
        )
        .bind(subscription_id)
        .fetch_optional(&state.db)
        .await?;
        if let Some(number) = unpaid {
            return Ok(failure(&format!(
                "Cannot invite new users while proration invoice {number} is unpaid. Please complete the payment first."
            )));
        }
    }

    // 3. Persiapan data. Login & confirm mencari username huruf kecil; tanpa normalisasi
    //    `Budi@x.com` lolos cek "sudah terdaftar" dan menjadi user kembar.
    let username = input.username.trim().to_lowercase();
    let new_date = JsDate::now();
    let want_generated = input.generate_password;
    // bcrypt CPU-bound: di luar transaksi supaya koneksi DB tidak tertahan.
    let plain_password = want_generated.then(generate_default_random_password);
    let password_hash = match &plain_password {
        Some(plain) => hash_password(state, plain).await?,
        None => String::new(),
    };

    // Aturan B: plafon jabatan (endpoint ini terbuka untuk MANAGER).
    let access_level = {
        let mut conn = state.db.acquire().await?;
        let actor = require_actor_role(&mut conn, user_id, company_id).await?;
        resolve_assignable_role(
            &mut conn,
            &AssignableRoleActor::Actor(actor),
            company_id,
            RoleRef {
                code: Some(&input.access_level_code),
                uuid: None,
            },
        )
        .await?
    };
    let company_json = json!({
        "companyId": company_id,
        "company": { "name": company_name, "dontChargeInvite": dont_charge_invite },
    });

    // 4. TRANSAKSI. Email dikirim SESUDAH commit.
    let mut tx = state.db.begin().await?;
    // §9.1: `.catch(() => discardAudit())` transaksi ini — galat apa pun sebelum commit membuang
    // buffer audit request.
    let discard = audit.discard_guard();
    // R-18: batas kursi company MANUAL, di bawah lock `seats:<companyId>`.
    assert_manual_seat_available(&mut tx, company_id, 1, None).await?;

    // Email yang sudah terdaftar (company mana pun, aktif atau belum aktivasi) ditolak.
    // Pengecualian (audit C3): user yang masih INVITED di company ini dikirimi link baru.
    let existing = sqlx::query_as::<_, ExistingUser>(
        r#"SELECT u.id, u.uuid::text AS "uuid", u.name, u.username, u.status,
                  EXISTS (SELECT 1 FROM "userCompany" uc
                          WHERE uc."userId" = u.id AND uc."companyId" = $2 AND uc.status = 1) AS member
           FROM users u WHERE u.username = $1"#,
    )
    .bind(&username)
    .bind(company_id)
    .fetch_optional(&mut *tx)
    .await?;

    let (result, mail_plan, created): (Value, MailPlan, Option<String>) = if let Some(existing) =
        existing
    {
        let can_resend = !want_generated && existing.status == STATUS_INVITED && existing.member;
        if !can_resend {
            // Callback transaksi Node mengembalikan nilai (tidak melempar): tidak ada discard.
            discard.disarm();
            return Ok(json!({ "success": false, "message": INVITE_USER_ALREADY_REGISTERED }));
        }
        let token = sign_invite_token_now(state, &existing.uuid)?;
        let url = build_invite_url(state, &existing.username, &token);
        (
            json!({
                "success": true,
                "message": INVITE_RESENT,
                "data": {
                    "user": {
                        "id": existing.id,
                        "uuid": existing.uuid,
                        "name": existing.name,
                        "username": existing.username,
                        "status": existing.status,
                    },
                    "company": company_json,
                    "accessLevel": null,
                    "token": null,
                    "isPasswordGenerated": false,
                },
            }),
            MailPlan::Invite { url },
            None,
        )
    } else {
        let status = if want_generated {
            STATUS_ACTIVE
        } else {
            STATUS_INVITED
        };
        let user = sqlx::query_as::<_, InvitedUserRow>(
            r#"INSERT INTO users (username, name, password, status, "createdAt", "sentAt")
               VALUES ($1, $2, $3, $4, $5, NULL)
               RETURNING id, uuid::text AS "uuid", username, name, "createdAt", "updatedAt", "sentAt",
                         status, "tokensValidAfter", "googleId", "googleCalendarTokenExpiresAt",
                         "googleCalendarEmail", "productTourStatus", "productTourStep",
                         "productTourUpdatedAt", "gaClientId", "emailVerifiedAt",
                         "salesOnboardingEmailEligible", "salesOnboardingEmailSentAt""#,
        )
        .bind(&username)
        .bind(&input.name)
        .bind(&password_hash)
        .bind(status)
        .bind(new_date)
        .fetch_one(&mut *tx)
        .await?;
        // Audit `users.create` tanpa `select` (allowlist: username, name, status).
        audit
            .record_create(
                mz_audit::Model::Users,
                &mz_audit::Row::new()
                    .with("id", user.id)
                    .with("uuid", user.uuid.as_str())
                    .with("username", user.username.as_str())
                    .with("name", user.name.as_str())
                    .with("status", user.status),
            )
            .await;

        let idle = state
            .config
            .var("IDLETIME_IN_SECOND")
            .as_deref()
            .and_then(js_parse_int)
            .and_then(|n| i32::try_from(n).ok())
            .ok_or_else(|| {
                ApiError::internal(anyhow::anyhow!("IDLETIME_IN_SECOND is not a number"))
            })?;
        // Email yang sudah terdaftar ditolak di atas, jadi baris ini selalu keanggotaan baru.
        let membership_id: i32 = sqlx::query_scalar(
            r#"INSERT INTO "userCompany"
                 ("userId", "companyId", status, "isScreenshotActive", "isAllowManualTime",
                  "isAllowEditTime", "isDefault", "idleTimeInSecond", "invitedById", "inviteMethod")
               VALUES ($1, $2, $3, $4, false, false, true, $5, $6, $7) RETURNING id"#,
        )
        .bind(user.id)
        .bind(company_id)
        .bind(STATUS_ACTIVE)
        .bind(input.is_screenshot_active)
        .bind(idle)
        .bind(user_id)
        .bind(invite_method::EMAIL)
        .fetch_one(&mut *tx)
        .await?;
        if let Some(row) = audit
            .result_row(
                &mut *tx,
                mz_audit::Model::UserCompany,
                mz_audit::Lookup::Id(membership_id),
            )
            .await
        {
            audit
                .record_create(mz_audit::Model::UserCompany, &row)
                .await;
        }

        // T6 (research R5): helper mengambil lock settings shared di transaksi join ini.
        apply_new_employee_manual_time_default(&mut tx, audit, company_id, user.id, Some(user_id))
            .await?;

        sqlx::query(
            r#"INSERT INTO "userAccessLevel" ("userId", "accessLevelId", "companyId", status)
               VALUES ($1, $2, $3, $4)"#,
        )
        .bind(user.id)
        .bind(access_level.id)
        .bind(company_id)
        .bind(STATUS_ACTIVE)
        .execute(&mut *tx)
        .await?;
        audit
            .record_create(
                mz_audit::Model::UserAccessLevel,
                &mz_audit::Row::new()
                    .with("userId", user.id)
                    .with("accessLevelId", access_level.id)
                    .with("companyId", company_id)
                    .with("status", STATUS_ACTIVE),
            )
            .await;

        let (mail_plan, generated) = match plain_password {
            Some(password) => (MailPlan::Credentials { password }, true),
            None => {
                // Token hanya dikirim lewat email, tidak di respons API (audit C3).
                let token = sign_invite_token_now(state, &user.uuid)?;
                (
                    MailPlan::Invite {
                        url: build_invite_url(state, &username, &token),
                    },
                    false,
                )
            }
        };
        let uuid = user.uuid.clone();
        (
            json!({
                "success": true,
                "message": "invitation success",
                "data": {
                    "user": user.to_json(),
                    "company": company_json,
                    "accessLevel": serde_json::to_value(&access_level).unwrap_or(Value::Null),
                    "token": null,
                    "isPasswordGenerated": generated,
                },
            }),
            mail_plan,
            Some(uuid),
        )
    };
    tx.commit().await?;
    discard.disarm();

    if let Some(new_user_uuid) = created {
        spawn_invited_sign_up(
            state.clone(),
            new_user_uuid,
            user_id,
            company_id,
            access_level.effective_code(),
            new_date,
        );
    }

    // 5. Email setelah commit (jangan kirim untuk transaksi yang rollback). Hasilnya diabaikan
    //    oleh Node juga (`MailService` menelan galat).
    match mail_plan {
        MailPlan::Credentials { password } => {
            let host_fe = mz_account::util::env_or_undefined(state.config.var("HOST_FE"));
            mail::send_invite_credentials_email(
                state,
                &username,
                &input.name,
                &inviter_name,
                &company_name,
                &password,
                &format!("{host_fe}/login"),
            )
            .await;
        }
        MailPlan::Invite { url } => {
            mail::send_invite_email(
                state,
                &username,
                &input.name,
                &inviter_name,
                &company_name,
                &url,
            )
            .await;
        }
    }

    Ok(result)
}

/// `void this.emitInvitedSignUp(...)` — fire-and-forget; kegagalan hanya dilog.
fn spawn_invited_sign_up(
    state: AppState,
    new_user_uuid: String,
    inviter_id: i32,
    company_id: i32,
    role_code: String,
    occurred_at: JsDate,
) {
    mz_core::bg::spawn(async move {
        if let Err(err) = emit_invited_sign_up(
            &state,
            &new_user_uuid,
            inviter_id,
            company_id,
            role_code,
            occurred_at,
        )
        .await
        {
            tracing::warn!(error = %err, "Emisi Mixpanel Sign Up (admin_invited) dilewati");
        }
    });
}

/// Event `Sign Up` (`signup_type: admin_invited`); `distinct_id` = uuid user BARU, bukan pengundang.
async fn emit_invited_sign_up(
    state: &AppState,
    new_user_uuid: &str,
    inviter_id: i32,
    company_id: i32,
    role_code: String,
    occurred_at: JsDate,
) -> ApiResult<()> {
    let company: Option<(String, bool, bool)> = sqlx::query_as(
        r#"SELECT uuid::text, "moduleTimeTrackingEnabled", "moduleSalesTrackerEnabled"
           FROM company WHERE id = $1"#,
    )
    .bind(company_id)
    .fetch_optional(&state.db)
    .await?;
    let Some((company_uuid, time_tracking, sales_tracker)) = company else {
        return Ok(());
    };
    let inviter_uuid: Option<String> =
        sqlx::query_scalar("SELECT uuid::text FROM users WHERE id = $1")
            .bind(inviter_id)
            .fetch_optional(&state.db)
            .await?;

    let properties: Map<String, Value> = build_sign_up_properties(&SignUpEventInput {
        signup_type: ADMIN_INVITED,
        signup_method: "email",
        signup_platform: "web".to_owned(),
        product_type: product_type_from_modules(time_tracking, sales_tracker),
        role: role_code,
        company_uuid,
        is_workspace_owner: false,
        plan: None,
        invited_by_uuid: inviter_uuid,
        attribution: None,
        occurred_at: occurred_at.utc(),
    });
    track(
        state,
        TrackInput {
            event: MixpanelEvent::SignUp,
            distinct_id: new_user_uuid.to_owned(),
            dedup_key: new_user_uuid.to_owned(),
            properties,
            occurred_at_ms: Some(occurred_at.timestamp_millis()),
        },
    );
    Ok(())
}

/// Loop `inviteBatch`: tiap employee lewat [`invite`]; `HttpException` (mis. 422 plafon jabatan,
/// 409 kursi) ditangkap PER employee supaya satu baris buruk tidak membatalkan undangan lain.
pub async fn invite_batch(
    state: &AppState,
    audit: &mz_audit::AuditHandle,
    items: &[InviteInput],
    user_id: i32,
) -> ApiResult<Value> {
    let mut results: Vec<Value> = Vec::with_capacity(items.len());
    for item in items {
        results.push(batch_entry(
            &item.username,
            invite(state, audit, item, user_id).await,
        ));
    }
    Ok(json!({ "success": true, "message": "Invite batch success", "data": results }))
}

/// Satu elemen `results` batch dari hasil `invite`.
pub fn batch_entry(username: &str, outcome: ApiResult<Value>) -> Value {
    match outcome {
        Err(err) => json!({
            "username": username,
            "status": http_exception_message(&err).unwrap_or_else(|| "failed".to_owned()),
        }),
        Ok(result) if result.get("success").and_then(Value::as_bool) == Some(false) => {
            let message = result
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("failed");
            json!({ "username": username, "status": message })
        }
        Ok(result) => json!({
            "username": username,
            "status": "invited",
            "isPasswordGenerated":
                result.pointer("/data/isPasswordGenerated").and_then(Value::as_bool) == Some(true),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    #[test]
    fn batch_entry_success_reports_generated_flag() {
        let ok = json!({ "success": true, "data": { "isPasswordGenerated": true } });
        assert_eq!(
            batch_entry("a@b.co", Ok(ok)),
            json!({ "username": "a@b.co", "status": "invited", "isPasswordGenerated": true })
        );
        let resent = json!({ "success": true, "message": "invitation resent", "data": { "isPasswordGenerated": false } });
        assert_eq!(
            batch_entry("a@b.co", Ok(resent)),
            json!({ "username": "a@b.co", "status": "invited", "isPasswordGenerated": false })
        );
    }

    #[test]
    fn batch_entry_failure_uses_message() {
        let failed = json!({ "success": false, "message": INVITE_USER_ALREADY_REGISTERED });
        assert_eq!(
            batch_entry("a@b.co", Ok(failed)),
            json!({ "username": "a@b.co", "status": "User already registered" })
        );
        assert_eq!(
            batch_entry("a@b.co", Ok(json!({ "success": false }))),
            json!({ "username": "a@b.co", "status": "failed" })
        );
    }

    #[test]
    fn batch_entry_http_exception_does_not_abort() {
        let err =
            ApiError::unprocessable("Access Level 'SYSTEM_ADMIN' is above your own access level");
        assert_eq!(
            batch_entry("a@b.co", Err(err)),
            json!({ "username": "a@b.co", "status": "Access Level 'SYSTEM_ADMIN' is above your own access level" })
        );
        let seat = ApiError::body(
            StatusCode::CONFLICT,
            json!({ "code": "SEAT_LIMIT_REACHED", "message": "Seat limit reached (3/3). Contact the Timemoz team to add seats." }),
        );
        assert_eq!(
            batch_entry("x@y.co", Err(seat))["status"],
            json!("Seat limit reached (3/3). Contact the Timemoz team to add seats.")
        );
    }

    #[test]
    fn invited_user_json_never_contains_password_and_has_null_tokens() {
        let row = InvitedUserRow {
            id: 1,
            uuid: "u".into(),
            username: "a@b.co".into(),
            name: "A".into(),
            created_at: JsDate::now(),
            updated_at: None,
            sent_at: None,
            status: 2,
            tokens_valid_after: None,
            google_id: None,
            google_calendar_token_expires_at: None,
            google_calendar_email: None,
            product_tour_status: Some("NOT_STARTED".into()),
            product_tour_step: None,
            product_tour_updated_at: None,
            ga_client_id: None,
            email_verified_at: None,
            sales_onboarding_email_eligible: false,
            sales_onboarding_email_sent_at: None,
        };
        let v = row.to_json();
        assert!(v.get("password").is_none());
        assert_eq!(v["googleCalendarRefreshToken"], Value::Null);
        assert_eq!(v["productTourStatus"], json!("NOT_STARTED"));
        assert_eq!(v["sentAt"], Value::Null);
        assert_eq!(v["status"], json!(2));
    }
}
