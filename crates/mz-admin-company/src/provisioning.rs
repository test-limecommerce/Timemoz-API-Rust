//! `src/admin-company/provisioning/*` — operator membuat company + owner, dan menambah user
//! ke company (2 route). Email kredensial dikirim SESUDAH commit lewat `mz_core::mail`
//! (template `invite-credentials` yang sama).

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use mz_account::assignable_role::{AssignableRoleActor, RoleRef, resolve_assignable_role};
use mz_audit::{AuditHandle, Lookup, Model, Row};
use mz_core::time::{now_naive, parse_js_date};
use mz_core::{ApiError, ApiResult, AppState};
use mz_operator::{AuditCtx, Operator, OperatorAuditEntry, record};
use serde_json::{Map, Value, json};
use sqlx::PgConnection;

use crate::common::{
    OWNER_SUPPORT, RawBody, body_value, coded, company_not_found, date_of, gate,
    is_invite_code_collision, uuid_pipe,
};
use crate::ownership::REGISTER_EMAIL_MAX;
use crate::val::{V, absent, array_keys, as_obj, trim_field, trim_lower_field};

type Op = Result<Operator, ApiError>;

const OPERATOR_SENDER_NAME: &str = "Tim Timemoz";
const COMPANY_USER_ROLES: [&str; 4] = ["ADMIN", "MANAGER", "STAFF", "GUEST"];
const FEATURE_MODULES: [&str; 2] = ["TIME_TRACKING", "SALES_TRACKER"];
const INVITE_CODE_MAX_ATTEMPTS: usize = 5;

fn email_in_use() -> ApiError {
    coded(
        StatusCode::CONFLICT,
        "EMAIL_IN_USE",
        "This email is already registered",
    )
}

// ── DTO ─────────────────────────────────────────────────────────────────────

const ACCOUNT_FIELDS: [&str; 3] = ["name", "email", "password"];

/// `OperatorAccountDto` (name, email, password) atas objek yang sudah di-transform.
fn validate_account(v: &mut V, m: &Map<String, Value>) {
    let name = m.get("name");
    v.max_length("name", name, 255);
    v.not_empty("name", name);
    v.is_string("name", name);
    let email = m.get("email");
    v.max_length("email", email, 255);
    v.is_email("email", email);
    let password = m.get("password");
    if !absent(password) {
        v.length("password", password, 8, 72);
        v.is_string("password", password);
    }
}

fn transform_account(m: &mut Map<String, Value>) {
    trim_field(m, "name");
    trim_lower_field(m, "email");
}

/// Akun hasil validasi.
#[derive(Debug, Clone)]
pub struct AccountInput {
    pub name: String,
    pub email: String,
    pub password: Option<String>,
}

