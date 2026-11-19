//! Baris tabel dibaca sebagai `to_jsonb(row)` (kolom camelCase persis seperti
//! hasil Prisma), lalu bentuk nilainya disamakan dengan serialisasi Prisma:
//! kolom tanggal menjadi `2026-11-16T08:00:00.000Z`, `Decimal` menjadi string.

use chrono::{NaiveDate, NaiveDateTime, NaiveTime};
use mz_core::time::JsDate;
use serde_json::{Map, Value};

/// Nama kolom `DateTime`/`Date`/`Time` di model yang dibaca crate ini.
const DATE_KEYS: &[&str] = &[
    "createdAt",
    "updatedAt",
    "startDate",
    "cycleStartAt",
    "thresholdAlertSentAt",
    "budgetReachedSentAt",
    "lastResetAt",
    "reportExportsResetAt",
    "jiraTokenExpiresAt",
    "googleCalendarTokenExpiresAt",
    "googleCalendarWebhookExpiry",
    "dailyReportSentAt",
];

/// Kolom `Decimal` pada `company` (Prisma → string).
const DECIMAL_KEYS: &[&str] = &["maxOrderDiscountPercent"];

/// Kolom list (`String[]`/`Int[]`): Prisma memetakan NULL menjadi `[]`.
const LIST_KEYS: &[&str] = &["imageBlurRoles"];

/// Urutan field model Prisma (`prisma/schema.prisma`): Prisma/Node mengeluarkan kolom skalar
/// menurut urutan field model, bukan urutan kolom fisik tabel dan bukan urutan kunci jsonb
/// (jsonb mengurutkan menurut panjang lalu abjad). Lihat [`order`].
pub const PROJECT_KEYS: &[&str] = &[
    "id",
    "uuid",
    "code",
    "name",
    "screenshotPerHour",
    "projectTypeId",
    "startDate",
    "createdAt",
    "updatedAt",
    "status",
    "totalHour",
    "billable",
    "clickupListId",
];

/// Model `projectBudget` (urutan field `schema.prisma`; relasi tidak dipilih).
pub const PROJECT_BUDGET_KEYS: &[&str] = &[
    "id",
    "uuid",
    "projectId",
    "isEnabled",
    "budgetType",
    "billableRate",
    "budgetAmount",
    "countNonBillable",
    "currentBurnedCost",
    "budgetHours",
    "resetCycle",
    "alertThreshold",
    "enforcement",
    "cycleStartAt",
    "currentBurnedSeconds",
    "thresholdAlertSentAt",
    "budgetReachedSentAt",
    "lastResetAt",
    "createdAt",
    "updatedAt",
];

/// Model `projectType`.
pub const PROJECT_TYPE_KEYS: &[&str] = &[
    "id",
    "uuid",
    "code",
    "name",
    "createdAt",
    "updatedAt",
    "status",
];

/// Model `projectHour`.
pub const PROJECT_HOUR_KEYS: &[&str] = &[
    "id",
    "uuid",
    "projectId",
    "totalHour",
    "createdAt",
    "updatedAt",
];

