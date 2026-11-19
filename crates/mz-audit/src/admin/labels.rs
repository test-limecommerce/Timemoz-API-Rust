//! Port `src/audit-logs/audit-field-labels.ts`: label kolom + terjemahan nilai untuk `diff`
//! (§6.2) dan kata aksi (`actionLabel`). Terjemahan terjadi saat BACA, bukan saat tulis.

use serde_json::{Map, Value, json};

/// `humanizeField`: `isActive` → "Is Active" untuk kolom yang belum terdaftar.
pub fn humanize_field(field: &str) -> String {
    let mut spaced = String::with_capacity(field.len() + 4);
    let mut prev: Option<char> = None;
    for c in field.chars() {
        // `/([a-z0-9])([A-Z])/g` → '$1 $2'.
        if c.is_ascii_uppercase()
            && prev.is_some_and(|p| p.is_ascii_lowercase() || p.is_ascii_digit())
        {
            spaced.push(' ');
        }
        spaced.push(c);
        prev = Some(c);
    }
    // `/^_+/` dibuang.
    let trimmed = spaced.trim_start_matches('_');
    let mut chars = trimmed.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

const COMMON_LABELS: &[(&str, &str)] = &[
    ("name", "Name"),
    ("code", "Code"),
    ("description", "Description"),
    ("address", "Address"),
    ("createdAt", "Created At"),
];

type Labels = &'static [(&'static str, &'static str)];

/// `FIELD_LABELS`, kunci `<entityType>:<subEntity|''>`.
const FIELD_LABELS: &[(&str, Labels)] = &[
    (
        "company:",
        &[
            ("name", "Company Name"),
            ("telp", "Phone"),
            ("address", "Address"),
            ("websiteURL", "Website"),
            ("timeZone", "Time Zone"),
            ("manualTimeApprovalMode", "Manual Time Approval"),
            (
                "manualTimeAutoApproveLimitMinutes",
                "Manual Time Auto-Approve Limit (minutes)",
            ),
            (
                "manualTimeAutoApprovePeriod",
                "Manual Time Auto-Approve Period",
            ),
            (
                "manualTimeAutoApproveNewEmployees",
                "Auto-Approve New Employees",
            ),
            ("currency", "Currency"),
            ("workweekStartDay", "Workweek Start Day"),
            ("maxUsers", "Max Users"),
            ("maxProject", "Max Projects"),
            ("moduleTimeTrackingEnabled", "Time Tracking Module"),
            ("moduleSalesTrackerEnabled", "Sales Tracker Module"),
            ("isTrial", "Trial"),
            ("status", "Status"),
            ("storeVisitFormReviseMode", "Form Correction Access"),
        ],
    ),
    (
        "employee:",
        &[
            ("username", "Username"),
            ("name", "Name"),
            ("status", "Status"),
        ],
    ),
    (
        "employee:accessLevel",
        &[
            ("accessLevelId", "Role"),
            ("companyId", "Company"),
            ("status", "Status"),
        ],
    ),
    (
        "employee:companyMembership",
        &[
            ("userId", "Employee"),
            ("companyId", "Company"),
            ("status", "Membership Status"),
            ("isScreenshotActive", "Screenshot Enabled"),
            ("isAllowManualTime", "Manual Time Allowed"),
            ("isBlur", "Screen Blur"),
            ("isAllowEditTime", "Time Editing Allowed"),
            ("screenshotPerHour", "Screenshots per Hour"),
            ("idleTimeInSecond", "Idle Threshold (seconds)"),
            ("isDefault", "Default Company"),
        ],
    ),
    (
        "store:",
        &[
            ("ownerName", "Owner Name"),
            ("phone", "Phone"),
            ("lat", "Latitude"),
            ("lng", "Longitude"),
            ("radiusMeters", "Radius (meters)"),
            ("isActive", "Status"),
            ("groupId", "Region"),
            ("deletedAt", "Status"),
        ],
    ),
    ("storeGroup:", &[("isActive", "Status")]),
    ("storeGroup:member", &[("members", "Members")]),
    (
        "division:",
        &[
            ("name", "Name"),
            ("description", "Description"),
            ("deletedAt", "Status"),
        ],
    ),
    ("division:divisionMember", &[("members", "Anggota Divisi")]),
    (
        "storeVisitForm:",
        &[
            ("sortOrder", "Sort Order"),
            ("scope", "Scope"),
            ("isActive", "Status"),
            ("deletedAt", "Status"),
        ],
    ),
    (
        "storeVisitForm:assignment",
        &[("members", "Form Recipients")],
    ),
    (
        "storeVisitPlan:",
        &[
            ("userId", "Sales"),
            ("storeId", "Store"),
            ("visitDate", "Visit Date"),
            ("sortOrder", "Sort Order"),
            ("note", "Note"),
            ("deletedAt", "Status"),
        ],
    ),
    (
        "geofence:",
        &[
            ("lat", "Latitude"),
            ("lng", "Longitude"),
            ("radiusMeters", "Radius (meters)"),
            ("enforced", "Enforced"),
            ("applicableTo", "Applies To"),
            ("applicableIds", "Targets"),
            ("isActive", "Status"),
            ("timezone", "Time Zone"),
            ("weeklySchedule", "Weekly Schedule"),
            ("breakTimeHours", "Break Hours"),
            ("breakTimeMinutes", "Break Minutes"),
        ],
    ),
    (
        "negativeZone:",
        &[
            ("lat", "Latitude"),
            ("lng", "Longitude"),
            ("radiusMeters", "Radius (meters)"),
            ("isActive", "Status"),
        ],
    ),
    (
        "role:",
        &[
            ("name", "Role Name"),
            ("baseRole", "Role Level"),
            ("description", "Description"),
            ("status", "Status"),
            ("isSystem", "Built-in Role"),
            ("companyId", "Company"),
            ("createdById", "Created By"),
        ],
    ),
    ("role:permission", &[("members", "Permissions")]),
    ("company:formReviser", &[("members", "Form Revisers")]),
    (
        "company:manualTimeApprovalException",
        &[("members", "Manual Time Approval Rules")],
    ),
    (
        "company:workPattern",
        &[
            ("workDays", "Work Days per Week"),
            ("offDays", "Weekly Days Off"),
            ("shortDay", "Short Work Day"),
            ("effectiveFrom", "Effective From"),
            ("supersededAt", "Superseded At"),
            ("createdById", "Created By"),
        ],
    ),
    (
        "company:overtimeSetting",
        &[
            ("enabled", "Overtime Enabled"),
            ("retroWindowDays", "Back-dating Window (days)"),
            ("updatedById", "Updated By"),
        ],
    ),
    (
        "company:overtimeApprover",
        &[("members", "Overtime Approvers")],
    ),
    (
        "customer:",
        &[
            ("typeId", "Type"),
            ("groupId", "Group"),
            ("channelId", "Channel"),
            ("subChannelId", "Sub Channel"),
            ("taxNumber", "Tax Number"),
            ("phone", "Phone"),
            ("isActive", "Status"),
            ("deletedAt", "Status"),
        ],
    ),
    (
        "customerType:",
        &[("description", "Description"), ("isActive", "Status")],
    ),
    (
        "customerGroup:",
        &[("description", "Description"), ("isActive", "Status")],
    ),
    (
        "customerChannel:",
        &[("description", "Description"), ("isActive", "Status")],
    ),
    (
        "customerSubChannel:",
        &[
            ("description", "Description"),
            ("channelId", "Channel"),
            ("isActive", "Status"),
        ],
    ),
];

fn lookup(table: Labels, field: &str) -> Option<&'static str> {
    table.iter().find(|(k, _)| *k == field).map(|(_, v)| *v)
}

