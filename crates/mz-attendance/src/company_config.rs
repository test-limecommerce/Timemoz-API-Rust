//! Port `src/mobile-company-config/*`: `GET /mobile/company/config` dan proxy logo bertanda
//! tangan `GET /v1/mobile/companies/:companyUuid/logo` (`company-logo-file.ts`).

use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::NaiveTime;
use mz_core::enums::PricingProductType;
use mz_core::feature_modules::flags_to_modules;
use mz_core::jsfmt::num;
use mz_core::{ApiError, ApiResult, AppState};
use serde_json::{Value, json};

use crate::clock::DEFAULT_POLICY;

/// `mobileCompanyLogoPath(companyUuid)`.
pub fn logo_path(company_uuid: &str) -> String {
    format!(
        "/v1/mobile/companies/{}/logo",
        mz_files::encode::encode_uri_component(company_uuid)
    )
}

/// `companyLogoKey(company)`: key yang boleh disajikan, wajib di folder `companies/<uuid>/`.
pub fn company_logo_key(
    state: &AppState,
    company_uuid: &str,
    full_path: Option<&str>,
) -> Option<String> {
    let full_path = full_path.filter(|p| !p.is_empty())?;
    let key = mz_files::object_key_of(state, full_path).ok()?;
    logo_key_in_folder(company_uuid, key)
}

fn logo_key_in_folder(company_uuid: &str, key: String) -> Option<String> {
    let prefix = format!("companies/{}/", company_uuid.to_lowercase());
    // `key.slice(0, prefix.length + 1)` dalam unit UTF-16; prefix ASCII sehingga potongan karakter
    // sudah cukup.
    let head: String = key
        .chars()
        .take(prefix.chars().count() + 1)
        .collect::<String>()
        .to_lowercase();
    (head.starts_with(&prefix) || head.starts_with(&format!("/{prefix}"))).then_some(key)
}

#[derive(sqlx::FromRow)]
struct CompanyRow {
    id: i32,
    uuid: String,
    name: String,
    telp: Option<String>,
    address: Option<String>,
    #[sqlx(rename = "websiteURL")]
    website_url: Option<String>,
    #[sqlx(rename = "timeZone")]
    time_zone: String,
    currency: String,
    #[sqlx(rename = "workweekStartDay")]
    workweek_start_day: i32,
    #[sqlx(rename = "dailyReportSentAt")]
    daily_report_sent_at: Option<NaiveTime>,
    #[sqlx(rename = "profilePictureFullPath")]
    profile_picture_full_path: Option<String>,
    #[sqlx(rename = "isAgencyMode")]
    is_agency_mode: bool,
    #[sqlx(rename = "isTrial")]
    is_trial: bool,
    #[sqlx(rename = "mobileRequireSelfie")]
    mobile_require_selfie: bool,
    #[sqlx(rename = "mobileBlockMockLocation")]
    mobile_block_mock_location: bool,
    #[sqlx(rename = "isSupportOfflineMode")]
    is_support_offline_mode: bool,
    #[sqlx(rename = "storeVisitDefaultRadiusMeters")]
    sv_default_radius: i32,
    #[sqlx(rename = "storeVisitMaxAccuracyMeters")]
    sv_max_accuracy: i32,
    #[sqlx(rename = "storeVisitBlockMockLocation")]
    sv_block_mock: bool,
    #[sqlx(rename = "storeVisitRequirePhotos")]
    sv_require_photos: bool,
    #[sqlx(rename = "storeVisitAllowOverride")]
    sv_allow_override: bool,
    #[sqlx(rename = "storeVisitMaxDurationHours")]
    sv_max_duration_hours: i32,
    #[sqlx(rename = "storeVisitMinDurationMinutes")]
    sv_min_duration_minutes: i32,
    #[sqlx(rename = "storeVisitEnforceGroupScope")]
    sv_enforce_group_scope: bool,
    #[sqlx(rename = "storeVisitCountsAsAttendance")]
    sv_counts_as_attendance: bool,
    #[sqlx(rename = "moduleTimeTrackingEnabled")]
    module_time_tracking: bool,
    #[sqlx(rename = "moduleSalesTrackerEnabled")]
    module_sales_tracker: bool,
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
    #[sqlx(rename = "isAlllowDeleteScreenshot")]
    allow_delete_screenshot: bool,
    #[sqlx(rename = "imageBlurEnabled")]
    image_blur_enabled: bool,
    #[sqlx(rename = "imageBlurRoles")]
    image_blur_roles: Vec<String>,
    #[sqlx(rename = "clientProofExportAllowed")]
    client_proof_export_allowed: bool,
    #[sqlx(rename = "maxUsers")]
    max_users: i32,
    #[sqlx(rename = "maxProject")]
    max_project: i32,
    #[sqlx(rename = "screenshotResolution")]
    screenshot_resolution: String,
    #[sqlx(rename = "screenshotRetentionDays")]
    screenshot_retention_days: i32,
    #[sqlx(rename = "dataHistoryMonths")]
    data_history_months: i32,
    #[sqlx(rename = "reportExportsPerMonth")]
    report_exports_per_month: i32,
    #[sqlx(rename = "reportExportsUsedThisMonth")]
    report_exports_used_this_month: i32,
    #[sqlx(rename = "fixedHourNotification")]
    fixed_hour_notification: i32,
    #[sqlx(rename = "budgetAlertDeliveryChannel")]
    budget_alert_delivery_channel: String,
    has_clickup: bool,
    has_jira: bool,
    has_gcal: bool,
    has_discord: bool,
    sales_boost: bool,
}