/// Kolom skalar model `company`.
pub const COMPANY_KEYS: &[&str] = &[
    "id",
    "uuid",
    "name",
    "telp",
    "address",
    "websiteURL",
    "register_name",
    "register_email",
    "register_roles",
    "register_total_employees",
    "timeZone",
    "currency",
    "workweekStartDay",
    "dailyReportSentAt",
    "isAlllowDeleteScreenshot",
    "fixedHourNotification",
    "isAgencyMode",
    "isTrial",
    "maxUsers",
    "maxProject",
    "useIdleDetection",
    "useCheatDetection",
    "useReports",
    "useTimesheetApproval",
    "useActivityLevel",
    "useIntegrations",
    "screenshotResolution",
    "screenshotRetentionDays",
    "reportExportsPerMonth",
    "reportExportsUsedThisMonth",
    "reportExportsResetAt",
    "dataHistoryMonths",
    "budgetAlertDeliveryChannel",
    "clientProofExportAllowed",
    "createdAt",
    "updatedAt",
    "status",
    "actualProfilePictureName",
    "profilePictureFullPath",
    "hasCreateOrgnanization",
    "inviteCode",
    "dontChargeInvite",
    "clickupApiToken",
    "clickupWorkspaceId",
    "jiraAccessToken",
    "jiraRefreshToken",
    "jiraCloudId",
    "jiraSiteUrl",
    "jiraTokenExpiresAt",
    "jiraWebhookId",
    "jiraWebhookSecret",
    "discordGuildId",
    "discordGuildName",
    "discordDefaultTaskChannelId",
    "discordDefaultTaskChannelName",
    "googleCalendarAccessToken",
    "googleCalendarRefreshToken",
    "googleCalendarTokenExpiresAt",
    "googleCalendarEmail",
    "googleCalendarSyncToken",
    "googleCalendarWebhookChannelId",
    "googleCalendarWebhookResourceId",
    "googleCalendarWebhookExpiry",
    "mobileRequireSelfie",
    "mobileBlockMockLocation",
    "locationHistorySyncIntervalMinutes",
    "isSupportOfflineMode",
    "useActivityTrack",
    "imageBlurEnabled",
    "imageBlurRoles",
    "storeVisitDefaultRadiusMeters",
    "storeVisitMaxAccuracyMeters",
    "storeVisitBlockMockLocation",
    "storeVisitRequirePhotos",
    "storeVisitAllowOverride",
    "storeVisitMaxDurationHours",
    "storeVisitEnforceGroupScope",
    "storeVisitMinDurationMinutes",
    "storeVisitCountsAsAttendance",
    "storeVisitFormReviseMode",
    "manualTimeApprovalMode",
    "manualTimeAutoApproveLimitMinutes",
    "manualTimeAutoApprovePeriod",
    "manualTimeAutoApproveNewEmployees",
    "divisionsEnabled",
    "moduleTimeTrackingEnabled",
    "moduleSalesTrackerEnabled",
    "customRolesEnabled",
    "maxOrderDiscountPercent",
    "catalogEpoch",
    "ownerUserId",
];

/// Susun ulang kunci objek: `keys` yang ada lebih dulu menurut urutan `keys`, sisanya (kolom
/// DB yang belum ada di daftar) mengikuti urutan semula. Bukan objek → dibiarkan.
pub fn order(v: &mut Value, keys: &[&str]) {
    if let Value::Object(m) = v {
        let mut old = std::mem::take(m);
        for k in keys {
            // `shift_remove`: `remove` pada `preserve_order` = `swap_remove` (mengacak urutan).
            if let Some(val) = old.shift_remove(*k) {
                m.insert((*k).to_owned(), val);
            }
        }
        for (k, val) in old {
            m.insert(k, val);
        }
    }
}

/// [`order`] untuk tiap elemen array (non-array dibiarkan).
pub fn order_each(v: &mut Value, keys: &[&str]) {
    if let Value::Array(a) = v {
        for item in a {
            order(item, keys);
        }
    }
}

/// Teks tanggal Postgres (`to_jsonb`) → bentuk `Date.toJSON()`.
pub fn js_date(s: &str) -> Option<String> {
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Some(format!("{d}T00:00:00.000Z"));
    }
    if let Ok(dt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f") {
        return Some(JsDate(dt).to_js_string());
    }
    if let Ok(t) = NaiveTime::parse_from_str(s, "%H:%M:%S%.f") {
        let dt = NaiveDate::from_ymd_opt(1970, 1, 1)?.and_time(t);
        return Some(JsDate(dt).to_js_string());
    }
    None
}

/// Rapikan seluruh pohon JSON (rekursif berdasarkan nama kunci).
pub fn fix(v: &mut Value) {
    match v {
        Value::Object(m) => {
            for (k, val) in m.iter_mut() {
                if val.is_null() && LIST_KEYS.contains(&k.as_str()) {
                    *val = Value::Array(Vec::new());
                    continue;
                }
                if DATE_KEYS.contains(&k.as_str()) {
                    if let Value::String(s) = val {
                        if let Some(n) = js_date(s) {
                            *s = n;
                        }
                        continue;
                    }
                } else if DECIMAL_KEYS.contains(&k.as_str())
                    && let Value::Number(n) = val
                    && let Some(f) = n.as_f64()
                {
                    *val = Value::String(crate::jsfmt::num_string(f));
                    continue;
                }
                fix(val);
            }
        }
        Value::Array(a) => a.iter_mut().for_each(fix),
        _ => {}
    }
}

