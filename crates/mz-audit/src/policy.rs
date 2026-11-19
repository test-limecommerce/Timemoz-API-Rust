//! Port `src/audit-logs/audit-field-policy.ts` — kolom mana yang berarti (§3.4).
//! Alasan per entri ada di berkas Node-nya; daftar di sini harus tetap identik.

use serde_json::{Map, Value};

use crate::row::Row;
use crate::scope::Model;

enum Pattern {
    Suffix(&'static str),
    Prefix(&'static str),
}

enum Policy {
    Allow(&'static [&'static str]),
    /// Model tanpa entri di `AUDIT_FIELD_POLICIES`.
    Missing,
    Deny {
        ignore: &'static [&'static str],
        patterns: &'static [Pattern],
        drop: &'static [&'static str],
    },
}

const fn deny(ignore: &'static [&'static str]) -> Policy {
    Policy::Deny {
        ignore,
        patterns: &[],
        drop: &[],
    }
}

const UPDATED_AT: &[&str] = &["updatedAt"];

fn policy_for(model: Model) -> Policy {
    match model {
        Model::Company => Policy::Deny {
            ignore: &[
                "updatedAt",
                "reportExportsUsedThisMonth",
                "reportExportsResetAt",
                "dailyReportSentAt",
                "googleCalendarSyncToken",
            ],
            patterns: &[
                Pattern::Suffix("TokenExpiresAt"),
                Pattern::Prefix("googleCalendarWebhook"),
            ],
            drop: &["inviteCode", "discordGuildId", "profilePictureFullPath"],
        },
        Model::Users => Policy::Allow(&["username", "name", "status"]),
        Model::Store => Policy::Deny {
            ignore: &["updatedAt", "totalVisits", "lastVisitAt", "lastVisitStatus"],
            patterns: &[],
            drop: &["qrToken"],
        },
        Model::StoreGroup
        | Model::StoreVisitFormTemplate
        | Model::StoreVisitPlan
        | Model::Geofence
        | Model::NegativeZone
        | Model::AccessLevel
        | Model::UserCompany
        | Model::CustomerType
        | Model::CustomerGroup
        | Model::CustomerChannel
        | Model::CustomerSubChannel
        | Model::OrderType
        | Model::OrderReason
        | Model::OrderPaymentTerm
        | Model::OrderPaymentMethod
        | Model::OrderCustomFieldDefinition
        | Model::VisitType
        | Model::VisitFrequency
        | Model::Order => deny(UPDATED_AT),
        Model::UserAccessLevel => Policy::Allow(&["accessLevelId", "companyId", "status"]),
        Model::StoreGroupUser
        | Model::StoreVisitFormTemplateUser
        | Model::StoreVisitFormReviser => Policy::Allow(&["userId", "deletedAt"]),
        Model::RolePermission => Policy::Allow(&["featureKey", "action"]),
        Model::ManualTimeApprovalException => {
            Policy::Allow(&["userId", "deletedAt", "limitMinutes"])
        }
        Model::Customer => Policy::Deny {
            ignore: UPDATED_AT,
            patterns: &[],
            drop: &["taxNumber"],
        },
        Model::Division => deny(&["updatedAt", "version"]),
        Model::DivisionMember => Policy::Allow(&["userId", "role", "deletedAt"]),
        Model::CompanyWorkPattern => deny(&["createdById"]),
        Model::CompanyOvertimeSetting => deny(&["updatedAt", "updatedById"]),
        Model::OvertimeApprover => Policy::Allow(&["userId"]),
        // Tidak terdaftar di `AUDIT_FIELD_POLICIES` Node → semua kolom dikecualikan
        // (`if (!policy) return true`). Dipertahankan apa adanya: baris `update`-nya
        // terbuang dan `create`-nya ber-`after` kosong, persis seperti Node.
        Model::Product
        | Model::ProductCategory
        | Model::ProductSubCategory
        | Model::ProductBrand
        | Model::ProductPrincipal
        | Model::Uom
        | Model::TaxRate
        | Model::ProductCustomField
        | Model::PriceList
        | Model::PriceListItem => Policy::Missing,
    }
}