const COMPANY_SQL: &str = r#"SELECT c.id, c.uuid::text AS uuid, c.name, c.telp, c.address, c."websiteURL",
    c."timeZone", c.currency, c."workweekStartDay", c."dailyReportSentAt", c."profilePictureFullPath",
    c."isAgencyMode", c."isTrial", c."mobileRequireSelfie", c."mobileBlockMockLocation", c."isSupportOfflineMode",
    c."storeVisitDefaultRadiusMeters", c."storeVisitMaxAccuracyMeters", c."storeVisitBlockMockLocation",
    c."storeVisitRequirePhotos", c."storeVisitAllowOverride", c."storeVisitMaxDurationHours",
    c."storeVisitMinDurationMinutes", c."storeVisitEnforceGroupScope", c."storeVisitCountsAsAttendance",
    c."moduleTimeTrackingEnabled", c."moduleSalesTrackerEnabled",
    c."useIdleDetection", c."useCheatDetection", c."useReports", c."useTimesheetApproval",
    c."useActivityLevel", c."useIntegrations", c."isAlllowDeleteScreenshot", c."imageBlurEnabled",
    COALESCE(c."imageBlurRoles", '{}') AS "imageBlurRoles", c."clientProofExportAllowed", c."maxUsers", c."maxProject", c."screenshotResolution",
    c."screenshotRetentionDays", c."dataHistoryMonths", c."reportExportsPerMonth",
    c."reportExportsUsedThisMonth", c."fixedHourNotification",
    c."budgetAlertDeliveryChannel"::text AS "budgetAlertDeliveryChannel",
    c."clickupWorkspaceId" IS NOT NULL AS has_clickup, c."jiraCloudId" IS NOT NULL AS has_jira,
    c."googleCalendarEmail" IS NOT NULL AS has_gcal, c."discordGuildId" IS NOT NULL AS has_discord,
    EXISTS (SELECT 1 FROM "companyPlanBoost" pb WHERE pb."companyId" = c.id
             AND pb.product::text = 'SALES' AND pb.enabled = true) AS sales_boost
  FROM company c WHERE c.id = $1"#;

#[derive(sqlx::FromRow)]
struct TierRow {
    uuid: String,
    name: String,
    #[sqlx(rename = "basePrice")]
    base_price: f64,
    #[sqlx(rename = "extraUserPrice")]
    extra_user_price: f64,
    #[sqlx(rename = "pricePerUserUsd")]
    price_per_user_usd: f64,
    #[sqlx(rename = "billingInterval")]
    billing_interval: String,
    #[sqlx(rename = "productType")]
    product_type: String,
    #[sqlx(rename = "maxStores")]
    max_stores: i32,
    #[sqlx(rename = "totalGeofence")]
    total_geofence: i32,
    #[sqlx(rename = "auditLog")]
    audit_log: bool,
    form: bool,
    #[sqlx(rename = "timelineRoute")]
    timeline_route: bool,
    product: bool,
    order: bool,
    #[sqlx(rename = "maxAutoCheckIns")]
    max_auto_check_ins: i32,
    #[sqlx(rename = "maxFormTemplates")]
    max_form_templates: i32,
    #[sqlx(rename = "visitHistoryDays")]
    visit_history_days: i32,
}

/// `raiseCount`: 0/negatif = tanpa batas.
fn raise_count(current: i32, boost: i32) -> i32 {
    if current <= 0 {
        current
    } else {
        current.max(boost)
    }
}

