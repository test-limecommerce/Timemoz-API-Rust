//! `AuthService.register` (`auth.service.ts:1031`) + langkah pembuatan company
//! yang dipakai bersama `registerWithGoogle`.

use chrono::{Duration, Months, NaiveDateTime, Utc};
use mz_account::assignable_role::{
    AccessLevel, AssignableRoleActor, RoleRef, resolve_assignable_role,
};
use mz_account::company_response::to_company_response;
use mz_account::customer_types::{DEFAULT_CUSTOMER_TYPES, seed_default_customer_types};
use mz_account::password::hash_password;
use mz_account::signup_tracking::{
    SELF_SIGNUP, SignUpEventInput, SignUpPlan, build_sign_up_properties,
    normalize_signup_attribution,
};
use mz_account::uoms::{DEFAULT_UOMS, seed_default_uoms};
use mz_account::username_conflict::is_username_conflict_api;
use mz_account::util::env_or_undefined;
use mz_account::visit_masters::{
    DEFAULT_VISIT_FREQUENCIES, DEFAULT_VISIT_TYPES, seed_default_visit_masters,
};
use mz_audit::{AuditHandle, Lookup, Model, Row};
use mz_core::enums::PricingProductType;
use mz_core::jwt::sign;
use mz_core::time::{JsDate, now_naive};
use mz_core::user_status::{STATUS_ACTIVE, STATUS_INVITED};
use mz_core::{ApiError, ApiResult, AppState};
use mz_paypal::{CreateSubscriptionPayload, PaypalClient, get_approval_url};
use serde_json::{Map, Value, json};
use sqlx::PgConnection;

use crate::analytics::{DiscordNewUser, emit_sign_up, spawn_discord_new_user};
use crate::dto::{RegisterDto, text};
use crate::js;
use crate::rowjson::row_to_map;
use crate::service::confirm_url;
use crate::service::invite::generate_unique_invite_code;

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// Baris `pricingTiers` yang dipakai registrasi.
#[derive(Debug, sqlx::FromRow)]
pub struct Tier {
    pub id: i32,
    pub uuid: String,
    pub name: String,
    #[sqlx(rename = "minUsers")]
    pub min_users: i32,
    #[sqlx(rename = "maxUsers")]
    pub max_users: i32,
    #[sqlx(rename = "basePrice")]
    pub base_price: f64,
    #[sqlx(rename = "pricePerUserUsd")]
    pub price_per_user_usd: f64,
    #[sqlx(rename = "billingInterval")]
    pub billing_interval: String,
    #[sqlx(rename = "productType")]
    pub product_type: PricingProductType,
    #[sqlx(rename = "paypalPlanId")]
    pub paypal_plan_id: Option<String>,
    #[sqlx(rename = "maxProject")]
    pub max_project: i32,
    #[sqlx(rename = "useIdleDetection")]
    pub use_idle_detection: Option<bool>,
    #[sqlx(rename = "useCheatDetection")]
    pub use_cheat_detection: Option<bool>,
    #[sqlx(rename = "useReports")]
    pub use_reports: Option<bool>,
    #[sqlx(rename = "useTimesheetApproval")]
    pub use_timesheet_approval: Option<bool>,
    #[sqlx(rename = "useActivityLevel")]
    pub use_activity_level: Option<bool>,
    #[sqlx(rename = "useIntegrations")]
    pub use_integrations: Option<bool>,
}

/// `!(x > 0)` (NaN ikut dianggap tidak positif, sama dengan JS).
pub fn not_positive(x: f64) -> bool {
    x.is_nan() || x <= 0.0
}

pub const TIER_COLUMNS: &str = r#"id, uuid::text AS "uuid", name, "minUsers", "maxUsers", "basePrice",
    "pricePerUserUsd", "billingInterval", "productType"::text AS "productType", "paypalPlanId", "maxProject",
    "useIdleDetection", "useCheatDetection", "useReports", "useTimesheetApproval", "useActivityLevel",
    "useIntegrations""#;

impl Tier {
    /// `tier.minUsers || 1`.
    pub fn initial_quantity(&self) -> i32 {
        if self.min_users == 0 {
            1
        } else {
            self.min_users
        }
    }
    /// `tier.billingInterval || 'MONTH'`.
    pub fn billing_interval(&self) -> &str {
        if self.billing_interval.is_empty() {
            "MONTH"
        } else {
            &self.billing_interval
        }
    }
    /// `isZeroPricedTier`.
    pub fn is_zero_priced(&self) -> bool {
        not_positive(self.base_price) && not_positive(self.price_per_user_usd)
    }
    /// `productTypeToModuleFlags` → (time tracking, sales tracker).
    pub fn module_flags(&self) -> (bool, bool) {
        (
            self.product_type != PricingProductType::SALES,
            self.product_type != PricingProductType::TIME,
        )
    }
    /// SALES = IDR, paket lain USD.
    pub fn currency(&self) -> &'static str {
        if self.product_type == PricingProductType::SALES {
            "IDR"
        } else {
            "USD"
        }
    }
}