/// `AUDIT_SECRET_FIELD_PATTERN` = `/token|secret|password|apikey|credential/i`.
pub fn is_secret_field(field: &str) -> bool {
    let lower = field.to_ascii_lowercase();
    ["token", "secret", "password", "apikey", "credential"]
        .iter()
        .any(|p| lower.contains(p))
}

/// `isExcludedField(model, field)`.
pub fn is_excluded_field(model: Model, field: &str) -> bool {
    if field == "id" || is_secret_field(field) {
        return true;
    }
    match policy_for(model) {
        Policy::Allow(allow) => !allow.contains(&field),
        Policy::Missing => true,
        Policy::Deny {
            ignore,
            patterns,
            drop,
        } => {
            ignore.contains(&field)
                || drop.contains(&field)
                || patterns.iter().any(|p| match p {
                    Pattern::Suffix(s) => field.ends_with(s),
                    Pattern::Prefix(s) => field.starts_with(s),
                })
        }
    }
}

/// `snapshotSelect(model, INTERNAL_FIELDS[model])`: `None` = baca seluruh kolom
/// (denylist), `Some` = hanya kolom ini (allowlist + kolom internal).
pub fn snapshot_select(model: Model) -> Option<Vec<&'static str>> {
    match policy_for(model) {
        Policy::Allow(allow) => {
            let mut out: Vec<&'static str> = allow.to_vec();
            out.extend_from_slice(model.internal_fields());
            Some(out)
        }
        Policy::Deny { .. } | Policy::Missing => None,
    }
}

/// `projectRow(model, row)` — baris mentah → snapshot tersaring §3.4.
pub fn project_row(model: Model, row: Option<&Row>) -> Option<Row> {
    let row = row?;
    let mut snapshot = Row::new();
    for (field, value) in row.iter() {
        if is_excluded_field(model, field) {
            continue;
        }
        snapshot.set(field, value.clone());
    }
    Some(snapshot)
}

/// `diffSnapshots(before, after)` — kolom yang berbeda, terurut.
pub fn diff_snapshots(before: Option<&Row>, after: Option<&Row>) -> Vec<String> {
    let (Some(before), Some(after)) = (before, after) else {
        return Vec::new();
    };
    let mut fields: Vec<&str> = Vec::new();
    for k in before.keys().chain(after.keys()) {
        if !fields.contains(&k) {
            fields.push(k);
        }
    }
    let mut changed: Vec<String> = fields
        .into_iter()
        .filter(|f| !same_value(before.get(f), after.get(f)))
        .map(str::to_owned)
        .collect();
    changed.sort();
    changed
}