/// `applySalesBoost(tier, boosted)` (`plan-boost.ts`).
fn apply_sales_boost(t: &mut TierRow) {
    t.max_stores = raise_count(t.max_stores, 100);
    t.total_geofence = raise_count(t.total_geofence, 20);
    t.max_auto_check_ins = raise_count(t.max_auto_check_ins, 5000);
    t.max_form_templates = raise_count(t.max_form_templates, 20);
    t.visit_history_days = raise_count(t.visit_history_days, 365);
    t.audit_log = true;
    t.form = true;
    t.timeline_route = true;
    t.product = true;
    t.order = true;
}

/// `toPricingTier(tier, autoCheckInsUsed)`: FREE bila paket tidak memungut biaya apa pun.
fn pricing_tier(t: &TierRow, auto_check_ins_used: i64) -> Value {
    let paid = t.base_price > 0.0 || t.price_per_user_usd > 0.0;
    let product_type: Value = t
        .product_type
        .parse::<PricingProductType>()
        .map_or_else(|()| Value::String(t.product_type.clone()), |p| json!(p));
    json!({
        "id": t.uuid,
        "name": t.name,
        "type": if paid { "PRO" } else { "FREE" },
        "productType": product_type,
        "basePrice": num(t.base_price),
        "extraUserPrice": num(t.extra_user_price),
        "pricePerUserUsd": num(t.price_per_user_usd),
        "billingInterval": t.billing_interval,
        "maxStores": t.max_stores,
        "totalGeofence": t.total_geofence,
        "auditLog": t.audit_log,
        "form": t.form,
        "timelineRoute": t.timeline_route,
        "product": t.product,
        "order": t.order,
        "maxAutoCheckIns": t.max_auto_check_ins,
        "autoCheckInsUsed": auto_check_ins_used,
        "maxFormTemplates": t.max_form_templates,
        "visitHistoryDays": t.visit_history_days,
    })
}

/// `formatTime`: kolom TIME → `HH:MM:SS`.
fn format_time(t: Option<NaiveTime>) -> String {
    t.map_or_else(
        || "00:00:00".to_owned(),
        |t| t.format("%H:%M:%S").to_string(),
    )
}