fn account_of(m: &Map<String, Value>) -> AccountInput {
    let s = |k: &str| {
        m.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    AccountInput {
        name: s("name"),
        email: s("email"),
        password: m.get("password").and_then(Value::as_str).map(str::to_owned),
    }
}

#[derive(Debug, Clone)]
pub struct CreateCompanyInput {
    pub company_name: String,
    pub telp: Option<String>,
    pub time_zone: String,
    pub modules: Vec<String>,
    pub pricing_uuid: String,
    pub trial_end_date: String,
    /// `None` = `owner` tidak dikirim (Node: TypeError → 500).
    pub owner: Option<AccountInput>,
    pub reason: String,
}

/// `CreateCompanyDto` (properti sendiri dulu, lalu `reason` warisan `ReasonDto`).
pub fn validate_create_company(body: &Value) -> ApiResult<CreateCompanyInput> {
    let mut m = as_obj(body);
    trim_field(&mut m, "companyName");
    trim_field(&mut m, "telp");
    trim_field(&mut m, "reason");
    if let Some(Value::Object(o)) = m.get_mut("owner") {
        transform_account(o);
    }
    let mut v = V::new();
    for e in array_keys(body) {
        v.push(e);
    }
    v.whitelist(
        &m,
        &[
            "companyName",
            "telp",
            "timeZone",
            "modules",
            "pricingUuid",
            "trialEndDate",
            "owner",
            "reason",
        ],
    );
    let cn = m.get("companyName");
    v.max_length("companyName", cn, 255);
    v.not_empty("companyName", cn);
    v.is_string("companyName", cn);
    if !absent(m.get("telp")) {
        v.max_length("telp", m.get("telp"), 255);
        v.is_string("telp", m.get("telp"));
    }
    v.is_time_zone("timeZone", m.get("timeZone"));
    v.is_string("timeZone", m.get("timeZone"));
    validate_modules(&mut v, m.get("modules"));
    v.is_uuid("pricingUuid", m.get("pricingUuid"));
    v.is_iso_strict("trialEndDate", m.get("trialEndDate"));
    match m.get("owner") {
        None => {}
        Some(Value::Object(o)) => {
            let mut child = V::nested("owner");
            child.whitelist(o, &ACCOUNT_FIELDS);
            validate_account(&mut child, o);
            v.errors.extend(child.errors);
        }
        Some(Value::Array(items)) => {
            for (i, item) in items.iter().enumerate() {
                let mut child = V::nested(&format!("owner.{i}"));
                match item {
                    Value::Object(o) => {
                        let mut o = o.clone();
                        transform_account(&mut o);
                        child.whitelist(&o, &ACCOUNT_FIELDS);
                        validate_account(&mut child, &o);
                    }
                    _ => child.nested_not_object(&i.to_string()),
                }
                v.errors.extend(child.errors);
            }
        }
        Some(_) => v.nested_not_object("owner"),
    }
    v.reason(m.get("reason"));
    v.finish()?;

    let s = |k: &str| {
        m.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    Ok(CreateCompanyInput {
        company_name: s("companyName"),
        telp: m.get("telp").and_then(Value::as_str).map(str::to_owned),
        time_zone: s("timeZone"),
        modules: m
            .get("modules")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
        pricing_uuid: s("pricingUuid"),
        trial_end_date: s("trialEndDate"),
        owner: m.get("owner").and_then(Value::as_object).map(account_of),
        reason: s("reason"),
    })
}

/// `@IsArray @ArrayNotEmpty @ArrayUnique @IsEnum(FeatureModule, { each: true })`
/// (pesan dari dekorator paling bawah).
fn validate_modules(v: &mut V, val: Option<&Value>) {
    let arr = val.and_then(Value::as_array);
    let each_ok = match (arr, val) {
        (Some(a), _) => a
            .iter()
            .all(|x| matches!(x.as_str(), Some(s) if FEATURE_MODULES.contains(&s))),
        (None, Some(Value::String(s))) => FEATURE_MODULES.contains(&s.as_str()),
        _ => false,
    };
    if !each_ok {
        v.push(format!(
            "each value in modules must be one of the following values: {}",
            FEATURE_MODULES.join(", ")
        ));
    }
    let unique = arr.is_some_and(|a| {
        let mut seen: Vec<&Value> = Vec::new();
        a.iter().all(|x| {
            if seen.contains(&x) {
                false
            } else {
                seen.push(x);
                true
            }
        })
    });
    if !unique {
        v.push("All modules's elements must be unique");
    }
    if !arr.is_some_and(|a| !a.is_empty()) {
        v.push("modules should not be empty");
    }
    if arr.is_none() {
        v.push("modules must be an array");
    }
}

#[derive(Debug, Clone)]
pub struct CreateUserInput {
    pub role: String,
    pub is_screenshot_active: Option<bool>,
    pub reason: String,
    pub account: AccountInput,
}

/// `CreateCompanyUserDto` (role, isScreenshotActive, reason, lalu warisan name/email/password).
pub fn validate_create_user(body: &Value) -> ApiResult<CreateUserInput> {
    let mut m = as_obj(body);
    trim_field(&mut m, "reason");
    transform_account(&mut m);
    let mut v = V::new();
    for e in array_keys(body) {
        v.push(e);
    }
    v.whitelist(
        &m,
        &[
            "role",
            "isScreenshotActive",
            "reason",
            "name",
            "email",
            "password",
        ],
    );
    v.is_in("role", m.get("role"), &COMPANY_USER_ROLES);
    if !absent(m.get("isScreenshotActive")) {
        v.is_boolean("isScreenshotActive", m.get("isScreenshotActive"));
    }
    v.reason(m.get("reason"));
    validate_account(&mut v, &m);
    v.finish()?;
    Ok(CreateUserInput {
        role: m
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        is_screenshot_active: m.get("isScreenshotActive").and_then(Value::as_bool),
        reason: m
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        account: account_of(&m),
    })
}

// ── helper ──────────────────────────────────────────────────────────────────

struct Prepared {
    name: String,
    email: String,
    hash: String,
    generated: Option<String>,
}

async fn assert_email_free(state: &AppState, email: &str) -> ApiResult<()> {
    let id: Option<i32> = sqlx::query_scalar("SELECT id FROM users WHERE username = $1")
        .bind(email)
        .fetch_optional(&state.db)
        .await?;
    if id.is_some() {
        return Err(email_in_use());
    }
    Ok(())
}

/// bcrypt di luar transaksi.
async fn prepare(state: &AppState, a: &AccountInput) -> ApiResult<Prepared> {
    let generated = match a.password.as_deref() {
        Some(p) if !p.is_empty() => None,
        _ => Some(mz_account::random_password::generate_default_random_password()),
    };
    let plain = match (&a.password, &generated) {
        (Some(p), None) => p.clone(),
        (_, Some(g)) => g.clone(),
        (None, None) => String::new(),
    };
    let hash = mz_account::password::hash_password(state, &plain).await?;
    Ok(Prepared {
        name: a.name.clone(),
        email: a.email.clone(),
        hash,
        generated,
    })
}

/// Kirim password acak setelah COMMIT; `false` = gagal / tidak dikirim.
async fn send_credentials(state: &AppState, p: &Prepared, company_name: &str) -> bool {
    let Some(password) = p.generated.as_deref() else {
        return false;
    };
    let url = format!(
        "{}/login",
        mz_account::util::env_or_undefined(state.config.var("HOST_FE"))
    );
    let res = mz_account::mail::send_invite_credentials_email(
        state,
        &p.email,
        &p.name,
        OPERATOR_SENDER_NAME,
        company_name,
        password,
        &url,
    )
    .await;
    res.success
}

/// `users.create({ data, select: { id, uuid, name, username } })` + audit.
async fn create_user(
    conn: &mut PgConnection,
    audit: &AuditHandle,
    p: &Prepared,
) -> ApiResult<(i32, uuid::Uuid)> {
    let now = now_naive();
    let (id, uuid): (i32, uuid::Uuid) = sqlx::query_as(
        r#"INSERT INTO users (name, username, password, status, "emailVerifiedAt", "createdAt", "sentAt")
           VALUES ($1, $2, $3, 1, $4, $4, NULL) RETURNING id, uuid"#,
    )
    .bind(&p.name)
    .bind(&p.email)
    .bind(&p.hash)
    .bind(now)
    .fetch_one(&mut *conn)
    .await
    .map_err(|e| {
        if mz_account::username_conflict::is_username_conflict(&e) {
            email_in_use()
        } else {
            ApiError::from(e)
        }
    })?;
    audit
        .record_create(
            Model::Users,
            &Row::new()
                .with("id", id)
                .with("uuid", uuid.to_string())
                .with("name", p.name.as_str())
                .with("username", p.email.as_str()),
        )
        .await;
    Ok((id, uuid))
}

#[derive(sqlx::FromRow)]
struct Tier {
    id: i32,
    uuid: uuid::Uuid,
    name: String,
    #[sqlx(rename = "minUsers")]
    min_users: i32,
    #[sqlx(rename = "maxUsers")]
    max_users: i32,
    #[sqlx(rename = "maxProject")]
    max_project: i32,
    #[sqlx(rename = "pricePerUserUsd")]
    price_per_user_usd: f64,
    #[sqlx(rename = "billingInterval")]
    billing_interval: String,
    #[sqlx(rename = "productType")]
    product_type: String,
    #[sqlx(rename = "paypalPlanId")]
    paypal_plan_id: Option<String>,
    #[sqlx(rename = "useIdleDetection")]
    use_idle_detection: Option<bool>,
    #[sqlx(rename = "useCheatDetection")]
    use_cheat_detection: Option<bool>,
    #[sqlx(rename = "useReports")]
    use_reports: Option<bool>,
    #[sqlx(rename = "useTimesheetApproval")]
    use_timesheet_approval: Option<bool>,
    #[sqlx(rename = "useActivityLevel")]
    use_activity_level: Option<bool>,
    #[sqlx(rename = "useIntegrations")]
    use_integrations: Option<bool>,
}

// ── POST /admin-company ─────────────────────────────────────────────────────

/// Node: src/admin-company/provisioning/company-provisioning.controller.ts:42 (`POST /admin-company`)
pub async fn h_create_company(
    State(state): State<AppState>,
    op: Op,
    ctx: AuditCtx,
    audit: AuditHandle,
    body: RawBody,
) -> ApiResult<impl IntoResponse> {
    let op = gate(op, &audit, Some(&body), Some(OWNER_SUPPORT))?;
    let dto = validate_create_company(&body_value(body))?;

    let now = now_naive();
    let trial_end = parse_js_date(&dto.trial_end_date).filter(|t| *t > now);
    let Some(trial_end) = trial_end else {
        return Err(coded(
            StatusCode::BAD_REQUEST,
            "INVALID_DATE",
            "trialEndDate must be a valid date in the future",
        ));
    };
    let tier: Option<Tier> = sqlx::query_as(
        r#"SELECT id, uuid, name, "minUsers", "maxUsers", "maxProject", "pricePerUserUsd", "billingInterval",
                  "productType"::text AS "productType", "paypalPlanId", "useIdleDetection", "useCheatDetection",
                  "useReports", "useTimesheetApproval", "useActivityLevel", "useIntegrations"
           FROM "pricingTiers" WHERE uuid = $1::uuid AND status = 1 ORDER BY id LIMIT 1"#,
    )
    .bind(&dto.pricing_uuid)
    .fetch_optional(&state.db)
    .await?;
    let Some(tier) = tier else {
        return Err(coded(
            StatusCode::NOT_FOUND,
            "PRICING_NOT_FOUND",
            "Active pricing tier not found",
        ));
    };
    // Node: `dto.owner.email` dengan `owner` undefined → TypeError (500).
    let Some(owner_in) = dto.owner.clone() else {
        return Err(ApiError::internal(anyhow::anyhow!(
            "Cannot read properties of undefined (reading 'email')"
        )));
    };
    assert_email_free(&state, &owner_in.email).await?;
    let owner = prepare(&state, &owner_in).await?;
    let mut warnings: Vec<String> = Vec::new();
    let email_fits = owner.email.encode_utf16().count() <= REGISTER_EMAIL_MAX;
    if !email_fits {
        warnings.push("OWNER_EMAIL_TOO_LONG_FOR_COMPANY_RECORD".into());
    }
    let idle = mz_auth::service::register::idle_time_in_second(&state)?;

    let mut attempt = 0;
    let (user_uuid, company_uuid) = loop {
        attempt += 1;
        let code = mz_auth::service::invite::generate_invite_code();
        let res = create_company_tx(
            &state, &audit, &ctx, &op, &dto, &tier, &owner, email_fits, idle, &code, now, trial_end,
        )
        .await;
        // Seperti Node, entri audit tulisan yang sudah berhasil sebelum rollback tidak dibuang.
        match res {
            Ok(v) => break v,
            Err(e) if attempt < INVITE_CODE_MAX_ATTEMPTS && is_invite_code_collision(&e) => {}
            Err(e) => return Err(e),
        }
    };

    let sent = send_credentials(&state, &owner, &dto.company_name).await;
    if owner.generated.is_some() && !sent {
        warnings.push("CREDENTIALS_EMAIL_FAILED".into());
    }
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "success": true,
            "message": "OK",
            "data": {
                "company": { "uuid": company_uuid.to_string(), "name": dto.company_name },
                "owner": {
                    "uuid": user_uuid.to_string(),
                    "name": owner.name,
                    "email": owner.email,
                    "role": "SUPER_ADMIN",
                },
                "passwordGenerated": owner.generated.is_some(),
                "credentialsEmailSent": sent,
                "warnings": warnings,
            },
        })),
    ))
}

