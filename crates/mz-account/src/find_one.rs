//! Port `UsersService.findOne` (`src/users/users.service.ts:931`) — isi `GET /auth/me`.
//!
//! `None` = Node mengembalikan `null` (user tidak ada / tidak aktif / tanpa keanggotaan aktif).
//! Query dipecah per tabel (bukan satu join raksasa) dan dirakit jadi `serde_json::Value`
//! dengan bentuk identik dengan hasil Prisma + penyusunan di service.

use mz_core::enums::{BaseRole, effective_role_code};
use mz_core::time::JsDate;
use mz_core::user_status::STATUS_ACTIVE;
use mz_core::{ApiResult, AppState};
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::invite_method::to_invite_method;
use crate::plan_boost::apply_plan_boosts;
use crate::role_summary::{RoleSummaryRow, role_summary_of};

#[derive(sqlx::FromRow)]
struct UserBase {
    id: i32,
    username: String,
    name: String,
    uuid: String,
    status: i32,
    #[sqlx(rename = "createdAt")]
    created_at: JsDate,
    #[sqlx(rename = "updatedAt")]
    updated_at: Option<JsDate>,
    #[sqlx(rename = "sentAt")]
    sent_at: Option<JsDate>,
}

/// `userCompany` aktif utama JOIN `company` (kolom yang di-`select` Node).
#[derive(sqlx::FromRow)]
struct MembershipRow {
    #[sqlx(rename = "isBlur")]
    is_blur: bool,
    #[sqlx(rename = "isScreenshotActive")]
    is_screenshot_active: bool,
    #[sqlx(rename = "idleTimeInSecond")]
    idle_time_in_second: i32,
    #[sqlx(rename = "screenshotPerHour")]
    screenshot_per_hour: i32,
    #[sqlx(rename = "isAllowEditTime")]
    is_allow_edit_time: bool,
    #[sqlx(rename = "isAllowManualTime")]
    is_allow_manual_time: bool,
    #[sqlx(rename = "inviteMethod")]
    invite_method: Option<String>,
    #[sqlx(rename = "inviterUuid")]
    inviter_uuid: Option<String>,
    #[sqlx(rename = "inviterName")]
    inviter_name: Option<String>,
    #[sqlx(rename = "inviterUsername")]
    inviter_username: Option<String>,
    #[sqlx(rename = "companyId")]
    company_id: i32,
    #[sqlx(rename = "companyUuid")]
    company_uuid: String,
    #[sqlx(rename = "companyName")]
    company_name: String,
    #[sqlx(rename = "companyStatus")]
    company_status: i32,
    #[sqlx(rename = "websiteURL")]
    website_url: Option<String>,
    register_total_employees: Option<String>,
    #[sqlx(rename = "maxUsers")]
    max_users: i32,
    #[sqlx(rename = "isTrial")]
    is_trial: bool,
    #[sqlx(rename = "maxProject")]
    max_project: i32,
    #[sqlx(rename = "screenshotResolution")]
    screenshot_resolution: String,
    #[sqlx(rename = "screenshotRetentionDays")]
    screenshot_retention_days: i32,
    #[sqlx(rename = "reportExportsPerMonth")]
    report_exports_per_month: i32,
    #[sqlx(rename = "reportExportsUsedThisMonth")]
    report_exports_used_this_month: i32,
    #[sqlx(rename = "reportExportsResetAt")]
    report_exports_reset_at: Option<JsDate>,
    #[sqlx(rename = "dataHistoryMonths")]
    data_history_months: i32,
    #[sqlx(rename = "budgetAlertDeliveryChannel")]
    budget_alert_delivery_channel: String,
    #[sqlx(rename = "clientProofExportAllowed")]
    client_proof_export_allowed: bool,
    #[sqlx(rename = "useCheatDetection")]
    use_cheat_detection: Option<bool>,
    #[sqlx(rename = "hasCreateOrgnanization")]
    has_create_organization: bool,
    #[sqlx(rename = "moduleTimeTrackingEnabled")]
    module_time_tracking_enabled: bool,
    #[sqlx(rename = "moduleSalesTrackerEnabled")]
    module_sales_tracker_enabled: bool,
    #[sqlx(rename = "useActivityTrack")]
    use_activity_track: bool,
    #[sqlx(rename = "divisionsEnabled")]
    divisions_enabled: bool,
}