/// `freeSubscriptionEndDate(from, fallback)`.
pub fn free_subscription_end(
    state: &AppState,
    from: NaiveDateTime,
    fallback: NaiveDateTime,
) -> NaiveDateTime {
    match state.config.free_subscription_days {
        Some(days) => from + Duration::milliseconds(days * DAY_MS),
        None => fallback,
    }
}

/// `parseInt(process.env.IDLETIME_IN_SECOND, 10)`; `NaN` ditolak Prisma → error.
pub fn idle_time_in_second(state: &AppState) -> ApiResult<i32> {
    js::parse_int(state.config.var("IDLETIME_IN_SECOND").as_deref())
        .and_then(|v| i32::try_from(v).ok())
        .ok_or_else(|| {
            ApiError::internal(anyhow::anyhow!(
                "Invalid value for argument `idleTimeInSecond`: NaN is not a valid Int"
            ))
        })
}

/// Pesan `err.message || err` untuk respons `register failed`.
pub fn error_message(err: &ApiError) -> String {
    match err {
        ApiError::Message { message, .. } | ApiError::Plain { message, .. } => message.clone(),
        ApiError::Body { body, .. } => body
            .get("message")
            .and_then(Value::as_str)
            .map_or_else(|| body.to_string(), str::to_owned),
        ApiError::UniqueViolation(m) | ApiError::BadJson(m) => m.clone(),
        ApiError::Validation(m) => m.join(","),
        ApiError::Bare(s) => s.canonical_reason().unwrap_or_default().to_owned(),
        ApiError::Internal(e) => e.to_string(),
    }
}

pub async fn find_tier(state: &AppState, uuid: Option<&str>) -> ApiResult<Option<Tier>> {
    // Prisma `where: { uuid: undefined }` = tanpa filter.
    let sql = match uuid {
        Some(_) => format!(
            "SELECT {TIER_COLUMNS} FROM \"pricingTiers\" WHERE uuid = $1::uuid ORDER BY id LIMIT 1"
        ),
        None => format!("SELECT {TIER_COLUMNS} FROM \"pricingTiers\" ORDER BY id LIMIT 1"),
    };
    let mut q = sqlx::query_as::<_, Tier>(&sql);
    if let Some(u) = uuid {
        q = q.bind(u);
    }
    Ok(q.fetch_optional(&state.db).await?)
}

pub async fn username_exists(state: &AppState, username: &str) -> ApiResult<bool> {
    let id: Option<i32> =
        sqlx::query_scalar("SELECT id FROM users WHERE username = $1 ORDER BY id LIMIT 1")
            .bind(username)
            .fetch_optional(&state.db)
            .await?;
    Ok(id.is_some())
}

/// Kolom company baru (`tx.company.create`).
pub struct NewCompany<'a> {
    pub owner_user_id: i32,
    pub address: &'a str,
    pub telp: &'a str,
    pub register_name: &'a str,
    pub register_email: &'a str,
    pub register_roles: &'a str,
    pub register_total_employees: &'a str,
    pub tier: &'a Tier,
    pub is_trial: bool,
    pub invite_code: &'a str,
}

/// INSERT company → seluruh kolom skalar (bentuk Prisma).
pub async fn insert_company(
    conn: &mut PgConnection,
    c: &NewCompany<'_>,
) -> ApiResult<Map<String, Value>> {
    let (time_tracking, sales_tracker) = c.tier.module_flags();
    // Audit `company` create dicatat pemanggil: bentuk hasil Prisma-nya beda per jalur
    // (register email tanpa `select`, register Google `select: { id, uuid }`).
    let row = sqlx::query(
        r#"INSERT INTO company ("ownerUserId", name, address, telp, register_name, register_email, register_roles,
               register_total_employees, status, "maxUsers", "maxProject", "useIdleDetection", "useCheatDetection",
               "useReports", "useTimesheetApproval", "useActivityLevel", "useIntegrations", "useActivityTrack",
               "isTrial", "hasCreateOrgnanization", "inviteCode", currency, "moduleTimeTrackingEnabled",
               "moduleSalesTrackerEnabled")
           VALUES ($1, 'DEFAULT', $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, true,
               $17, false, $18, $19, $20, $21)
           RETURNING *"#,
    )
    .bind(c.owner_user_id)
    .bind(c.address)
    .bind(c.telp)
    .bind(c.register_name)
    .bind(c.register_email)
    .bind(c.register_roles)
    .bind(c.register_total_employees)
    .bind(STATUS_ACTIVE)
    .bind(c.tier.max_users)
    .bind(c.tier.max_project)
    .bind(c.tier.use_idle_detection)
    .bind(c.tier.use_cheat_detection)
    .bind(c.tier.use_reports)
    .bind(c.tier.use_timesheet_approval)
    .bind(c.tier.use_activity_level)
    .bind(c.tier.use_integrations)
    .bind(c.is_trial)
    .bind(c.invite_code)
    .bind(c.tier.currency())
    .bind(time_tracking)
    .bind(sales_tracker)
    .fetch_one(&mut *conn)
    .await?;
    Ok(row_to_map(&row)?)
}