#[allow(clippy::too_many_arguments)]
async fn create_company_tx(
    state: &AppState,
    audit: &AuditHandle,
    ctx: &AuditCtx,
    op: &Operator,
    dto: &CreateCompanyInput,
    tier: &Tier,
    owner: &Prepared,
    email_fits: bool,
    idle: i32,
    invite_code: &str,
    now: chrono::NaiveDateTime,
    trial_end: chrono::NaiveDateTime,
) -> ApiResult<(uuid::Uuid, uuid::Uuid)> {
    let mut tx = state.db.begin().await?;
    let (user_id, user_uuid) = create_user(&mut tx, audit, owner).await?;

    let time = dto.modules.iter().any(|m| m == "TIME_TRACKING");
    let sales = dto.modules.iter().any(|m| m == "SALES_TRACKER");
    let (company_id, company_uuid): (i32, uuid::Uuid) = sqlx::query_as(
        r#"INSERT INTO company ("ownerUserId", name, telp, address, "timeZone", register_name, register_email,
               register_roles, status, "maxUsers", "maxProject", "useIdleDetection", "useCheatDetection",
               "useReports", "useTimesheetApproval", "useActivityLevel", "useIntegrations", "useActivityTrack",
               "isTrial", "hasCreateOrgnanization", "inviteCode", currency, "moduleTimeTrackingEnabled",
               "moduleSalesTrackerEnabled")
           VALUES ($1, $2, $3, '', $4, $5, $6, 'Owner', 1, $7, $8, $9, $10, $11, $12, $13, $14, true, true, true,
                   $15, $16, $17, $18)
           RETURNING id, uuid"#,
    )
    .bind(user_id)
    .bind(&dto.company_name)
    .bind(dto.telp.as_deref().filter(|t| !t.is_empty()).unwrap_or(""))
    .bind(&dto.time_zone)
    .bind(&owner.name)
    .bind(email_fits.then_some(owner.email.as_str()))
    .bind(tier.max_users)
    .bind(tier.max_project)
    .bind(tier.use_idle_detection)
    .bind(tier.use_cheat_detection)
    .bind(tier.use_reports)
    .bind(tier.use_timesheet_approval)
    .bind(tier.use_activity_level)
    .bind(tier.use_integrations)
    .bind(invite_code)
    .bind(if tier.product_type == "SALES" { "IDR" } else { "USD" })
    .bind(time)
    .bind(sales)
    .fetch_one(&mut *tx)
    .await?;
    audit
        .record_create(
            Model::Company,
            &Row::new()
                .with("id", company_id)
                .with("uuid", company_uuid.to_string())
                .with("name", dto.company_name.as_str()),
        )
        .await;

    mz_auth::service::register::seed_company_defaults(
        &mut tx,
        audit,
        company_id,
        user_id,
        &format!("{} Project", dto.company_name),
    )
    .await?;
    sqlx::query(r#"INSERT INTO trials ("companyId", "startDate", "endDate") VALUES ($1, $2, $3)"#)
        .bind(company_id)
        .bind(now)
        .bind(trial_end)
        .execute(&mut *tx)
        .await?;
    let billing_interval = if tier.billing_interval.is_empty() {
        "MONTH"
    } else {
        tier.billing_interval.as_str()
    };
    mz_auth::service::register::insert_subscription(
        &mut tx,
        &mz_auth::service::register::NewSubscription {
            company_id,
            tier_id: tier.id,
            start: now,
            end: trial_end,
            status: "TRIAL",
            quantity: if tier.min_users == 0 {
                1
            } else {
                tier.min_users
            },
            price_per_user: tier.price_per_user_usd,
            billing_interval,
            paypal_plan_id: tier.paypal_plan_id.as_deref(),
            paypal_subscription_id: None,
            paypal_status: None,
        },
    )
    .await?;
    mz_auth::service::register::attach_owner(&mut tx, audit, user_id, company_id, idle).await?;
    if sales {
        mz_auth::service::organization::init_quickstart(&mut tx, company_id).await?;
    }

    let after = json!({
        "name": dto.company_name,
        "timeZone": dto.time_zone,
        "modules": dto.modules,
        "pricing": { "uuid": tier.uuid.to_string(), "name": tier.name },
        "trialEndDate": date_of(trial_end),
        "owner": {
            "uuid": user_uuid.to_string(),
            "name": owner.name,
            "email": owner.email,
            "passwordGenerated": owner.generated.is_some(),
        },
    });
    record(
        &mut *tx,
        ctx,
        &OperatorAuditEntry {
            operator_id: op.id,
            action: "company.create",
            target_type: "company",
            target_uuid: Some(company_uuid),
            company_id: Some(company_id),
            before: None,
            after: Some(&after),
            reason: Some(&dto.reason),
        },
    )
    .await?;
    tx.commit().await?;
    Ok((user_uuid, company_uuid))
}

