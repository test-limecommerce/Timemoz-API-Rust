//! Port `src/permissions/{data-scope,role-preview,me-permissions,enforcement}.ts` — logika
//! murni (tanpa DB) untuk `POST /v1/admin/roles/preview` dan `GET /v1/me/permissions`.

use std::collections::HashSet;

use mz_core::enums::{BaseRole, PermissionAction};
use mz_core::permissions::{EffectiveAccess, granted_actions_for};
use serde::Serialize;

use crate::reach::{is_out_of_reach, min_base_role_for};
use mz_core::feature_registry::{FEATURES, feature_by_key, required_feature_keys};

/// `DataScope` (`data-scope.ts`). Bukan input otorisasi — hanya kalimat tampilan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[allow(non_camel_case_types)]
pub enum DataScope {
    ALL_COMPANIES,
    COMPANY,
    SELF,
}

/// `dataScopeOf(effectiveRoleCode)`: kode tak dikenal jatuh ke `SELF` (paling sempit).
pub fn data_scope_of(effective_role_code: Option<&str>) -> DataScope {
    match effective_role_code {
        Some("SYSTEM_ADMIN") => DataScope::ALL_COMPANIES,
        Some("SUPER_ADMIN" | "ADMIN" | "MANAGER") => DataScope::COMPANY,
        _ => DataScope::SELF,
    }
}

/// Satu baris matriks yang sudah ternormalisasi (urutan sisip Node).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub feature_key: String,
    pub actions: Vec<PermissionAction>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PersonScopedNote {
    pub feature: String,
    pub visibility: &'static str,
}

/// `PreviewWarning`. Urutan kunci mengikuti objek literal Node:
/// `MISSING_DEPENDENCY` = `code, feature, requires, message`; `OUT_OF_REACH` =
/// `code, feature, message, minBaseRole`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewWarning {
    pub code: &'static str,
    pub feature: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requires: Option<Vec<String>>,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_base_role: Option<&'static str>,
}

/// `RolePreview`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RolePreview {
    pub menus_web: Vec<String>,
    pub menus_mobile: Vec<String>,
    pub scope_summary: Option<String>,
    pub person_scoped_notes: Vec<PersonScopedNote>,
    pub warnings: Vec<PreviewWarning>,
    pub affected_users: i64,
}

fn dedupe(items: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    items
        .into_iter()
        .filter(|i| seen.insert(i.clone()))
        .collect()
}