/// `DEFAULT_LEAVE_TYPES` (`leave-types.service.ts`): code, name, deductBalance,
/// balanceSource, requiresDocument, annualQuota (semua `isDefault: true`).
/// (code, name, deductBalance, balanceSource, requiresDocument, annualQuota).
pub type LeaveTypeRow = (
    &'static str,
    &'static str,
    bool,
    &'static str,
    bool,
    Option<i32>,
);

pub const DEFAULT_LEAVE_TYPES: [LeaveTypeRow; 6] = [
    ("ANNUAL_LEAVE", "Annual Leave", true, "ANNUAL", false, None),
    ("SICK_LEAVE", "Sick Leave", true, "SICK", true, None),
    (
        "MARRIAGE_LEAVE",
        "Marriage Leave",
        false,
        "ANNUAL",
        true,
        Some(3),
    ),
    (
        "MATERNITY_LEAVE",
        "Maternity Leave",
        false,
        "ANNUAL",
        true,
        None,
    ),
    (
        "BEREAVEMENT_LEAVE",
        "Bereavement Leave",
        false,
        "ANNUAL",
        false,
        None,
    ),
    ("UNPAID_LEAVE", "Unpaid Leave", false, "ANNUAL", false, None),
];

/// Master data & project bawaan company baru, urutan sama dengan Node: leave type,
/// customer type, UOM, visit master, leave policy, project DEF + anggota.
pub async fn seed_company_defaults(
    conn: &mut PgConnection,
    audit: &AuditHandle,
    company_id: i32,
    user_id: i32,
    project_name: &str,
) -> ApiResult<()> {
    let codes: Vec<&str> = DEFAULT_LEAVE_TYPES.iter().map(|t| t.0).collect();
    let names: Vec<&str> = DEFAULT_LEAVE_TYPES.iter().map(|t| t.1).collect();
    let deducts: Vec<bool> = DEFAULT_LEAVE_TYPES.iter().map(|t| t.2).collect();
    let sources: Vec<&str> = DEFAULT_LEAVE_TYPES.iter().map(|t| t.3).collect();
    let docs: Vec<bool> = DEFAULT_LEAVE_TYPES.iter().map(|t| t.4).collect();
    let quotas: Vec<Option<i32>> = DEFAULT_LEAVE_TYPES.iter().map(|t| t.5).collect();
    sqlx::query(
        r#"INSERT INTO "companyLeaveType" ("companyId", code, name, "deductBalance", "balanceSource",
               "requiresDocument", "annualQuota", "isDefault")
           SELECT $1, t.code, t.name, t.deduct, t.src::"LeaveBalanceSource", t.doc, t.quota, true
           FROM UNNEST($2::text[], $3::text[], $4::bool[], $5::text[], $6::bool[], $7::int4[])
                AS t(code, name, deduct, src, doc, quota)
           ON CONFLICT DO NOTHING"#,
    )
    .bind(company_id)
    .bind(&codes)
    .bind(&names)
    .bind(&deducts)
    .bind(&sources)
    .bind(&docs)
    .bind(&quotas)
    .execute(&mut *conn)
    .await?;

    // Ketiga master ber-audit: `createMany` entitas utama = satu baris ringkasan (`bulk`).
    let count = seed_default_customer_types(conn, company_id, user_id).await?;
    let rows: Vec<Row> = DEFAULT_CUSTOMER_TYPES
        .iter()
        .map(|t| {
            Row::new()
                .with("companyId", company_id)
                .with("code", t.code)
                .with("name", t.name)
                .with("createdById", user_id)
        })
        .collect();
    audit
        .record_create_many(Model::CustomerType, &rows, count)
        .await;
    let count = seed_default_uoms(conn, company_id, user_id).await?;
    let rows: Vec<Row> = DEFAULT_UOMS
        .iter()
        .map(|u| {
            Row::new()
                .with("companyId", company_id)
                .with("code", u.code)
                .with("name", u.name)
                .with("uomType", u.uom_type)
                .with("defaultOperator", "multiply")
                .with("defaultFactor", u.default_factor)
                .with("createdById", user_id)
        })
        .collect();
    audit.record_create_many(Model::Uom, &rows, count).await;
    let (types, frequencies) = seed_default_visit_masters(conn, company_id, user_id).await?;
    let rows: Vec<Row> = DEFAULT_VISIT_TYPES
        .iter()
        .map(|t| {
            Row::new()
                .with("companyId", company_id)
                .with("code", t.code)
                .with("name", t.name)
                .with("sortOrder", t.sort_order)
                .with("createdById", user_id)
                .with("requiresOrder", t.requires_order)
                .with("requiresPhoto", t.requires_photo)
                .with("requiresGps", t.requires_gps)
                .with("isDefault", t.is_default)
        })
        .collect();
    audit
        .record_create_many(Model::VisitType, &rows, types)
        .await;
    let rows: Vec<Row> = DEFAULT_VISIT_FREQUENCIES
        .iter()
        .map(|f| {
            Row::new()
                .with("companyId", company_id)
                .with("code", f.code)
                .with("name", f.name)
                .with("sortOrder", f.sort_order)
                .with("intervalDays", f.interval_days)
                .with("createdById", user_id)
        })
        .collect();
    audit
        .record_create_many(Model::VisitFrequency, &rows, frequencies)
        .await;

    sqlx::query(
        r#"INSERT INTO "leavePolicy" ("companyId", "annualLeaveDays", "sickLeaveDays", "sickLeaveMode",
               "carryOverEnabled", "maxCarryOverDays", "carryOverExpiryMonth", "resetType", "entitlementType")
           VALUES ($1, 12, NULL, 'UNLIMITED'::"LeaveSickMode", false, NULL, NULL, 'ANNUAL'::"LeaveResetType",
               'UNIFORM'::"LeaveEntitlementType")"#,
    )
    .bind(company_id)
    .execute(&mut *conn)
    .await?;

    let project_type: Option<i32> = sqlx::query_scalar(
        r#"SELECT id FROM "projectType" WHERE code = 'ROLLING_HOURS' ORDER BY id LIMIT 1"#,
    )
    .fetch_optional(&mut *conn)
    .await?;
    let project_type = project_type.ok_or_else(|| {
        ApiError::internal(anyhow::anyhow!(
            "Cannot read properties of null (reading 'id')"
        ))
    })?;
    let project_id: i32 = sqlx::query_scalar(
        r#"INSERT INTO project (code, name, "screenshotPerHour", "totalHour", "projectTypeId", "startDate", status)
           VALUES ('DEF', $1, 3, 0, $2, $3, 1) RETURNING id"#,
    )
    .bind(project_name)
    .bind(project_type)
    .bind(now_naive().date())
    .fetch_one(&mut *conn)
    .await?;
    sqlx::query(r#"INSERT INTO "projectUser" ("userId", "projectId", status) VALUES ($1, $2, 1)"#)
        .bind(user_id)
        .bind(project_id)
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        r#"INSERT INTO "projectCompany" ("companyId", "projectId", status) VALUES ($1, $2, 1)"#,
    )
    .bind(company_id)
    .bind(project_id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Baris `subscriptions` baru.
pub struct NewSubscription<'a> {
    pub company_id: i32,
    pub tier_id: i32,
    pub start: NaiveDateTime,
    pub end: NaiveDateTime,
    pub status: &'a str,
    pub quantity: i32,
    pub price_per_user: f64,
    pub billing_interval: &'a str,
    pub paypal_plan_id: Option<&'a str>,
    pub paypal_subscription_id: Option<&'a str>,
    pub paypal_status: Option<&'a str>,
}

