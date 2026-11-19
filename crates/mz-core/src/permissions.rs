//! Port `PermissionsService.getEffectiveAccess` + `PermissionInterceptor`
//! (`src/permissions/*`). Prinsip ASIMETRIS (§5.1):
//!
//! * role BAWAAN: `RolesGuard` sudah memutuskan; gerbang ini hanya boleh
//!   mempersempit dan boleh melewatkan saat datanya tidak ada;
//! * role CUSTOM: gerbang ini ADALAH keputusannya; ketiadaan data = tolak.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use axum::http::StatusCode;
use serde_json::json;

use crate::cache::{EpochCache, Keep, shared_err};
use crate::enums::{BaseRole, PermissionAction, effective_role_code, rank_of};
use crate::feature_registry::{FEATURES, FeatureDefinition, feature_by_key};
use crate::{ApiError, ApiResult, AppState};

pub const PERMISSION_CACHE_TTL: Duration = Duration::from_secs(60);

/// `req.access` di Node.
#[derive(Debug, Clone)]
pub struct EffectiveAccess {
    pub role_id: i32,
    pub role_code: String,
    pub role_name: String,
    pub is_system: bool,
    pub base_role: Option<BaseRole>,
    pub effective_role_code: String,
    pub company_id: i32,
    pub permissions: HashMap<String, HashSet<PermissionAction>>,
}

impl EffectiveAccess {
    pub fn is_granted(&self, feature: &str, action: Option<PermissionAction>) -> bool {
        action.is_some_and(|a| {
            self.permissions
                .get(feature)
                .is_some_and(|s| s.contains(&a))
        })
    }
}

/// `grantedActionsFor` (`me-permissions.ts`): role bawaan ikut bypass interceptor
/// (`legacyOpen` / matriks kosong, semua aksi fitur); selain itu hanya aksi yang tercentang,
/// dalam urutan `FeatureDefinition.actions`.
pub fn granted_actions_for(
    feature: &FeatureDefinition,
    access: &EffectiveAccess,
) -> Vec<PermissionAction> {
    if access.is_system && (feature.is_legacy_open() || access.permissions.is_empty()) {
        return feature.actions.to_vec();
    }
    let granted = access.permissions.get(feature.key);
    feature
        .actions
        .iter()
        .copied()
        .filter(|a| granted.is_some_and(|s| s.contains(a)))
        .collect()
}

/// [`granted_actions_for`] menurut `key` fitur; tanpa role aktif atau kunci tak dikenal → kosong.
pub fn granted_actions(
    access: Option<&EffectiveAccess>,
    feature_key: &str,
) -> Vec<PermissionAction> {
    match (access, feature_by_key(feature_key)) {
        (Some(a), Some(f)) => granted_actions_for(f, a),
        _ => Vec::new(),
    }
}

pub type PermissionCache = EpochCache<(i32, i32), Option<Arc<EffectiveAccess>>>;

pub fn new_permission_cache() -> PermissionCache {
    EpochCache::new(100_000)
}

/// Pengecualian `@PermissionExempt(...)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exempt {
    NoUser,
    SeparateAuth,
    SelfService,
    SelfScoped,
}

/// Fitur registry ber-`legacyOpen` (`feature.registry.ts`), diturunkan dari
/// [`crate::feature_registry::FEATURES`] (satu sumber).
pub static LEGACY_OPEN_FEATURES: LazyLock<Vec<&'static str>> = LazyLock::new(|| {
    FEATURES
        .iter()
        .filter(|f| f.is_legacy_open())
        .map(|f| f.key)
        .collect()
});

#[derive(sqlx::FromRow)]
struct RoleRow {
    id: i32,
    code: String,
    name: String,
    #[sqlx(rename = "isSystem")]
    is_system: bool,
    #[sqlx(rename = "baseRole")]
    base_role: Option<BaseRole>,
    status: i32,
}

async fn fetch(
    db: &sqlx::PgPool,
    user_id: i32,
    company_id: i32,
) -> sqlx::Result<Option<Arc<EffectiveAccess>>> {
    let rows = sqlx::query_as::<_, RoleRow>(
        r#"SELECT al.id, al.code, al.name, al."isSystem", al."baseRole"::text AS "baseRole", al.status
           FROM "userAccessLevel" ual
           JOIN "accessLevel" al ON al.id = ual."accessLevelId"
           WHERE ual."userId" = $1 AND ual."companyId" = $2 AND ual.status = 1
           ORDER BY ual.id DESC"#,
    )
    .bind(user_id)
    .bind(company_id)
    .fetch_all(db)
    .await?;

    // `pickRole`: role aktif dengan peringkat efektif tertinggi; seri → yang
    // muncul lebih dulu (urutan `ual.id DESC`).
    let mut best: Option<(&RoleRow, i16)> = None;
    for role in rows.iter().filter(|r| r.status == 1) {
        let code = effective_role_code(&role.code, role.is_system, role.base_role);
        let rank = rank_of(&code).map_or(-1, i16::from);
        if best.is_none_or(|(_, r)| rank > r) {
            best = Some((role, rank));
        }
    }
    let Some((winner, _)) = best else {
        return Ok(None);
    };

    let grants = sqlx::query_as::<_, (String, PermissionAction)>(
        r#"SELECT "featureKey", action::text FROM "rolePermission" WHERE "accessLevelId" = $1"#,
    )
    .bind(winner.id)
    .fetch_all(db)
    .await?;
    let mut permissions: HashMap<String, HashSet<PermissionAction>> = HashMap::new();
    for (feature, action) in grants {
        permissions.entry(feature).or_default().insert(action);
    }

    Ok(Some(Arc::new(EffectiveAccess {
        role_id: winner.id,
        role_code: winner.code.clone(),
        role_name: winner.name.clone(),
        is_system: winner.is_system,
        base_role: winner.base_role,
        effective_role_code: effective_role_code(&winner.code, winner.is_system, winner.base_role),
        company_id,
        permissions,
    })))
}