/// `buildRolePreview(baseRole, permissions, affectedUsers)`.
pub fn build_role_preview(
    base: BaseRole,
    permissions: &[Grant],
    affected_users: i64,
) -> RolePreview {
    // `new Map(...)`: kunci kembar → nilai terakhir menang, posisi pertama dipertahankan.
    let mut granted: Vec<(&str, Vec<PermissionAction>)> = Vec::new();
    for p in permissions {
        let mut set: Vec<PermissionAction> = Vec::new();
        for a in &p.actions {
            if !set.contains(a) {
                set.push(*a);
            }
        }
        match granted.iter_mut().find(|(k, _)| *k == p.feature_key) {
            Some(slot) => slot.1 = set,
            None => granted.push((p.feature_key.as_str(), set)),
        }
    }

    let mut menus_web = Vec::new();
    let mut menus_mobile = Vec::new();
    let mut notes = Vec::new();
    let mut warnings = Vec::new();
    let scope = data_scope_of(Some(base.as_str()));
    let mut scoped_labels = Vec::new();

    for (feature_key, actions) in &granted {
        let Some(feature) = feature_by_key(feature_key) else {
            continue;
        };
        if feature.has_surface("web") {
            menus_web.push(feature.label.to_owned());
        }
        if feature.has_surface("mobile") {
            menus_mobile.push(feature.label.to_owned());
        }

        if feature.is_person_scoped() && actions.contains(&PermissionAction::READ) {
            let self_only = scope == DataScope::SELF;
            notes.push(PersonScopedNote {
                feature: (*feature_key).to_owned(),
                visibility: if self_only {
                    "self-only"
                } else {
                    "all-employees"
                },
            });
            scoped_labels.push(feature.label.to_owned());
            if self_only {
                warnings.push(PreviewWarning {
                    code: "SCOPE_TOO_NARROW",
                    feature: (*feature_key).to_owned(),
                    requires: None,
                    message: format!(
                        "Role ini bisa membuka {}, tapi karena setingkat Staff ia hanya melihat datanya sendiri.",
                        feature.label
                    ),
                    min_base_role: None,
                });
            }
        }

        for dep in required_feature_keys(feature_key) {
            if !granted.iter().any(|(k, _)| *k == dep) {
                let dep_label = feature_by_key(dep).map_or(dep, |f| f.label);
                warnings.push(PreviewWarning {
                    code: "MISSING_DEPENDENCY",
                    feature: (*feature_key).to_owned(),
                    requires: Some(vec![dep.to_owned()]),
                    message: format!(
                        "{} butuh menu {dep_label} — tanpanya layarnya kosong.",
                        feature.label
                    ),
                    min_base_role: None,
                });
            }
        }

        for action in actions {
            if is_out_of_reach(feature_key, *action, base) {
                warnings.push(PreviewWarning {
                    code: "OUT_OF_REACH",
                    feature: (*feature_key).to_owned(),
                    requires: None,
                    message: format!(
                        "{} ({action}) tidak terjangkau plafon role setingkat {base}.",
                        feature.label
                    ),
                    min_base_role: min_base_role_for(feature_key, *action),
                });
            }
        }
    }

    let scope_summary = if scoped_labels.is_empty() {
        None
    } else {
        Some(format!(
            "Pada menu {}: {}",
            dedupe(scoped_labels).join(" & "),
            if scope == DataScope::SELF {
                "hanya melihat datanya sendiri"
            } else {
                "melihat data seluruh karyawan"
            }
        ))
    };

    RolePreview {
        menus_web: dedupe(menus_web),
        menus_mobile: dedupe(menus_mobile),
        scope_summary,
        person_scoped_notes: notes,
        warnings,
        affected_users,
    }
}

/// `MyFeatureAccess`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MyFeatureAccess {
    pub key: &'static str,
    pub label: &'static str,
    pub label_en: &'static str,
    pub group: &'static str,
    pub surfaces: &'static [&'static str],
    pub person_scoped: bool,
    pub actions: Vec<PermissionAction>,
}

/// `MePermissions`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MePermissions {
    pub data_scope: DataScope,
    pub scope_description: &'static str,
    pub enforced: bool,
    pub features: Vec<MyFeatureAccess>,
}

fn scope_description(scope: DataScope) -> &'static str {
    match scope {
        DataScope::ALL_COMPANIES => "Melihat data lintas seluruh company.",
        DataScope::COMPANY => "Melihat data seluruh karyawan di company ini.",
        DataScope::SELF => "Hanya melihat datanya sendiri.",
    }
}