pub async fn insert_subscription(
    conn: &mut PgConnection,
    s: &NewSubscription<'_>,
) -> ApiResult<i32> {
    Ok(sqlx::query_scalar(
        r#"INSERT INTO subscriptions ("companyId", "pricingTierId", "startDate", "endDate", status, quantity,
               "pricePerUser", "billingInterval", "paypalPlanId", "paypalSubscriptionId", "paypalStatus")
           VALUES ($1, $2, $3, $4, $5::"SubscriptionStatus", $6, $7, $8, $9, $10, $11) RETURNING id"#,
    )
    .bind(s.company_id)
    .bind(s.tier_id)
    .bind(s.start)
    .bind(s.end)
    .bind(s.status)
    .bind(s.quantity)
    .bind(s.price_per_user)
    .bind(s.billing_interval)
    .bind(s.paypal_plan_id)
    .bind(s.paypal_subscription_id)
    .bind(s.paypal_status)
    .fetch_one(&mut *conn)
    .await?)
}

/// `userCompany` default + `userAccessLevel` SUPER_ADMIN pendaftar mandiri.
pub async fn attach_owner(
    conn: &mut PgConnection,
    audit: &AuditHandle,
    user_id: i32,
    company_id: i32,
    idle_time: i32,
) -> ApiResult<AccessLevel> {
    let membership_id: i32 = sqlx::query_scalar(
        r#"INSERT INTO "userCompany" ("userId", "companyId", status, "isDefault", "idleTimeInSecond")
           VALUES ($1, $2, $3, true, $4) RETURNING id"#,
    )
    .bind(user_id)
    .bind(company_id)
    .bind(STATUS_ACTIVE)
    .bind(idle_time)
    .fetch_one(&mut *conn)
    .await?;
    // Audit `userCompany` create (hasil tanpa `select` = semua kolom).
    if let Some(row) = audit
        .result_row(&mut *conn, Model::UserCompany, Lookup::Id(membership_id))
        .await
    {
        audit.record_create(Model::UserCompany, &row).await;
    }
    // Pendaftaran mandiri: tidak ada pelaku untuk dibandingkan tingkatnya.
    let access_level = resolve_assignable_role(
        conn,
        &AssignableRoleActor::SelfRegistration,
        company_id,
        RoleRef {
            code: Some("SUPER_ADMIN"),
            uuid: None,
        },
    )
    .await?;
    sqlx::query(r#"INSERT INTO "userAccessLevel" ("userId", "accessLevelId", "companyId", status) VALUES ($1, $2, $3, $4)"#)
        .bind(user_id)
        .bind(access_level.id)
        .bind(company_id)
        .bind(STATUS_ACTIVE)
        .execute(&mut *conn)
        .await?;
    // Audit `userAccessLevel` create: allowlist (accessLevelId, companyId, status) + userId induk.
    audit
        .record_create(
            Model::UserAccessLevel,
            &user_access_level_row(user_id, access_level.id, company_id, STATUS_ACTIVE),
        )
        .await;
    Ok(access_level)
}

/// Hasil `userAccessLevel.create` tanpa `select`, sebatas kolom yang dibaca audit.
pub fn user_access_level_row(
    user_id: i32,
    access_level_id: i32,
    company_id: i32,
    status: i32,
) -> Row {
    Row::new()
        .with("userId", user_id)
        .with("accessLevelId", access_level_id)
        .with("companyId", company_id)
        .with("status", status)
}

/// Hasil `users.create({ select: { id, uuid, username, name } })`.
pub fn created_user_row(id: i32, uuid: &str, username: &str, name: &str) -> Row {
    Row::new()
        .with("id", id)
        .with("uuid", uuid)
        .with("username", username)
        .with("name", name)
}

pub fn company_str(company: &Map<String, Value>, key: &str) -> String {
    company
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

pub fn company_id_of(company: &Map<String, Value>) -> i32 {
    company
        .get("id")
        .and_then(Value::as_i64)
        .and_then(|v| i32::try_from(v).ok())
        .unwrap_or(0)
}

/// Deskripsi item invoice register berbayar.
pub fn invoice_item_description(
    billing_interval: &str,
    quantity: i32,
    price_per_user: f64,
) -> String {
    let yearly = billing_interval == "YEAR";
    format!(
        "{} Subscription - {quantity} user(s) @ ${}/user{}",
        if yearly { "Yearly" } else { "Monthly" },
        js::num_string(price_per_user),
        if yearly { "/month (billed yearly)" } else { "" }
    )
}

struct Payment {
    invoice_number: String,
    payment_url: Option<String>,
    paypal_id: Option<String>,
    paypal_status: Option<String>,
    sub_price: f64,
    tax_price: f64,
    unit_price_idr: f64,
    start: NaiveDateTime,
    end: NaiveDateTime,
    json: Value,
}

/// Langkah PayPal `register` berbayar (sebelum transaksi, sama dengan Node).
async fn create_payment(
    state: &AppState,
    tier: &Tier,
    username: &str,
    name: &str,
    invoice_number: String,
) -> ApiResult<Payment> {
    let billing_interval = tier.billing_interval();
    let quantity = tier.initial_quantity();
    let multiplier: i32 = if billing_interval == "YEAR" { 12 } else { 1 };
    let price_per_user = tier.price_per_user_usd;
    let sub_price_usd = f64::from(quantity) * price_per_user * f64::from(multiplier);
    let tax_price = 0.0;
    let unit_price_idr = tier.base_price * f64::from(multiplier);
    let sub_price = unit_price_idr * f64::from(quantity);
    let start = now_naive();
    let end = if billing_interval == "YEAR" {
        start.checked_add_months(Months::new(12)).unwrap_or(start)
    } else {
        start + Duration::days(30)
    };

    let plan_id = tier.paypal_plan_id.as_deref().unwrap_or_default();
    let subscription = PaypalClient::from_state(state)
        .create_subscription(&CreateSubscriptionPayload {
            plan_id: plan_id.to_owned(),
            subscriber_email: username.to_owned(),
            subscriber_name: name.to_owned(),
            custom_id: Some("company-pending".to_owned()),
            return_url: format!(
                "{}?invoice={invoice_number}",
                env_or_undefined(state.config.var("PAYPAL_RETURN_URL"))
            ),
            cancel_url: format!(
                "{}?invoice={invoice_number}",
                env_or_undefined(state.config.var("PAYPAL_CANCEL_URL"))
            ),
            quantity: Some(i64::from(quantity)),
            start_time: None,
        })
        .await
        .map_err(ApiError::internal)?;
    let payment_url = get_approval_url(&subscription);
    let paypal_id = subscription
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let paypal_status = subscription
        .get("status")
        .and_then(Value::as_str)
        .map(str::to_owned);

    let mut json = Map::new();
    json.insert("invoiceNumber".into(), json!(invoice_number));
    json.insert("paymentUrl".into(), json!(payment_url));
    if let Some(id) = subscription.get("id") {
        json.insert("paypalSubscriptionId".into(), id.clone());
    }
    json.insert("quantity".into(), json!(quantity));
    json.insert("pricePerUserUsd".into(), js::num(price_per_user));
    json.insert("billingInterval".into(), json!(billing_interval));
    json.insert("billingMultiplier".into(), json!(multiplier));
    json.insert("subPriceUsd".into(), js::num(sub_price_usd));
    json.insert("subPrice".into(), js::num(sub_price));
    json.insert("taxPrice".into(), js::num(tax_price));
    json.insert("totalPrice".into(), js::num(sub_price + tax_price));
    json.insert("subscriptionStartDate".into(), json!(JsDate(start)));
    json.insert("subscriptionEndDate".into(), json!(JsDate(end)));
    json.insert("unitPriceIdr".into(), js::num(unit_price_idr));

    Ok(Payment {
        invoice_number,
        payment_url,
        paypal_id,
        paypal_status,
        sub_price,
        tax_price,
        unit_price_idr,
        start,
        end,
        json: Value::Object(json),
    })
}

fn failure(message: &str) -> Value {
    json!({ "success": false, "message": message, "data": null })
}

/// `AuthService.register(dto)` — semua galat menjadi `{ success:false, ... }` (HTTP 200).
pub async fn register(state: &AppState, audit: &AuditHandle, dto: &RegisterDto) -> Value {
    match register_inner(state, audit, dto).await {
        Ok(v) => v,
        Err(err) if is_username_conflict_api(&err) => failure("Username already exists"),
        Err(err) => {
            tracing::error!(error = %err, "register failed");
            json!({ "success": false, "message": "register failed", "error": error_message(&err) })
        }
    }
}

async fn register_inner(
    state: &AppState,
    audit: &AuditHandle,
    dto: &RegisterDto,
) -> ApiResult<Value> {
    let now_utc = Utc::now();
    let attribution =
        normalize_signup_attribution(dto.attribution.as_ref().unwrap_or(&Value::Null), now_utc);
    let username = text(&dto.username).to_lowercase();
    let name = text(&dto.name);
    let is_trial = dto.is_trial();

    if username_exists(state, &username).await? {
        return Ok(failure("Username already exists"));
    }
    let Some(tier) = find_tier(state, Some(text(&dto.pricing_uuid))).await? else {
        return Ok(failure("Pricing not found"));
    };
    if !is_trial {
        if tier.paypal_plan_id.as_deref().is_none_or(str::is_empty) {
            return Ok(failure("PayPal plan not configured for this pricing tier"));
        }
        if not_positive(tier.price_per_user_usd) {
            return Ok(failure(
                "Price per user not configured for this pricing tier",
            ));
        }
    }

    let now = now_naive();
    let paid_trial_end = now + Duration::days(14);
    // Trial berbayar tetap 14 hari; paket gratis mengikuti `FREE_SUBSCRIPTION_DAYS`.
    let trial_end = if tier.is_zero_priced() {
        free_subscription_end(state, now, paid_trial_end)
    } else {
        paid_trial_end
    };
    let invoice_number = mz_core::invoice::generate_invoice_number(now_utc);
    let quantity = tier.initial_quantity();
    let billing_interval = tier.billing_interval().to_owned();
    let hashed_password = hash_password(state, text(&dto.password)).await?;

    let payment = if is_trial {
        None
    } else {
        Some(create_payment(state, &tier, &username, name, invoice_number).await?)
    };

    let invite_code = generate_unique_invite_code(state).await?;
    let idle_time = idle_time_in_second(state);
    let company_name_label = js::template(dto.company_name.as_ref());

    let mut tx = state.db.begin().await?;
    let (user_id, user_uuid, user_name): (i32, String, String) = sqlx::query_as(
        r#"INSERT INTO users (name, username, "createdAt", status, password, "salesOnboardingEmailEligible")
           VALUES ($1, $2, $3, $4, $5, $6) RETURNING id, uuid::text, name"#,
    )
    .bind(name)
    .bind(&username)
    .bind(now)
    .bind(STATUS_INVITED)
    .bind(&hashed_password)
    .bind(tier.product_type == PricingProductType::SALES)
    .fetch_one(&mut *tx)
    .await?;
    audit
        .record_create(
            Model::Users,
            &created_user_row(user_id, &user_uuid, &username, &user_name),
        )
        .await;

    let company = insert_company(
        &mut tx,
        &NewCompany {
            owner_user_id: user_id,
            address: js::or_str(dto.address.as_ref(), ""),
            telp: js::or_str(dto.handphone.as_ref(), ""),
            register_name: name,
            register_email: &username,
            register_roles: js::or_str(dto.roles.as_ref(), "Owner"),
            register_total_employees: js::or_str(dto.total_employees.as_ref(), "1-10"),
            tier: &tier,
            is_trial: true,
            invite_code: &invite_code,
        },
    )
    .await?;
    let company_id = company_id_of(&company);
    // `company.create` tanpa `select` di jalur ini: hasilnya semua kolom.
    if let Some(row) = audit
        .result_row(&mut *tx, Model::Company, Lookup::Id(company_id))
        .await
    {
        audit.record_create(Model::Company, &row).await;
    }
    seed_company_defaults(
        &mut tx,
        audit,
        company_id,
        user_id,
        &format!("{company_name_label} Project"),
    )
    .await?;

    sqlx::query(r#"INSERT INTO trials ("companyId", "startDate", "endDate") VALUES ($1, $2, $3)"#)
        .bind(company_id)
        .bind(now)
        .bind(trial_end)
        .execute(&mut *tx)
        .await?;

    match &payment {
        None => {
            insert_subscription(
                &mut tx,
                &NewSubscription {
                    company_id,
                    tier_id: tier.id,
                    start: now,
                    end: trial_end,
                    status: "TRIAL",
                    quantity,
                    price_per_user: tier.price_per_user_usd,
                    billing_interval: &billing_interval,
                    paypal_plan_id: tier.paypal_plan_id.as_deref(),
                    paypal_subscription_id: None,
                    paypal_status: None,
                },
            )
            .await?;
        }
        Some(p) => {
            let subscription_id = insert_subscription(
                &mut tx,
                &NewSubscription {
                    company_id,
                    tier_id: tier.id,
                    start: p.start,
                    end: p.end,
                    status: "PENDING",
                    quantity,
                    price_per_user: tier.price_per_user_usd,
                    billing_interval: &billing_interval,
                    paypal_plan_id: tier.paypal_plan_id.as_deref(),
                    paypal_subscription_id: p.paypal_id.as_deref(),
                    paypal_status: p.paypal_status.as_deref(),
                },
            )
            .await?;
            let invoice_id: i32 = sqlx::query_scalar(
                r#"INSERT INTO invoices ("subscriptionId", "invoiceNumber", "paymentUrl", "subPrice", "taxPrice",
                       "totalPrice", "expiredAt", "paymentProvider")
                   VALUES ($1, $2, $3, $4, $5, $6, $7, 'paypal') RETURNING id"#,
            )
            .bind(subscription_id)
            .bind(&p.invoice_number)
            .bind(p.payment_url.as_deref().unwrap_or_default())
            .bind(p.sub_price)
            .bind(p.tax_price)
            .bind(p.sub_price + p.tax_price)
            .bind(now_naive() + Duration::days(3))
            .fetch_one(&mut *tx)
            .await?;
            sqlx::query(
                r#"INSERT INTO "invoiceItems" ("invoiceId", description, quantity, "unitPrice", "totalPrice")
                   VALUES ($1, $2, $3, $4, $5)"#,
            )
            .bind(invoice_id)
            .bind(invoice_item_description(&billing_interval, quantity, tier.price_per_user_usd))
            .bind(f64::from(quantity))
            .bind(p.unit_price_idr)
            .bind(p.sub_price)
            .execute(&mut *tx)
            .await?;
        }
    }

    let access_level = attach_owner(&mut tx, audit, user_id, company_id, idle_time?).await?;
    tx.commit().await?;

    let hashed_uuid = hash_password(state, &user_uuid).await?;
    let access_token = {
        let mut payload = Map::new();
        payload.insert("userId".into(), json!(user_id));
        sign(state, payload)?
    };

    let url = confirm_url(state, &username, &hashed_uuid);
    {
        let (state, email, name) = (state.clone(), username.clone(), user_name.clone());
        mz_core::bg::spawn(async move {
            let r = mz_account::mail::send_account_confirmation(&state, &email, &name, &url).await;
            if !r.success {
                tracing::error!("sendAccountConfirmation failed");
            }
        });
    }
    spawn_admin_register_info(state, &user_name, &username);

    let company_uuid = company_str(&company, "uuid");
    let company_label = company_str(&company, "name");
    let sign_up = build_sign_up_properties(&SignUpEventInput {
        signup_type: SELF_SIGNUP,
        signup_method: "email",
        signup_platform: "web".into(),
        product_type: tier.product_type,
        role: "SUPER_ADMIN".into(),
        company_uuid,
        is_workspace_owner: true,
        plan: Some(SignUpPlan {
            uuid: tier.uuid.clone(),
            name: tier.name.clone(),
            billing_interval: billing_interval.clone(),
            is_trial,
        }),
        invited_by_uuid: None,
        attribution: Some(attribution),
        occurred_at: now.and_utc(),
    });
    emit_sign_up(state, &user_uuid, sign_up.clone(), now.and_utc());
    spawn_discord_new_user(
        state,
        DiscordNewUser {
            name: user_name.clone(),
            email: username.clone(),
            company_name: company_label,
            registration_date: mz_core::time::to_js_iso(&Utc::now()),
            sign_up,
        },
    );

    let mut data = Map::new();
    data.insert(
        "user".into(),
        json!({ "id": user_id, "uuid": user_uuid, "username": username, "name": user_name }),
    );
    data.insert(
        "company".into(),
        to_company_response(Value::Object(company)),
    );
    data.insert(
        "accessLevel".into(),
        serde_json::to_value(&access_level).map_err(ApiError::internal)?,
    );
    data.insert("accessToken".into(), json!(access_token));
    data.insert("token".into(), json!(hashed_uuid));
    if let Some(p) = payment {
        data.insert("paymentUrl".into(), json!(p.payment_url));
        data.insert("payment".into(), p.json);
    }
    Ok(json!({ "success": true, "message": "register success", "data": data }))
}

/// `mail.sendAdminRegisterInfo(...)` tanpa ditunggu.
pub fn spawn_admin_register_info(state: &AppState, name: &str, email: &str) {
    let (state, name, email) = (state.clone(), name.to_owned(), email.to_owned());
    mz_core::bg::spawn(async move {
        let date = mz_core::time::to_js_iso(&Utc::now());
        let r = mz_account::mail::send_admin_register_info(&state, &name, &email, &date).await;
        if !r.success {
            tracing::error!("sendAdminRegisterInfo failed");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tier(product_type: PricingProductType, base: f64, per_user: f64) -> Tier {
        Tier {
            id: 1,
            uuid: "u".into(),
            name: "Free".into(),
            min_users: 0,
            max_users: 5,
            base_price: base,
            price_per_user_usd: per_user,
            billing_interval: String::new(),
            product_type,
            paypal_plan_id: None,
            max_project: 1,
            use_idle_detection: None,
            use_cheat_detection: None,
            use_reports: None,
            use_timesheet_approval: None,
            use_activity_level: None,
            use_integrations: None,
        }
    }

    #[test]
    fn tier_rules_match_node() {
        let t = tier(PricingProductType::SALES, 0.0, 0.0);
        assert_eq!(t.initial_quantity(), 1);
        assert_eq!(t.billing_interval(), "MONTH");
        assert!(t.is_zero_priced());
        assert_eq!(t.module_flags(), (false, true));
        assert_eq!(t.currency(), "IDR");
        let t = tier(PricingProductType::TIME, 0.0, 5.0);
        assert!(!t.is_zero_priced());
        assert_eq!(t.module_flags(), (true, false));
        assert_eq!(t.currency(), "USD");
        assert_eq!(
            tier(PricingProductType::FULL, 1.0, 0.0).module_flags(),
            (true, true)
        );
    }

    #[test]
    fn invoice_texts() {
        assert_eq!(
            invoice_item_description("MONTH", 3, 5.0),
            "Monthly Subscription - 3 user(s) @ $5/user"
        );
        assert_eq!(
            invoice_item_description("YEAR", 1, 4.5),
            "Yearly Subscription - 1 user(s) @ $4.5/user/month (billed yearly)"
        );
    }

    #[tokio::test]
    async fn free_days_fallback() {
        let Ok(state) = AppState::for_test() else {
            return;
        };
        let now = now_naive();
        let fallback = now + Duration::days(14);
        assert_eq!(free_subscription_end(&state, now, fallback), fallback);
    }

    #[test]
    fn error_messages() {
        assert_eq!(
            error_message(&ApiError::unprocessable("Access Level 'X' not found")),
            "Access Level 'X' not found"
        );
        assert_eq!(
            error_message(&ApiError::coded(
                axum::http::StatusCode::CONFLICT,
                "SEAT",
                "full"
            )),
            "full"
        );
    }
}