const MEMBERSHIP_SQL: &str = r#"
    SELECT uc."isBlur", uc."isScreenshotActive", uc."idleTimeInSecond", uc."screenshotPerHour",
           uc."isAllowEditTime", uc."isAllowManualTime", uc."inviteMethod",
           inv.uuid::text AS "inviterUuid", inv.name AS "inviterName", inv.username AS "inviterUsername",
           c.id AS "companyId", c.uuid::text AS "companyUuid", c.name AS "companyName", c.status AS "companyStatus",
           c."websiteURL", c.register_total_employees, c."maxUsers", c."isTrial", c."maxProject",
           c."screenshotResolution", c."screenshotRetentionDays", c."reportExportsPerMonth",
           c."reportExportsUsedThisMonth", c."reportExportsResetAt", c."dataHistoryMonths",
           c."budgetAlertDeliveryChannel"::text AS "budgetAlertDeliveryChannel", c."clientProofExportAllowed",
           c."useCheatDetection", c."hasCreateOrgnanization", c."moduleTimeTrackingEnabled",
           c."moduleSalesTrackerEnabled", c."useActivityTrack", c."divisionsEnabled"
    FROM "userCompany" uc
    JOIN company c ON c.id = uc."companyId"
    LEFT JOIN users inv ON inv.id = uc."invitedById"
    WHERE uc."userId" = $1 AND uc.status = 1
    ORDER BY uc."isDefault" DESC, uc.id ASC
    LIMIT 1"#;

#[derive(sqlx::FromRow)]
struct AccessRow {
    #[sqlx(rename = "companyId")]
    company_id: i32,
    uuid: String,
    code: String,
    name: String,
    #[sqlx(rename = "isSystem")]
    is_system: bool,
    #[sqlx(rename = "baseRole")]
    base_role: Option<BaseRole>,
}

#[derive(sqlx::FromRow)]
struct SubscriptionRow {
    uuid: String,
    #[sqlx(rename = "startDate")]
    start_date: JsDate,
    #[sqlx(rename = "endDate")]
    end_date: JsDate,
    status: String,
    #[sqlx(rename = "tierUuid")]
    tier_uuid: Option<String>,
    #[sqlx(rename = "tierName")]
    tier_name: Option<String>,
    #[sqlx(rename = "tierProductType")]
    tier_product_type: Option<String>,
    #[sqlx(rename = "maxStores")]
    max_stores: Option<i32>,
    #[sqlx(rename = "totalGeofence")]
    total_geofence: Option<i32>,
    #[sqlx(rename = "auditLog")]
    audit_log: Option<bool>,
    form: Option<bool>,
    #[sqlx(rename = "timelineRoute")]
    timeline_route: Option<bool>,
    product: Option<bool>,
    #[sqlx(rename = "order")]
    order_gate: Option<bool>,
    divisions: Option<bool>,
    #[sqlx(rename = "maxAutoCheckIns")]
    max_auto_check_ins: Option<i32>,
    #[sqlx(rename = "maxFormTemplates")]
    max_form_templates: Option<i32>,
    #[sqlx(rename = "visitHistoryDays")]
    visit_history_days: Option<i32>,
}

#[derive(sqlx::FromRow, Serialize)]
struct TrialRow {
    #[serde(rename = "startDate")]
    #[sqlx(rename = "startDate")]
    start_date: JsDate,
    #[serde(rename = "endDate")]
    #[sqlx(rename = "endDate")]
    end_date: JsDate,
}