/// Node: `MobileCompanyConfigController.getConfig` + `MobileCompanyConfigService.getConfig`.
pub async fn get_config(state: &AppState, user_id: i32) -> ApiResult<Value> {
    let db = &state.db;
    let config = async {
        let company_id = mz_core::company::require_company_id(db, user_id).await?;
        let Some(c) = sqlx::query_as::<_, CompanyRow>(COMPANY_SQL)
            .bind(company_id)
            .fetch_optional(db)
            .await?
        else {
            return Err(ApiError::not_found("Company not found"));
        };
        let (break_policy, leave, tier) = tokio::try_join!(
            async {
                sqlx::query_as::<_, crate::clock::BreakPolicy>(
                    r#"SELECT "maxMinutesLunch", "maxMinutesPrayer", "maxMinutesCoffee",
                              "maxMinutesOther", "countTowardWorkHours"
                         FROM "mobileBreakPolicy" WHERE "companyId" = $1"#,
                )
                .bind(c.id)
                .fetch_optional(db)
                .await
            },
            async {
                sqlx::query_as::<_, (i32, Option<i32>, String, bool, Option<i32>, Option<i32>, String, String)>(
                    r#"SELECT "annualLeaveDays", "sickLeaveDays", "sickLeaveMode"::text, "carryOverEnabled",
                              "maxCarryOverDays", "carryOverExpiryMonth", "resetType"::text,
                              "entitlementType"::text
                         FROM "leavePolicy" WHERE "companyId" = $1"#,
                )
                .bind(c.id)
                .fetch_optional(db)
                .await
            },
            async {
                // `PLAN_SUBSCRIPTION_STATUS_FILTER` + `PLAN_SUBSCRIPTION_ORDER`, `take: 1`.
                sqlx::query_as::<_, TierRow>(
                    r#"SELECT pt.uuid::text AS uuid, pt.name, pt."basePrice", pt."extraUserPrice",
                              pt."pricePerUserUsd", pt."billingInterval", pt."productType"::text AS "productType",
                              pt."maxStores", pt."totalGeofence", pt."auditLog", pt.form, pt."timelineRoute",
                              pt.product, pt."order", pt."maxAutoCheckIns", pt."maxFormTemplates",
                              pt."visitHistoryDays"
                         FROM (SELECT "pricingTierId" FROM subscriptions
                                WHERE "companyId" = $1 AND status::text IN ('ACTIVE', 'TRIAL')
                                ORDER BY "startDate" DESC, id DESC LIMIT 1) s
                         JOIN "pricingTiers" pt ON pt.id = s."pricingTierId""#,
                )
                .bind(c.id)
                .fetch_optional(db)
                .await
            },
        )?;
        let tier = tier.map(|mut t| {
            if c.sales_boost {
                apply_sales_boost(&mut t);
            }
            t
        });
        // Seumur hidup; filter sama dengan `PlanLimit.AUTO_CHECKINS` (`source = 'ping'`).
        let auto_check_ins_used: i64 = if tier.is_some() {
            sqlx::query_scalar(
                r#"SELECT COUNT(*) FROM "storeVisitLocationEvent" e
                     JOIN "storeVisit" v ON v.id = e."visitId"
                    WHERE v."companyId" = $1 AND e.source = 'ping'"#,
            )
            .bind(c.id)
            .fetch_one(db)
            .await?
        } else {
            0
        };
        let logo_url =
            match company_logo_key(state, &c.uuid, c.profile_picture_full_path.as_deref()) {
                Some(_) => {
                    let signed = mz_files::sign_file_path(state, &logo_path(&c.uuid))
                        .map_err(|e| ApiError::internal(anyhow::anyhow!("{e:?}")))?;
                    Value::String(mz_files::public_api_url(state, &signed))
                }
                None => Value::Null,
            };
        let bp = break_policy.unwrap_or(DEFAULT_POLICY);
        let leave = leave.map_or(Value::Null, |l| {
            json!({
                "annualLeaveDays": l.0,
                "sickLeaveDays": l.1,
                "sickLeaveMode": l.2,
                "carryOverEnabled": l.3,
                "maxCarryOverDays": l.4,
                "carryOverExpiryMonth": l.5,
                "resetType": l.6,
                "entitlementType": l.7,
            })
        });
        Ok::<_, ApiError>(json!({
            "company": {
                "id": c.uuid,
                "name": c.name,
                "phone": c.telp,
                "address": c.address,
                "websiteUrl": c.website_url,
                "timeZone": c.time_zone,
                "currency": c.currency,
                "workweekStartDay": c.workweek_start_day,
                "dailyReportSentAt": format_time(c.daily_report_sent_at),
                "logoUrl": logo_url,
                "isAgencyMode": c.is_agency_mode,
                "isTrial": c.is_trial,
                "pricingTier": tier.as_ref().map_or(Value::Null, |t| pricing_tier(t, auto_check_ins_used)),
            },
            "modules": flags_to_modules(c.module_time_tracking, c.module_sales_tracker),
            "attendance": {
                "requireSelfie": c.mobile_require_selfie,
                "blockMockLocation": c.mobile_block_mock_location,
            },
            "break": {
                "maxMinutesLunch": bp.lunch,
                "maxMinutesPrayer": bp.prayer,
                "maxMinutesCoffee": bp.coffee,
                "maxMinutesOther": bp.other,
                "countTowardWorkHours": bp.count_toward_work_hours,
            },
            "storeVisit": {
                "defaultRadiusMeters": c.sv_default_radius,
                "maxAccuracyMeters": c.sv_max_accuracy,
                "blockMockLocation": c.sv_block_mock,
                "requirePhotos": c.sv_require_photos,
                "allowOverride": c.sv_allow_override,
                "maxDurationHours": c.sv_max_duration_hours,
                "minDurationMinutes": c.sv_min_duration_minutes,
                "enforceGroupScope": c.sv_enforce_group_scope,
                "countsAsAttendance": c.sv_counts_as_attendance,
            },
            "leave": leave,
            "features": {
                "useIdleDetection": c.use_idle_detection,
                "useCheatDetection": c.use_cheat_detection,
                "useReports": c.use_reports,
                "useTimesheetApproval": c.use_timesheet_approval,
                "useActivityLevel": c.use_activity_level,
                "useIntegrations": c.use_integrations,
                "allowDeleteScreenshot": c.allow_delete_screenshot,
                "imageBlurEnabled": c.image_blur_enabled,
                "imageBlurRoles": c.image_blur_roles,
                "clientProofExportAllowed": c.client_proof_export_allowed,
                "isSupportOfflineMode": c.is_support_offline_mode,
            },
            "limits": {
                "maxUsers": c.max_users,
                "maxProject": c.max_project,
                "screenshotResolution": c.screenshot_resolution,
                "screenshotRetentionDays": c.screenshot_retention_days,
                "dataHistoryMonths": c.data_history_months,
                "reportExportsPerMonth": c.report_exports_per_month,
                "reportExportsUsedThisMonth": c.report_exports_used_this_month,
                "fixedHourNotification": c.fixed_hour_notification,
                "budgetAlertDeliveryChannel": c.budget_alert_delivery_channel,
            },
            "integrations": {
                "clickup": c.has_clickup,
                "jira": c.has_jira,
                "googleCalendar": c.has_gcal,
                "discord": c.has_discord,
            },
        }))
    };
    let (mut data, lock) = tokio::try_join!(
        config,
        mz_core::subscription_lock::get_state(state, user_id)
    )?;
    if let Some(o) = data.as_object_mut() {
        o.insert(
            "subscriptionLock".into(),
            serde_json::to_value(lock).map_err(ApiError::internal)?,
        );
    }
    Ok(json!({ "data": data }))
}

