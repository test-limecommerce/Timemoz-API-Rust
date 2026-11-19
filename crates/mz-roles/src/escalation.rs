//! Port `src/permissions/role-escalation.ts` (§9.3 + §9.4) dan
//! `normalizeAndValidatePermissions` / `normalizeActions` (`roles.service.ts`,
//! `backfill-plan.ts`) — satu aturan, dipakai `create`, `replacePermissions`,
//! `customizeBuiltIn`, `clone`, dan `preview`.

use axum::http::StatusCode;
use mz_core::enums::{BaseRole, PermissionAction, rank_of};
use mz_core::permissions::EffectiveAccess;
use serde::Serialize;

use crate::error::RolesError;
use crate::preview::Grant;
use mz_core::feature_registry::{feature_by_key, required_feature_keys};

/// Satu baris input DTO (`RolePermissionInputDto`), sudah lolos ValidationPipe.
#[derive(Debug, Clone)]
pub struct PermissionInput {
    pub feature_key: String,
    pub actions: Vec<PermissionAction>,
}

#[derive(Serialize)]
struct FeatureError<'a> {
    code: &'static str,
    feature: &'a str,
    message: String,
}

#[derive(Serialize)]
struct ActionError<'a> {
    code: &'static str,
    feature: &'a str,
    actions: Vec<PermissionAction>,
    message: String,
}

#[derive(Serialize)]
struct DependencyError<'a> {
    code: &'static str,
    feature: &'a str,
    requires: [&'a str; 1],
    message: String,
}

fn unprocessable(body: &impl Serialize) -> RolesError {
    RolesError::serialized(StatusCode::UNPROCESSABLE_ENTITY, body)
}

/// `normalizeActions` (§5.4): dedupe urutan pertama; `WRITE`/`DELETE` menambah `READ` di akhir.
pub fn normalize_actions(actions: &[PermissionAction]) -> Vec<PermissionAction> {
    let mut out: Vec<PermissionAction> = Vec::new();
    for a in actions {
        if !out.contains(a) {
            out.push(*a);
        }
    }
    if (out.contains(&PermissionAction::WRITE) || out.contains(&PermissionAction::DELETE))
        && !out.contains(&PermissionAction::READ)
    {
        out.push(PermissionAction::READ);
    }
    out
}

/// `normalizeAndValidatePermissions` — §9.10, §9.2, §5.4, §9.11. Urutan cek & pesan = Node.
pub fn normalize_and_validate(input: &[PermissionInput]) -> Result<Vec<Grant>, RolesError> {
    let mut result: Vec<Grant> = Vec::new();
    for entry in input {
        let key = entry.feature_key.as_str();
        if result.iter().any(|g| g.feature_key == key) {
            return Err(unprocessable(&FeatureError {
                code: "DUPLICATE_FEATURE",
                feature: key,
                message: format!("Feature '{key}' is listed more than once"),
            }));
        }
        let Some(feature) = feature_by_key(key) else {
            return Err(unprocessable(&FeatureError {
                code: "UNKNOWN_FEATURE",
                feature: key,
                message: format!("Unknown feature '{key}'"),
            }));
        };
        if feature.is_system_only() {
            return Err(unprocessable(&FeatureError {
                code: "SYSTEM_ONLY_FEATURE",
                feature: key,
                message: format!(
                    "Feature '{}' cannot be granted to a company role",
                    feature.label
                ),
            }));
        }
        let invalid: Vec<PermissionAction> = entry
            .actions
            .iter()
            .copied()
            .filter(|a| !feature.actions.contains(a))
            .collect();
        if !invalid.is_empty() {
            let list: Vec<&str> = invalid.iter().map(PermissionAction::as_str).collect();
            return Err(unprocessable(&ActionError {
                code: "ACTION_NOT_SUPPORTED",
                feature: key,
                message: format!(
                    "Feature '{}' does not support {}",
                    feature.label,
                    list.join(", ")
                ),
                actions: invalid,
            }));
        }
        result.push(Grant {
            feature_key: key.to_owned(),
            actions: normalize_actions(&entry.actions),
        });
    }

    for grant in &result {
        for dep in required_feature_keys(&grant.feature_key) {
            if !result.iter().any(|g| g.feature_key == dep) {
                let label = |k: &str| feature_by_key(k).map_or("undefined", |f| f.label);
                return Err(unprocessable(&DependencyError {
                    code: "MISSING_DEPENDENCY",
                    feature: &grant.feature_key,
                    requires: [dep],
                    message: format!(
                        "Feature '{}' requires '{}' to also be checked",
                        label(&grant.feature_key),
                        label(dep)
                    ),
                }));
            }
        }
    }
    Ok(result)
}