// ── POST /admin-company/{uuid}/users ────────────────────────────────────────

/// Node: src/admin-company/provisioning/company-provisioning.controller.ts:58 (`POST /admin-company/:uuid/users`)
pub async fn h_create_user(
    State(state): State<AppState>,
    op: Op,
    ctx: AuditCtx,
    audit: AuditHandle,
    Path(raw): Path<String>,
    body: RawBody,
) -> ApiResult<impl IntoResponse> {
    let op = gate(op, &audit, Some(&body), Some(OWNER_SUPPORT))?;
    // Node: galat body (pipe global) muncul sebelum `ParseUUIDPipe` param (diverifikasi paritas).
    let dto = validate_create_user(&body_value(body))?;
    let uuid = uuid_pipe(&raw)?;

    assert_email_free(&state, &dto.account.email).await?;
    let account = prepare(&state, &dto.account).await?;
    let idle = mz_auth::service::register::idle_time_in_second(&state);

    let (company_name, user_uuid) =
        create_user_tx(&state, &audit, &ctx, &op, &uuid, &dto, &account, idle).await?;

    let sent = send_credentials(&state, &account, &company_name).await;
    let mut warnings: Vec<String> = Vec::new();
    if account.generated.is_some() && !sent {
        warnings.push("CREDENTIALS_EMAIL_FAILED".into());
    }
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "success": true,
            "message": "OK",
            "data": {
                "user": {
                    "uuid": user_uuid.to_string(),
                    "name": account.name,
                    "email": account.email,
                    "role": dto.role,
                },
                "passwordGenerated": account.generated.is_some(),
                "credentialsEmailSent": sent,
                "warnings": warnings,
            },
        })),
    ))
}