/// `toCompanyResponse`: buang kolom yang tidak boleh keluar.
pub fn company_response(mut v: Value) -> Value {
    if let Value::Object(m) = &mut v {
        // `shift_remove`: `remove` pada `preserve_order` = `swap_remove` (mengacak urutan).
        m.shift_remove("profilePictureFullPath");
    }
    order(&mut v, COMPANY_KEYS);
    fix(&mut v);
    v
}

/// Sisipkan/timpa kunci pada objek JSON (`{...row, extra}`).
pub fn merge(base: &mut Value, extra: Vec<(&str, Value)>) {
    if let Value::Object(m) = base {
        for (k, v) in extra {
            m.insert(k.to_owned(), v);
        }
    }
}

pub fn obj(pairs: Vec<(&str, Value)>) -> Value {
    let mut m = Map::new();
    for (k, v) in pairs {
        m.insert(k.to_owned(), v);
    }
    Value::Object(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn null_list_column_becomes_empty_array() {
        let v = company_response(json!({"imageBlurRoles": null, "name": null}));
        assert_eq!(v["imageBlurRoles"], json!([]));
        assert_eq!(v["name"], Value::Null);
    }

    #[test]
    fn dates_are_normalised() {
        let mut v = json!({
            "createdAt": "2026-11-16T08:00:00.5",
            "startDate": "2026-11-16",
            "dailyReportSentAt": "00:00:00",
            "nested": [{ "updatedAt": "2026-11-16T08:00:00" }],
            "name": "2026-11-16"
        });
        fix(&mut v);
        assert_eq!(v["createdAt"], "2026-11-16T08:00:00.500Z");
        assert_eq!(v["startDate"], "2026-11-16T00:00:00.000Z");
        assert_eq!(v["dailyReportSentAt"], "1970-01-01T00:00:00.000Z");
        assert_eq!(v["nested"][0]["updatedAt"], "2026-11-16T08:00:00.000Z");
        assert_eq!(v["name"], "2026-11-16");
    }

    #[test]
    fn order_follows_prisma_field_order_then_unknown_columns() {
        // Urutan jsonb Postgres: panjang kunci lalu abjad.
        let mut v = json!({
            "id": 1, "code": "A", "uuid": "u", "name": "n", "status": 1, "extra": 2, "billable": true
        });
        order(&mut v, PROJECT_KEYS);
        let keys: Vec<&str> = v
            .as_object()
            .map_or(vec![], |m| m.keys().map(String::as_str).collect());
        assert_eq!(
            keys,
            ["id", "uuid", "code", "name", "status", "billable", "extra"]
        );
        assert_eq!(
            serde_json::to_string(&v).unwrap_or_default(),
            r#"{"id":1,"uuid":"u","code":"A","name":"n","status":1,"billable":true,"extra":2}"#
        );
    }

    #[test]
    fn company_response_is_in_schema_order() {
        let v = company_response(json!({
            "id": 1, "name": "n", "uuid": "u", "profilePictureFullPath": "x", "telp": "t", "status": 1
        }));
        let keys: Vec<&str> = v
            .as_object()
            .map_or(vec![], |m| m.keys().map(String::as_str).collect());
        assert_eq!(keys, ["id", "uuid", "name", "telp", "status"]);
    }

    #[test]
    fn company_hides_logo_and_stringifies_decimal() {
        let v = company_response(json!({
            "profilePictureFullPath": "x", "maxOrderDiscountPercent": 10.5, "name": "A"
        }));
        assert!(v.get("profilePictureFullPath").is_none());
        assert_eq!(v["maxOrderDiscountPercent"], "10.5");
    }
}