fn logo_not_found() -> ApiError {
    ApiError::coded(
        StatusCode::NOT_FOUND,
        "COMPANY_LOGO_NOT_FOUND",
        "Logo not found",
    )
}

/// Node: `streamSignedLogo` — semua kegagalan menjawab 404 yang sama; header file tanpa login
/// (`Referrer-Policy`, `X-Robots-Tag`) dipasang SEBELUM pemeriksaan sehingga ikut di 404.
pub async fn stream_logo(
    state: &AppState,
    company_uuid: &str,
    exp: Option<&str>,
    sig: Option<&str>,
    headers: &HeaderMap,
) -> Response {
    let result = async {
        if !mz_files::verify_file_path_signature(state, &logo_path(company_uuid), exp, sig) {
            return Err(logo_not_found());
        }
        let row: Option<(String, Option<String>)> = sqlx::query_as(
            r#"SELECT uuid::text, "profilePictureFullPath" FROM company WHERE uuid = $1::uuid"#,
        )
        .bind(company_uuid)
        .fetch_optional(&state.db)
        .await?;
        let key = row
            .and_then(|(uuid, path)| company_logo_key(state, &uuid, path.as_deref()))
            .ok_or_else(logo_not_found)?;
        let info = mz_files::get_object_info(state, &key).await?;
        let Some(info) = info else {
            return Err(logo_not_found());
        };
        let safe = mz_files::file_response::safe_content_type(info.content_type.as_deref());
        if !safe.inline || !safe.content_type.starts_with("image/") {
            return Err(logo_not_found());
        }
        mz_files::stream_object(
            state,
            &key,
            Some(headers),
            mz_files::StreamObjectOptions {
                cache_control: Some(
                    mz_files::signed_file_path::SIGNED_FILE_CACHE_CONTROL.to_owned(),
                ),
                info: Some(info),
                ..Default::default()
            },
        )
        .await
    }
    .await;
    let mut resp = match result {
        Ok(r) => r,
        Err(e) => e.into_response(),
    };
    mz_files::set_unauthenticated_file_headers(resp.headers_mut());
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logo_key_must_be_in_company_folder() {
        let u = "6F9619FF-8B86-D011-B42D-00CF4FC964FF";
        let ok = |k: &str| logo_key_in_folder(u, k.to_owned());
        assert!(ok("companies/6f9619ff-8b86-d011-b42d-00cf4fc964ff/logo.png").is_some());
        assert!(ok("/companies/6F9619FF-8B86-D011-B42D-00CF4FC964FF/logo.png").is_some());
        assert!(ok("companies/other/logo.png").is_none());
        assert!(ok("screenshots/x.png").is_none());
    }

    #[test]
    fn sales_boost_raises_limits() {
        let mut t = TierRow {
            uuid: "u".into(),
            name: "Free".into(),
            base_price: 0.0,
            extra_user_price: 0.0,
            price_per_user_usd: 0.0,
            billing_interval: "MONTH".into(),
            product_type: "SALES".into(),
            max_stores: 10,
            total_geofence: 0,
            audit_log: false,
            form: false,
            timeline_route: false,
            product: false,
            order: false,
            max_auto_check_ins: 100,
            max_form_templates: 1,
            visit_history_days: 400,
        };
        apply_sales_boost(&mut t);
        assert_eq!(
            (
                t.max_stores,
                t.total_geofence,
                t.max_auto_check_ins,
                t.visit_history_days
            ),
            (100, 0, 5000, 400)
        );
        assert!(t.timeline_route && t.form);
        let v = pricing_tier(&t, 3);
        assert_eq!(v["type"], "FREE");
        assert_eq!(v["basePrice"], 0);
        assert_eq!(v["productType"], "SALES");
    }

    #[test]
    fn time_format() {
        assert_eq!(format_time(None), "00:00:00");
        assert_eq!(format_time(NaiveTime::from_hms_opt(17, 5, 0)), "17:05:00");
    }
}