fn pricing_tier_json(sub: &SubscriptionRow, boosts: &[String], company: &MembershipRow) -> Value {
    let mut tier = Map::new();
    if let Some(tier_uuid) = &sub.tier_uuid {
        tier.insert("uuid".into(), json!(tier_uuid));
        tier.insert("name".into(), json!(sub.tier_name));
        tier.insert("productType".into(), json!(sub.tier_product_type));
        tier.insert("maxStores".into(), json!(sub.max_stores));
        tier.insert("totalGeofence".into(), json!(sub.total_geofence));
        tier.insert("auditLog".into(), json!(sub.audit_log));
        tier.insert("form".into(), json!(sub.form));
        tier.insert("timelineRoute".into(), json!(sub.timeline_route));
        tier.insert("product".into(), json!(sub.product));
        tier.insert("order".into(), json!(sub.order_gate));
        tier.insert("divisions".into(), json!(sub.divisions));
        tier.insert("maxAutoCheckIns".into(), json!(sub.max_auto_check_ins));
        tier.insert("maxFormTemplates".into(), json!(sub.max_form_templates));
        tier.insert("visitHistoryDays".into(), json!(sub.visit_history_days));
        apply_plan_boosts(&mut tier, boosts);
    }
    // Batas yang dibaca dari kolom company (menimpa kolom paket bernama sama).
    for (k, v) in [
        ("maxProject", json!(company.max_project)),
        ("screenshotResolution", json!(company.screenshot_resolution)),
        (
            "screenshotRetentionDays",
            json!(company.screenshot_retention_days),
        ),
        (
            "reportExportsPerMonth",
            json!(company.report_exports_per_month),
        ),
        (
            "reportExportsUsedThisMonth",
            json!(company.report_exports_used_this_month),
        ),
        (
            "reportExportsResetAt",
            json!(company.report_exports_reset_at),
        ),
        ("dataHistoryMonths", json!(company.data_history_months)),
        (
            "budgetAlertDeliveryChannel",
            json!(company.budget_alert_delivery_channel),
        ),
        (
            "clientProofExportAllowed",
            json!(company.client_proof_export_allowed),
        ),
        ("useIdleDetection", json!(true)),
        (
            "useCheatDetection",
            json!(company.use_cheat_detection.unwrap_or(false)),
        ),
        ("useActivityLevel", json!(true)),
    ] {
        tier.insert(k.into(), v);
    }
    Value::Object(tier)
}