#[allow(clippy::too_many_arguments)]
async fn create_user_tx(
    state: &AppState,
    audit: &AuditHandle,
    ctx: &AuditCtx,
    op: &Operator,
    company_uuid: &uuid::Uuid,
    dto: &CreateUserInput,
    account: &Prepared,
    idle: ApiResult<i32>,
) -> ApiResult<(String, uuid::Uuid)> {
    let mut tx = state.db.begin().await?;
    let company: Option<(i32, uuid::Uuid, String, i32)> =
        sqlx::query_as("SELECT id, uuid, name, status FROM company WHERE uuid = $1 FOR UPDATE")
            .bind(company_uuid)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((company_id, c_uuid, c_name, _)) = company.filter(|c| c.3 != 0) else {
        return Err(company_not_found());
    };
    mz_account::manual_seat_limit::assert_manual_seat_available(&mut tx, company_id, 1, None)
        .await?;
    let existing: Option<i32> = sqlx::query_scalar("SELECT id FROM users WHERE username = $1")
        .bind(&account.email)
        .fetch_optional(&mut *tx)
        .await?;
    if existing.is_some() {
        return Err(email_in_use());
    }
    let role = resolve_assignable_role(
        &mut tx,
        &AssignableRoleActor::SelfRegistration,
        company_id,
        RoleRef {
            code: Some(&dto.role),
            uuid: None,
        },
    )
    .await?;
    let (user_id, user_uuid) = create_user(&mut tx, audit, account).await?;
    let idle = idle?;
    let uc_id: i32 = sqlx::query_scalar(
        r#"INSERT INTO "userCompany" ("userId", "companyId", status, "isScreenshotActive", "isAllowManualTime",
               "isAllowEditTime", "isDefault", "idleTimeInSecond", "inviteMethod")
           VALUES ($1, $2, 1, $3, false, false, true, $4, $5) RETURNING id"#,
    )
    .bind(user_id)
    .bind(company_id)
    .bind(dto.is_screenshot_active.unwrap_or(true))
    .bind(idle)
    .bind(mz_account::invite_method::EMAIL)
    .fetch_one(&mut *tx)
    .await?;
    if let Some(r) = audit
        .result_row(&mut *tx, Model::UserCompany, Lookup::Id(uc_id))
        .await
    {
        audit.record_create(Model::UserCompany, &r).await;
    }
    mz_account::new_employee_default::apply_new_employee_manual_time_default(
        &mut tx, audit, company_id, user_id, None,
    )
    .await?;
    let ual_id: i32 = sqlx::query_scalar(
        r#"INSERT INTO "userAccessLevel" ("userId", "companyId", "accessLevelId", status)
           VALUES ($1, $2, $3, 1) RETURNING id"#,
    )
    .bind(user_id)
    .bind(company_id)
    .bind(role.id)
    .fetch_one(&mut *tx)
    .await?;
    if let Some(r) = audit
        .result_row(&mut *tx, Model::UserAccessLevel, Lookup::Id(ual_id))
        .await
    {
        audit.record_create(Model::UserAccessLevel, &r).await;
    }
    let after = json!({
        "name": account.name,
        "email": account.email,
        "role": dto.role,
        "companyUuid": c_uuid.to_string(),
        "passwordGenerated": account.generated.is_some(),
    });
    record(
        &mut *tx,
        ctx,
        &OperatorAuditEntry {
            operator_id: op.id,
            action: "company.user_create",
            target_type: "user",
            target_uuid: Some(user_uuid),
            company_id: Some(company_id),
            before: None,
            after: Some(&after),
            reason: Some(&dto.reason),
        },
    )
    .await?;
    tx.commit().await?;
    Ok((c_name, user_uuid))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn errs(r: ApiResult<impl std::fmt::Debug>) -> Vec<String> {
        match r {
            Err(ApiError::Validation(m)) => m,
            _ => Vec::new(),
        }
    }

    #[test]
    fn create_company_messages() {
        let e = errs(validate_create_company(&json!({
            "companyName": "  ", "timeZone": "Mars/Base", "modules": ["X", "X"],
            "pricingUuid": "nope", "trialEndDate": "2026-02-30", "owner": { "name": "A", "email": "BAD", "extra": 1 },
            "reason": "valid reason"
        })));
        assert_eq!(
            e,
            vec![
                "companyName should not be empty",
                "timeZone must be a valid IANA time-zone",
                "each value in modules must be one of the following values: TIME_TRACKING, SALES_TRACKER",
                "All modules's elements must be unique",
                "pricingUuid must be a UUID",
                "trialEndDate must be a valid ISO 8601 date string",
                "owner.property extra should not exist",
                "owner.email must be an email",
            ]
        );
        let e = errs(validate_create_company(&json!({ "owner": "x" })));
        assert!(e.contains(&"nested property owner must be either object or array".to_owned()));
        assert!(e.contains(&"modules must be an array".to_owned()));
    }

    #[test]
    fn create_company_transforms() {
        let ok = validate_create_company(&json!({
            "companyName": "  PT Contoh ", "timeZone": "Asia/Jakarta", "modules": ["TIME_TRACKING"],
            "pricingUuid": "3f2c1a9e-8b7d-4c6e-9a1b-2d3e4f5a6b7c", "trialEndDate": "2030-01-01",
            "owner": { "name": " Budi ", "email": "  Budi@Example.COM " }, "reason": "  buat company  "
        }))
        .ok();
        let ok = ok.as_ref();
        assert_eq!(ok.map(|d| d.company_name.as_str()), Some("PT Contoh"));
        assert_eq!(
            ok.and_then(|d| d.owner.as_ref()).map(|o| o.email.as_str()),
            Some("budi@example.com")
        );
        assert_eq!(ok.map(|d| d.reason.as_str()), Some("buat company"));
    }

    #[test]
    fn create_user_order() {
        let e = errs(validate_create_user(
            &json!({ "role": "SUPER_ADMIN", "password": "short" }),
        ));
        assert_eq!(
            e,
            vec![
                "role must be one of the following values: ADMIN, MANAGER, STAFF, GUEST",
                "reason must be longer than or equal to 5 characters",
                "reason must be a string",
                "name must be shorter than or equal to 255 characters",
                "name should not be empty",
                "name must be a string",
                "email must be shorter than or equal to 255 characters",
                "email must be an email",
                "password must be longer than or equal to 8 characters",
            ]
        );
    }
}