fn key(entity_type: &str, sub_entity: Option<&str>) -> String {
    format!("{entity_type}:{}", sub_entity.unwrap_or(""))
}

/// `labelFor`: label per entitas → label umum → `humanizeField`.
pub fn label_for(entity_type: &str, sub_entity: Option<&str>, field: &str) -> String {
    let k = key(entity_type, sub_entity);
    FIELD_LABELS
        .iter()
        .find(|(name, _)| *name == k)
        .and_then(|(_, table)| lookup(table, field))
        .or_else(|| lookup(COMMON_LABELS, field))
        .map_or_else(|| humanize_field(field), str::to_owned)
}

// ── Terjemahan nilai ─────────────────────────────────────────────────────────

/// `Number(value)` JS untuk nilai JSON (hanya bentuk yang bisa muncul dari kolom audit).
fn js_number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::Null => Some(0.0),
        Value::Bool(b) => Some(f64::from(u8::from(*b))),
        Value::String(s) => {
            let t = s.trim();
            if t.is_empty() {
                Some(0.0)
            } else {
                t.parse::<f64>().ok()
            }
        }
        _ => None,
    }
}

/// `codeMap`: kode angka → kalimat; di luar peta (atau bukan angka hingga) apa adanya.
fn code_map(labels: &[(i64, &'static str)], value: &Value) -> Value {
    let Some(code) = js_number(value).filter(|c| c.is_finite()) else {
        return value.clone();
    };
    if code.fract() == 0.0 && code.abs() < 1e15 {
        // `String(code)` untuk bilangan bulat.
        #[allow(clippy::cast_possible_truncation)]
        let int = code as i64;
        if let Some((_, label)) = labels.iter().find(|(k, _)| *k == int) {
            return json!(label);
        }
    }
    value.clone()
}

/// `textMap`: enum string → kalimat; nilai di luar peta apa adanya.
fn text_map(labels: &[(&str, &'static str)], value: &Value) -> Value {
    match value {
        Value::String(s) => labels
            .iter()
            .find(|(k, _)| k == s)
            .map_or_else(|| value.clone(), |(_, v)| json!(v)),
        _ => value.clone(),
    }
}

/// `boolStatus`.
fn bool_status(active: &str, inactive: &str, value: &Value) -> Value {
    match value {
        Value::Bool(true) => json!(active),
        Value::Bool(false) => json!(inactive),
        _ => value.clone(),
    }
}

/// `nullableDateStatus`: `value == null` → kata "ada tidaknya".
fn nullable_date_status(present: &str, absent: &str, value: &Value) -> Value {
    if value.is_null() {
        json!(absent)
    } else {
        json!(present)
    }
}

/// `users.STATUS_*` (§3.3a).
const USER_STATUS_LABELS: &[(i64, &str)] = &[
    (0, "Deleted"),
    (1, "Active"),
    (2, "Invited"),
    (3, "Deactivated"),
];
/// `CompaniesEntity.STATUS_*` — 0 = hapus ATAU suspend billing (§0.30).
const COMPANY_STATUS_LABELS: &[(i64, &str)] = &[(0, "Inactive"), (1, "Active")];
const REVISE_MODE_LABELS: &[(&str, &str)] = &[
    ("allPrivileged", "All admins & managers"),
    ("selected", "Selected revisers only"),
];

/// `translateValue(entityType, subEntity, field, value)`.
fn translate_value(
    entity_type: &str,
    sub_entity: Option<&str>,
    field: &str,
    value: &Value,
) -> Value {
    let k = format!("{}:{field}", key(entity_type, sub_entity));
    match k.as_str() {
        "company::status" => code_map(COMPANY_STATUS_LABELS, value),
        "company::storeVisitFormReviseMode" => text_map(REVISE_MODE_LABELS, value),
        "employee::status" => code_map(USER_STATUS_LABELS, value),
        "store::isActive" | "storeVisitForm::isActive" => bool_status("Active", "Inactive", value),
        "store::deletedAt" | "storeVisitForm::deletedAt" | "storeVisitPlan::deletedAt" => {
            nullable_date_status("Deleted", "Active", value)
        }
        "storeGroup::isActive" => bool_status("Active", "Archived", value),
        // Delete = satu-satunya transisi isActive geofence/negativeZone (§3.3a).
        "geofence::isActive" | "negativeZone::isActive" => bool_status("Active", "Deleted", value),
        _ => value.clone(),
    }
}

/// `describeAction`: `delete` oleh `system` dibaca "Deactivated by system" (§0.30).
pub fn describe_action(action: &str, actor_type: &str) -> String {
    match action {
        "delete" => {
            if actor_type == "system" {
                "Deactivated by system".to_owned()
            } else {
                "Deleted".to_owned()
            }
        }
        "create" => "Created".to_owned(),
        "update" => "Updated".to_owned(),
        "restore" => "Restored".to_owned(),
        other => other.to_owned(),
    }
}

/// `buildDiff`: `before`/`after` → `[{ field, label, before, after }]` (§6.2). Kosong bila
/// `changedFields` kosong (ringkasan massal, pra-baca gagal).
pub fn build_diff(
    entity_type: &str,
    sub_entity: Option<&str>,
    before: Option<&Value>,
    after: Option<&Value>,
    changed_fields: &[String],
) -> Vec<Value> {
    if changed_fields.is_empty() {
        return Vec::new();
    }
    let empty = Map::new();
    let before_obj = before.and_then(Value::as_object).unwrap_or(&empty);
    let after_obj = after.and_then(Value::as_object).unwrap_or(&empty);
    changed_fields
        .iter()
        .map(|field| {
            // `readProp(obj, field) ?? null`.
            let b = before_obj.get(field).unwrap_or(&Value::Null);
            let a = after_obj.get(field).unwrap_or(&Value::Null);
            json!({
                "field": field,
                "label": label_for(entity_type, sub_entity, field),
                "before": translate_value(entity_type, sub_entity, field, b),
                "after": translate_value(entity_type, sub_entity, field, a),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn humanize_splits_camel_case_and_capitalizes() {
        assert_eq!(humanize_field("isActive"), "Is Active");
        assert_eq!(humanize_field("_hidden"), "Hidden");
        assert_eq!(humanize_field("radius2Meters"), "Radius2 Meters");
        assert_eq!(humanize_field("URL"), "URL");
        assert_eq!(humanize_field(""), "");
        assert_eq!(humanize_field("aBC"), "A BC");
    }

    #[test]
    fn label_falls_back_scoped_then_common_then_humanized() {
        assert_eq!(label_for("store", None, "isActive"), "Status");
        assert_eq!(label_for("store", None, "name"), "Name");
        assert_eq!(label_for("store", None, "someNewField"), "Some New Field");
        assert_eq!(
            label_for("employee", Some("accessLevel"), "accessLevelId"),
            "Role"
        );
        // subEntity tak dikenal → hanya label umum / humanize.
        assert_eq!(label_for("store", Some("zzz"), "isActive"), "Is Active");
        assert_eq!(
            label_for("storeGroup", Some("member"), "members"),
            "Members"
        );
    }

    #[test]
    fn action_words() {
        assert_eq!(describe_action("delete", "system"), "Deactivated by system");
        assert_eq!(describe_action("delete", "user"), "Deleted");
        assert_eq!(describe_action("create", "user"), "Created");
        assert_eq!(describe_action("update", "webhook"), "Updated");
        assert_eq!(describe_action("restore", "user"), "Restored");
    }

    #[test]
    fn value_translators() {
        let d = build_diff(
            "store",
            None,
            Some(&json!({"isActive": true, "deletedAt": null})),
            Some(&json!({"isActive": false, "deletedAt": "2026-01-01T00:00:00.000Z"})),
            &["isActive".into(), "deletedAt".into()],
        );
        assert_eq!(d[0]["before"], "Active");
        assert_eq!(d[0]["after"], "Inactive");
        assert_eq!(d[1]["before"], "Active");
        assert_eq!(d[1]["after"], "Deleted");

        let g = build_diff(
            "storeGroup",
            None,
            Some(&json!({"isActive": true})),
            Some(&json!({"isActive": false})),
            &["isActive".into()],
        );
        assert_eq!(g[0]["after"], "Archived");
        let z = build_diff(
            "geofence",
            None,
            Some(&json!({"isActive": true})),
            Some(&json!({"isActive": false})),
            &["isActive".into()],
        );
        assert_eq!(z[0]["after"], "Deleted");
    }

    #[test]
    fn code_map_quirks_match_js_number() {
        // `Number(null)` = 0 → label "Deleted" (perilaku Node apa adanya).
        let d = build_diff(
            "employee",
            None,
            None,
            Some(&json!({"status": 3})),
            &["status".into()],
        );
        assert_eq!(d[0]["before"], "Deleted");
        assert_eq!(d[0]["after"], "Deactivated");
        // Teks angka; di luar peta; bukan angka.
        assert_eq!(code_map(USER_STATUS_LABELS, &json!("1")), json!("Active"));
        assert_eq!(code_map(USER_STATUS_LABELS, &json!(9)), json!(9));
        assert_eq!(code_map(USER_STATUS_LABELS, &json!("abc")), json!("abc"));
        assert_eq!(
            code_map(COMPANY_STATUS_LABELS, &json!(0)),
            json!("Inactive")
        );
    }

    #[test]
    fn text_map_and_unknown_pass_through() {
        let d = build_diff(
            "company",
            None,
            Some(&json!({"storeVisitFormReviseMode": "allPrivileged"})),
            Some(&json!({"storeVisitFormReviseMode": "selected"})),
            &["storeVisitFormReviseMode".into()],
        );
        assert_eq!(d[0]["before"], "All admins & managers");
        assert_eq!(d[0]["after"], "Selected revisers only");
        assert_eq!(d[0]["label"], "Form Correction Access");
        let o = build_diff(
            "store",
            None,
            Some(&json!({"name": "A"})),
            Some(&json!({"name": "B"})),
            &["name".into()],
        );
        assert_eq!(o[0]["before"], "A");
    }

    #[test]
    fn empty_changed_fields_gives_empty_diff() {
        assert!(build_diff("store", None, None, None, &[]).is_empty());
    }
}