pub async fn effective_access(
    state: &AppState,
    user_id: i32,
    company_id: i32,
) -> ApiResult<Option<Arc<EffectiveAccess>>> {
    let db = state.db.clone();
    state
        .caches
        .permissions
        .get_or_load(
            (user_id, company_id),
            |_| Some(Keep::ttl(PERMISSION_CACHE_TTL)),
            move || async move { fetch(&db, user_id, company_id).await.map_err(shared_err) },
        )
        .await
        .map_err(|e| ApiError::internal(anyhow::anyhow!("{e}")))
}

/// `custom-role-probe.ts` `holdsCustomRole` — hanya bisa MENOLAK.
pub async fn holds_custom_role(db: &sqlx::PgPool, user_id: i32) -> sqlx::Result<bool> {
    let row: Option<i32> = sqlx::query_scalar(
        r#"SELECT ual.id FROM "userAccessLevel" ual
           JOIN "accessLevel" al ON al.id = ual."accessLevelId"
           WHERE ual."userId" = $1 AND ual.status = 1 AND al."isSystem" = false AND al.status = 1
           LIMIT 1"#,
    )
    .bind(user_id)
    .fetch_optional(db)
    .await?;
    Ok(row.is_some())
}

pub fn denied(feature: Option<&str>, action: Option<PermissionAction>) -> ApiError {
    let message = match feature {
        Some(f) => format!(
            "Role ini tidak punya izin {} untuk fitur {f}",
            action.map_or("?", |a| a.as_str())
        ),
        None => "Endpoint ini belum dipetakan ke fitur mana pun".to_owned(),
    };
    ApiError::body(
        StatusCode::FORBIDDEN,
        json!({ "code": "PERMISSION_DENIED", "feature": feature, "action": action, "message": message }),
    )
}

/// Keputusan `PermissionInterceptor` sesudah `req.access` terpasang.
/// `is_custom` = hasil `isCustomRoleHolder`.
pub fn decide(
    feature: Option<&str>,
    action: Option<PermissionAction>,
    company_id: Option<i32>,
    access: Option<&EffectiveAccess>,
    is_custom: bool,
) -> ApiResult<()> {
    let Some(feature) = feature else {
        if !is_custom {
            return Ok(());
        }
        return Err(denied(None, action));
    };

    if !is_custom {
        if LEGACY_OPEN_FEATURES.contains(&feature) {
            return Ok(());
        }
        let Some(access) = access else { return Ok(()) };
        if access.permissions.is_empty() {
            return Ok(());
        }
        if !access.is_granted(feature, action) {
            return Err(denied(Some(feature), action));
        }
        return Ok(());
    }

    let (Some(_), Some(access)) = (company_id, access) else {
        return Err(ApiError::coded(
            StatusCode::FORBIDDEN,
            "NO_ACTIVE_COMPANY",
            "No active company found for user",
        ));
    };
    if !access.is_granted(feature, action) {
        return Err(denied(Some(feature), action));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn access(is_system: bool, grants: &[(&str, PermissionAction)]) -> EffectiveAccess {
        let mut permissions: HashMap<String, HashSet<PermissionAction>> = HashMap::new();
        for (f, a) in grants {
            permissions.entry((*f).to_owned()).or_default().insert(*a);
        }
        EffectiveAccess {
            role_id: 1,
            role_code: "X".into(),
            role_name: "X".into(),
            is_system,
            base_role: None,
            effective_role_code: "ADMIN".into(),
            company_id: 1,
            permissions,
        }
    }

    #[test]
    fn built_in_with_empty_matrix_passes() {
        let a = access(true, &[]);
        assert!(
            decide(
                Some("users"),
                Some(PermissionAction::WRITE),
                Some(1),
                Some(&a),
                false
            )
            .is_ok()
        );
    }

    #[test]
    fn built_in_with_matrix_is_narrowed() {
        let a = access(true, &[("users", PermissionAction::READ)]);
        assert!(
            decide(
                Some("users"),
                Some(PermissionAction::READ),
                Some(1),
                Some(&a),
                false
            )
            .is_ok()
        );
        assert!(
            decide(
                Some("users"),
                Some(PermissionAction::WRITE),
                Some(1),
                Some(&a),
                false
            )
            .is_err()
        );
    }

    #[test]
    fn custom_role_without_grant_is_denied() {
        let a = access(false, &[]);
        assert!(
            decide(
                Some("users"),
                Some(PermissionAction::READ),
                Some(1),
                Some(&a),
                true
            )
            .is_err()
        );
        assert!(decide(None, Some(PermissionAction::READ), Some(1), Some(&a), true).is_err());
    }

    #[test]
    fn unknown_method_is_denied() {
        let a = access(false, &[("users", PermissionAction::READ)]);
        assert!(decide(Some("users"), None, Some(1), Some(&a), true).is_err());
    }
}