/// `buildMePermissions(access, resolveEnforcement(access))` — `enforced = access !== null`.
pub fn build_me_permissions(access: Option<&EffectiveAccess>) -> MePermissions {
    let data_scope = data_scope_of(access.map(|a| a.effective_role_code.as_str()));
    MePermissions {
        data_scope,
        scope_description: scope_description(data_scope),
        enforced: access.is_some(),
        features: FEATURES
            .iter()
            .map(|f| MyFeatureAccess {
                key: f.key,
                label: f.label,
                label_en: f.label_en,
                group: f.group,
                surfaces: f.surfaces,
                person_scoped: f.is_person_scoped(),
                actions: access
                    .map(|a| granted_actions_for(f, a))
                    .unwrap_or_default(),
            })
            .collect(),
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn g(key: &str, actions: &[PermissionAction]) -> Grant {
        Grant {
            feature_key: key.into(),
            actions: actions.to_vec(),
        }
    }

    const R: PermissionAction = PermissionAction::READ;
    const W: PermissionAction = PermissionAction::WRITE;

    /// `role-preview.spec.ts`: STAFF + fitur personScoped → self-only + SCOPE_TOO_NARROW.
    #[test]
    fn staff_person_scoped_is_self_only() {
        let p = build_role_preview(BaseRole::STAFF, &[g("leaves", &[R, W])], 3);
        let v = serde_json::to_value(&p).unwrap_or_default();
        assert_eq!(v["menusWeb"], serde_json::json!(["Cuti"]));
        assert_eq!(v["menusMobile"], serde_json::json!(["Cuti"]));
        assert_eq!(
            v["scopeSummary"],
            "Pada menu Cuti: hanya melihat datanya sendiri"
        );
        assert_eq!(v["personScopedNotes"][0]["visibility"], "self-only");
        assert_eq!(v["warnings"][0]["code"], "SCOPE_TOO_NARROW");
        assert_eq!(v["affectedUsers"], 3);
    }

    #[test]
    fn manager_out_of_reach_has_min_base_role() {
        let p = build_role_preview(BaseRole::MANAGER, &[g("roles", &[W, R])], 0);
        // MISSING_DEPENDENCY datang SEBELUM OUT_OF_REACH (urutan Node).
        let codes: Vec<&str> = p.warnings.iter().map(|w| w.code).collect();
        assert_eq!(
            codes,
            ["MISSING_DEPENDENCY", "OUT_OF_REACH", "OUT_OF_REACH"]
        );
        let oor = serde_json::to_string(&p.warnings[1]).unwrap_or_default();
        assert_eq!(
            oor,
            r#"{"code":"OUT_OF_REACH","feature":"roles","message":"Peran & Hak Akses (WRITE) tidak terjangkau plafon role setingkat MANAGER.","minBaseRole":"ADMIN"}"#
        );
        let md = serde_json::to_string(&p.warnings[0]).unwrap_or_default();
        assert_eq!(
            md,
            r#"{"code":"MISSING_DEPENDENCY","feature":"roles","requires":["users"],"message":"Peran & Hak Akses butuh menu Karyawan — tanpanya layarnya kosong."}"#
        );
    }

    fn access(is_system: bool, code: &str, grants: &[(&str, PermissionAction)]) -> EffectiveAccess {
        let mut permissions: HashMap<String, HashSet<PermissionAction>> = HashMap::new();
        for (f, a) in grants {
            permissions.entry((*f).to_owned()).or_default().insert(*a);
        }
        EffectiveAccess {
            role_id: 1,
            role_code: code.into(),
            role_name: code.into(),
            is_system,
            base_role: None,
            effective_role_code: code.into(),
            company_id: 1,
            permissions,
        }
    }

    #[test]
    fn me_permissions_bypass_and_custom() {
        let none = build_me_permissions(None);
        assert_eq!(none.data_scope, DataScope::SELF);
        assert!(!none.enforced);
        assert!(none.features.iter().all(|f| f.actions.is_empty()));

        // Bawaan dengan matriks kosong = semua verb.
        let empty = build_me_permissions(Some(&access(true, "ADMIN", &[])));
        assert!(empty.enforced);
        assert_eq!(empty.data_scope, DataScope::COMPANY);
        assert!(empty.features.iter().all(|f| {
            f.actions.len()
                == FEATURES
                    .iter()
                    .find(|x| x.key == f.key)
                    .map_or(0, |x| x.actions.len())
        }));

        // Bawaan bermatriks: legacyOpen tetap penuh, sisanya disaring.
        let b = build_me_permissions(Some(&access(true, "STAFF", &[("users", R)])));
        let users = b.features.iter().find(|f| f.key == "users");
        assert_eq!(users.map(|f| f.actions.clone()), Some(vec![R]));
        let shots = b.features.iter().find(|f| f.key == "screenshots");
        assert_eq!(shots.map(|f| f.actions.len()), Some(3));

        // Custom: matriks satu-satunya sumber, legacyOpen tidak bypass.
        let c = build_me_permissions(Some(&access(false, "MANAGER", &[])));
        assert!(c.features.iter().all(|f| f.actions.is_empty()));
    }
}