/// `sameValue` — `undefined` dan `null` dianggap sama.
pub(crate) fn same_value(a: Option<&Value>, b: Option<&Value>) -> bool {
    let a = a.filter(|v| !v.is_null());
    let b = b.filter(|v| !v.is_null());
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// `sanitizeWhere(model, where)` — klausa `where` ringkasan massal tanpa kolom rahasia.
pub fn sanitize_where(where_: &Value) -> Value {
    match where_ {
        Value::Array(items) => Value::Array(items.iter().map(sanitize_where).collect()),
        Value::Object(map) => {
            let mut out = Map::new();
            for (k, v) in map {
                if is_secret_field(k) {
                    continue;
                }
                let v = if v.is_object() || v.is_array() {
                    sanitize_where(v)
                } else {
                    v.clone()
                };
                out.insert(k.clone(), v);
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn secret_guard_drops_unregistered_secret_columns() {
        for f in [
            "clickupApiToken",
            "jiraWebhookSecret",
            "password",
            "someCredentialX",
            "x_APIKEY",
        ] {
            assert!(is_excluded_field(Model::Company, f), "{f}");
        }
        assert!(is_secret_field("qrToken"));
        // Berulang: jawaban stabil.
        assert!(is_secret_field("jiraAccessToken"));
        assert!(is_secret_field("jiraAccessToken"));
        let row = Row::new()
            .with("name", "A")
            .with("jiraAccessToken", "s3cret");
        let snap = project_row(Model::Company, Some(&row)).unwrap_or_default();
        assert!(!snap.contains("jiraAccessToken"));
    }

    #[test]
    fn company_noise_and_drops() {
        for f in [
            "updatedAt",
            "reportExportsUsedThisMonth",
            "jiraTokenExpiresAt",
            "googleCalendarWebhookChannelId",
            "inviteCode",
            "discordGuildId",
            "profilePictureFullPath",
            "dailyReportSentAt",
        ] {
            assert!(is_excluded_field(Model::Company, f), "{f}");
        }
        assert!(!is_excluded_field(Model::Company, "status"));
        assert!(!is_excluded_field(Model::Company, "name"));
        assert!(!is_excluded_field(Model::Company, "uuid"));
    }

    #[test]
    fn users_is_an_allowlist() {
        for f in ["username", "name", "status"] {
            assert!(!is_excluded_field(Model::Users, f));
        }
        for f in [
            "password",
            "sentAt",
            "gaClientId",
            "productTourStep",
            "uuid",
            "createdAt",
        ] {
            assert!(is_excluded_field(Model::Users, f));
        }
        let select = snapshot_select(Model::Users).unwrap_or_default();
        assert_eq!(
            select,
            vec!["username", "name", "status", "id", "uuid", "name"]
        );
        assert!(!select.contains(&"password"));
        assert!(snapshot_select(Model::Store).is_none());
    }

    #[test]
    fn store_field_noise() {
        for f in [
            "totalVisits",
            "lastVisitAt",
            "lastVisitStatus",
            "qrToken",
            "updatedAt",
        ] {
            assert!(is_excluded_field(Model::Store, f));
        }
        assert!(!is_excluded_field(Model::Store, "deletedAt"));
        assert!(!is_excluded_field(Model::Store, "isActive"));
    }

    #[test]
    fn customer_tax_number_never_copied() {
        assert!(is_excluded_field(Model::Customer, "taxNumber"));
        assert!(is_excluded_field(Model::Customer, "updatedAt"));
        assert!(!is_excluded_field(Model::Customer, "phone"));
        let before = Row::new().with("taxNumber", "1").with("name", "A");
        let after = Row::new().with("taxNumber", "2").with("name", "A");
        let b = project_row(Model::Customer, Some(&before));
        let a = project_row(Model::Customer, Some(&after));
        assert!(diff_snapshots(b.as_ref(), a.as_ref()).is_empty());
    }

    #[test]
    fn project_row_keeps_null_json_and_arrays() {
        assert_eq!(project_row(Model::Geofence, None), None);
        let row = Row::new()
            .with("weeklySchedule", json!({"mon": [1, 2]}))
            .with("tags", json!(["a"]))
            .with("deletedAt", Value::Null);
        let snap = project_row(Model::Geofence, Some(&row)).unwrap_or_default();
        assert_eq!(snap.get("weeklySchedule"), Some(&json!({"mon": [1, 2]})));
        assert_eq!(snap.get("tags"), Some(&json!(["a"])));
        assert_eq!(snap.get("deletedAt"), Some(&Value::Null));
    }

    #[test]
    fn diff_compares_content_and_presence() {
        let a = Row::new()
            .with("x", json!([1, 2]))
            .with("y", json!({"k": 1}))
            .with("z", 1);
        let b = Row::new()
            .with("x", json!([1, 2]))
            .with("y", json!({"k": 2}))
            .with("w", 1);
        assert_eq!(diff_snapshots(Some(&a), Some(&b)), vec!["w", "y", "z"]);
        // null ≡ tidak ada.
        let c = Row::new().with("n", Value::Null);
        assert!(diff_snapshots(Some(&c), Some(&Row::new())).is_empty());
        assert!(diff_snapshots(None, Some(&b)).is_empty());
    }

    #[test]
    fn sanitize_where_drops_secret_values() {
        let w = json!({"companyId": 7, "qrToken": "abc", "AND": [{"inviteCodeToken": "x", "id": {"in": [1]}}]});
        assert_eq!(
            sanitize_where(&w),
            json!({"companyId": 7, "AND": [{"id": {"in": [1]}}]})
        );
        assert_eq!(sanitize_where(&Value::Null), Value::Null);
    }
}