#[derive(Serialize)]
struct EscalationError {
    code: &'static str,
    missing: Vec<String>,
    message: String,
}

/// `assertGrantsWithinActor` (§9.3): matriks yang diminta ⊆ matriks pelaku. Tanpa
/// pengecualian SYSTEM_ADMIN/SUPER_ADMIN (§1.12).
pub fn assert_grants_within_actor(
    actor: &EffectiveAccess,
    requested: &[Grant],
) -> Result<(), RolesError> {
    let mut missing: Vec<String> = Vec::new();
    for entry in requested {
        let held = actor.permissions.get(&entry.feature_key);
        for action in &entry.actions {
            if !held.is_some_and(|h| h.contains(action)) {
                missing.push(format!("{}:{action}", entry.feature_key));
            }
        }
    }
    if missing.is_empty() {
        return Ok(());
    }
    let mut features: Vec<&str> = Vec::new();
    for m in &missing {
        let f = m.split(':').next().unwrap_or_default();
        if !features.contains(&f) {
            features.push(f);
        }
    }
    let labels: Vec<&str> = features
        .iter()
        .map(|k| feature_by_key(k).map_or(*k, |f| f.label))
        .collect();
    let message = format!(
        "You cannot grant access you do not have yourself: {}. \
         If you believe you should have it, ask someone with a higher role — \
         or check that the permission backfill has been run in this environment.",
        labels.join(", ")
    );
    Err(unprocessable(&EscalationError {
        code: "PERMISSION_ESCALATION",
        missing,
        message,
    }))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BaseRoleError {
    code: &'static str,
    base_role: Option<BaseRole>,
    message: String,
}

/// `assertBaseRoleWithinActor` (§9.4): setingkat boleh; peringkat tak dikenal = tolak.
/// `None` = `baseRole: null` yang lolos `@IsOptional()` di `PATCH` (Node: `rankOf(null)` → tolak).
pub fn assert_base_role_within_actor(
    actor: &EffectiveAccess,
    base: Option<BaseRole>,
) -> Result<(), RolesError> {
    let actor_rank = rank_of(&actor.effective_role_code);
    let target_rank = base.and_then(|b| rank_of(b.as_str()));
    let ok = matches!((actor_rank, target_rank), (Some(a), Some(t)) if t <= a);
    if ok {
        return Ok(());
    }
    let shown = base.map_or("null", |b| b.as_str());
    Err(unprocessable(&BaseRoleError {
        code: "BASE_ROLE_ABOVE_ACTOR",
        base_role: base,
        message: format!("Base role '{shown}' is above your own access level"),
    }))
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    const R: PermissionAction = PermissionAction::READ;
    const W: PermissionAction = PermissionAction::WRITE;
    const D: PermissionAction = PermissionAction::DELETE;

    fn input(key: &str, actions: &[PermissionAction]) -> PermissionInput {
        PermissionInput {
            feature_key: key.into(),
            actions: actions.to_vec(),
        }
    }

    fn body(e: RolesError) -> String {
        match e {
            RolesError::Body { body, .. } => body.to_string(),
            e => format!("{e:?}"),
        }
    }

    fn actor(code: &str, grants: &[(&str, PermissionAction)]) -> EffectiveAccess {
        let mut permissions: HashMap<String, HashSet<PermissionAction>> = HashMap::new();
        for (f, a) in grants {
            permissions.entry((*f).to_owned()).or_default().insert(*a);
        }
        EffectiveAccess {
            role_id: 1,
            role_code: code.into(),
            role_name: code.into(),
            is_system: true,
            base_role: None,
            effective_role_code: code.into(),
            company_id: 1,
            permissions,
        }
    }

    #[test]
    fn normalize_adds_read_last() {
        assert_eq!(normalize_actions(&[W, W]), vec![W, R]);
        assert_eq!(normalize_actions(&[D, R]), vec![D, R]);
        assert_eq!(normalize_actions(&[R]), vec![R]);
    }

    #[test]
    fn validation_errors_keep_node_key_order() {
        let e = normalize_and_validate(&[input("users", &[R]), input("users", &[R])]);
        assert_eq!(
            e.map_err(body).err().as_deref(),
            Some(
                r#"{"code":"DUPLICATE_FEATURE","feature":"users","message":"Feature 'users' is listed more than once"}"#
            )
        );
        let e = normalize_and_validate(&[input("access-levels", &[R])]);
        assert_eq!(
            e.map_err(body).err().as_deref(),
            Some(
                r#"{"code":"SYSTEM_ONLY_FEATURE","feature":"access-levels","message":"Feature 'Access Level (lama)' cannot be granted to a company role"}"#
            )
        );
        let e = normalize_and_validate(&[input("clock", &[R, D, D])]);
        assert_eq!(
            e.map_err(body).err().as_deref(),
            Some(
                r#"{"code":"ACTION_NOT_SUPPORTED","feature":"clock","actions":["DELETE","DELETE"],"message":"Feature 'Absensi' does not support DELETE, DELETE"}"#
            )
        );
        let e = normalize_and_validate(&[input("store-visits", &[R])]);
        assert_eq!(
            e.map_err(body).err().as_deref(),
            Some(
                r#"{"code":"MISSING_DEPENDENCY","feature":"store-visits","requires":["stores"],"message":"Feature 'Kunjungan Toko' requires 'Toko' to also be checked"}"#
            )
        );
        let ok = normalize_and_validate(&[input("store-visits", &[W]), input("stores", &[R])]);
        assert_eq!(
            ok.ok(),
            Some(vec![
                Grant {
                    feature_key: "store-visits".into(),
                    actions: vec![W, R]
                },
                Grant {
                    feature_key: "stores".into(),
                    actions: vec![R]
                },
            ])
        );
    }

    #[test]
    fn escalation_lists_missing_in_request_order() {
        let a = actor("ADMIN", &[("users", R), ("users", W)]);
        let req = vec![
            Grant {
                feature_key: "users".into(),
                actions: vec![D, R],
            },
            Grant {
                feature_key: "roles".into(),
                actions: vec![W, R],
            },
        ];
        assert_eq!(
            assert_grants_within_actor(&a, &req)
                .map_err(body)
                .err()
                .as_deref(),
            Some(
                "{\"code\":\"PERMISSION_ESCALATION\",\"missing\":[\"users:DELETE\",\"roles:WRITE\",\"roles:READ\"],\"message\":\"You cannot grant access you do not have yourself: Karyawan, Peran & Hak Akses. If you believe you should have it, ask someone with a higher role — or check that the permission backfill has been run in this environment.\"}"
            )
        );
        assert!(assert_grants_within_actor(&a, &req[..0]).is_ok());
    }

    #[test]
    fn base_role_ceiling() {
        let m = actor("MANAGER", &[]);
        assert!(assert_base_role_within_actor(&m, Some(BaseRole::MANAGER)).is_ok());
        assert_eq!(
            assert_base_role_within_actor(&m, Some(BaseRole::ADMIN))
                .map_err(body)
                .err()
                .as_deref(),
            Some(
                r#"{"code":"BASE_ROLE_ABOVE_ACTOR","baseRole":"ADMIN","message":"Base role 'ADMIN' is above your own access level"}"#
            )
        );
        assert_eq!(
            assert_base_role_within_actor(&m, None)
                .map_err(body)
                .err()
                .as_deref(),
            Some(
                r#"{"code":"BASE_ROLE_ABOVE_ACTOR","baseRole":null,"message":"Base role 'null' is above your own access level"}"#
            )
        );
        let unknown = actor("c1_x", &[]);
        assert!(assert_base_role_within_actor(&unknown, Some(BaseRole::STAFF)).is_err());
        assert!(
            assert_base_role_within_actor(&actor("SYSTEM_ADMIN", &[]), Some(BaseRole::ADMIN))
                .is_ok()
        );
    }
}