/// `findOne(id)`.
pub async fn find_one(state: &AppState, id: i32) -> ApiResult<Option<Value>> {
    let db = &state.db;

    let user = sqlx::query_as::<_, UserBase>(
        r#"SELECT id, username, name, uuid::text AS "uuid", status, "createdAt", "updatedAt", "sentAt"
           FROM users WHERE id = $1"#,
    )
    .bind(id)
    .fetch_optional(db)
    .await?;
    let Some(user) = user.filter(|u| u.status == STATUS_ACTIVE) else {
        return Ok(None);
    };

    let Some(membership) = sqlx::query_as::<_, MembershipRow>(MEMBERSHIP_SQL)
        .bind(id)
        .fetch_optional(db)
        .await?
    else {
        return Ok(None);
    };

    // `userAccessLevel` take 1 orderBy id desc — TANPA filter company/status (apa adanya Node).
    let access = sqlx::query_as::<_, AccessRow>(
        r#"SELECT ual."companyId", al.uuid::text AS "uuid", al.code, al.name,
                  al."isSystem", al."baseRole"::text AS "baseRole"
           FROM "userAccessLevel" ual JOIN "accessLevel" al ON al.id = ual."accessLevelId"
           WHERE ual."userId" = $1 ORDER BY ual.id DESC LIMIT 1"#,
    )
    .bind(id)
    .fetch_optional(db)
    .await?;
    let access_code = access
        .as_ref()
        .map(|a| effective_role_code(&a.code, a.is_system, a.base_role));

    let mut is_owner = false;
    // Daftar putih satu nilai: hanya role bawaan SUPER_ADMIN.
    if let (Some(access), Some("SUPER_ADMIN")) = (&access, access_code.as_deref()) {
        let owner_user_id: Option<Option<i32>> =
            sqlx::query_scalar(r#"SELECT "ownerUserId" FROM company WHERE id = $1"#)
                .bind(access.company_id)
                .fetch_optional(db)
                .await?;
        if let Some(Some(owner)) = owner_user_id {
            // spec 007 R-24: `company.ownerUserId` didahulukan.
            is_owner = owner == id;
        } else {
            let first: Option<i32> = sqlx::query_scalar(
                r#"SELECT "userId" FROM "userCompany" WHERE "companyId" = $1 ORDER BY "createdAt" ASC LIMIT 1"#,
            )
            .bind(access.company_id)
            .fetch_optional(db)
            .await?;
            is_owner = first == Some(id);
        }
    }

    let subs = sqlx::query_as::<_, SubscriptionRow>(
        r#"SELECT s.uuid::text AS "uuid", s."startDate", s."endDate", s.status::text AS "status",
                  pt.uuid::text AS "tierUuid", pt.name AS "tierName", pt."productType"::text AS "tierProductType",
                  pt."maxStores", pt."totalGeofence", pt."auditLog", pt.form, pt."timelineRoute", pt.product,
                  pt."order", pt.divisions, pt."maxAutoCheckIns", pt."maxFormTemplates", pt."visitHistoryDays"
           FROM subscriptions s LEFT JOIN "pricingTiers" pt ON pt.id = s."pricingTierId"
           WHERE s."companyId" = $1 AND s.status = 'ACTIVE' ORDER BY s.id LIMIT 1"#,
    )
    .bind(membership.company_id)
    .fetch_all(db)
    .await?;
    let boosts: Vec<String> = sqlx::query_scalar(
        r#"SELECT product::text FROM "companyPlanBoost"
           WHERE "companyId" = $1 AND enabled = true AND product::text IN ('SALES', 'TIME') ORDER BY id"#,
    )
    .bind(membership.company_id)
    .fetch_all(db)
    .await?;
    let trial = sqlx::query_as::<_, TrialRow>(
        r#"SELECT "startDate", "endDate" FROM trials WHERE "companyId" = $1 ORDER BY id"#,
    )
    .bind(membership.company_id)
    .fetch_all(db)
    .await?;

    let subscriptions: Vec<Value> = subs
        .iter()
        .map(|s| {
            json!({
                "uuid": s.uuid,
                "startDate": s.start_date,
                "endDate": s.end_date,
                "pricingTier": pricing_tier_json(s, &boosts, &membership),
                "status": s.status,
            })
        })
        .collect();

    let company = json!({
        "id": membership.company_id,
        "uuid": membership.company_uuid,
        "name": membership.company_name,
        "status": membership.company_status,
        "websiteURL": membership.website_url,
        "register_total_employees": membership.register_total_employees,
        "maxUsers": membership.max_users,
        "isTrial": membership.is_trial,
        "moduleTimeTrackingEnabled": membership.module_time_tracking_enabled,
        "moduleSalesTrackerEnabled": membership.module_sales_tracker_enabled,
        "useActivityTrack": membership.use_activity_track,
        "divisionsEnabled": membership.divisions_enabled,
        "trial": trial,
        "subscriptions": subscriptions,
        "screenshotResolution": membership.screenshot_resolution,
        "isBlur": membership.is_blur,
        "screenshotPerHour": membership.screenshot_per_hour,
        "idleTimeInSecond": membership.idle_time_in_second,
        "allowTakeScreenshot": membership.is_screenshot_active,
        "isAllowEditTime": membership.is_allow_edit_time,
        "isAllowManualTime": membership.is_allow_manual_time,
    });

    // `effectiveRoleCodeOfRow(...) || 'USER'`
    let access_level = access_code
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| "USER".to_owned());
    let role = access.as_ref().map(|a| RoleSummaryRow {
        uuid: a.uuid.clone(),
        code: a.code.clone(),
        name: a.name.clone(),
        is_system: a.is_system,
        base_role: a.base_role,
    });

    let invited_by = membership.inviter_uuid.as_ref().map(|uuid| {
        json!({ "uuid": uuid, "name": membership.inviter_name, "email": membership.inviter_username })
    });

    Ok(Some(json!({
        "id": user.id,
        "username": user.username,
        "name": user.name,
        "uuid": user.uuid,
        "status": user.status,
        "createdAt": user.created_at,
        "updatedAt": user.updated_at,
        "sentAt": user.sent_at,
        "hasCreateOrganization": membership.has_create_organization,
        "accessLevel": access_level,
        "role": role_summary_of(role.as_ref()),
        "company": company,
        "is_owner": is_owner,
        "invited_by": invited_by,
        "invite_method": to_invite_method(membership.invite_method.as_deref()),
        "screenshotQuota": { "used": 0, "max": 9_999_999, "remaining": 9_999_999 },
    })))
}
