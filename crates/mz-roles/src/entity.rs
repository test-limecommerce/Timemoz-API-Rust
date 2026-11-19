//! Bentuk respons `src/roles/entities/role.entity.ts` (urutan kunci = `toRoleEntity`).

use mz_account::assignable_role::AccessLevel;
use mz_core::enums::{BaseRole, PermissionAction};
use mz_core::time::JsDate;
use serde::Serialize;

/// `{ data }` — pembungkus semua respons controller ini.
#[derive(Debug, Serialize)]
pub struct Data<T> {
    pub data: T,
}

/// `BUILT_IN_SLOTS`.
const BUILT_IN_SLOTS: &[&str] = &["ADMIN", "MANAGER", "STAFF", "GUEST", "SUPER_ADMIN"];

/// `builtInSlotOf(role)`.
pub fn built_in_slot_of(role: &AccessLevel) -> Option<String> {
    if role.is_system {
        return BUILT_IN_SLOTS
            .contains(&role.code.as_str())
            .then(|| role.code.clone());
    }
    role.overrides_role_id
        .and(role.base_role)
        .map(|b| b.as_str().to_owned())
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RoleEntity {
    pub uuid: String,
    pub code: String,
    pub name: String,
    pub description: Option<String>,
    pub base_role: Option<BaseRole>,
    pub is_system: bool,
    pub is_built_in_override: bool,
    pub built_in_slot: Option<String>,
    pub status: i32,
    pub created_at: JsDate,
    pub updated_at: Option<JsDate>,
}

impl From<&AccessLevel> for RoleEntity {
    /// `toRoleEntity(role)`.
    fn from(role: &AccessLevel) -> Self {
        Self {
            uuid: role.uuid.clone(),
            code: role.code.clone(),
            name: role.name.clone(),
            description: role.description.clone(),
            base_role: role.base_role,
            is_system: role.is_system,
            is_built_in_override: role.overrides_role_id.is_some(),
            built_in_slot: built_in_slot_of(role),
            status: role.status,
            created_at: role.created_at,
            updated_at: role.updated_at,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RolePermissionEntity {
    pub feature_key: String,
    pub actions: Vec<PermissionAction>,
}

#[derive(Debug, Serialize)]
pub struct RoleDetailEntity {
    #[serde(flatten)]
    pub role: RoleEntity,
    pub permissions: Vec<RolePermissionEntity>,
}

/// `groupByFeature(rows)` — urutan kemunculan pertama tiap fitur, verb sesuai urutan baris.
pub fn group_by_feature(rows: &[(String, PermissionAction)]) -> Vec<RolePermissionEntity> {
    let mut out: Vec<RolePermissionEntity> = Vec::new();
    for (feature, action) in rows {
        match out.iter_mut().find(|p| &p.feature_key == feature) {
            Some(p) => p.actions.push(*action),
            None => out.push(RolePermissionEntity {
                feature_key: feature.clone(),
                actions: vec![*action],
            }),
        }
    }
    out
}

/// `AssignableRoleEntity` = `RoleEntity` tanpa `status/createdAt/updatedAt` + `assignable`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AssignableRoleEntity {
    pub uuid: String,
    pub code: String,
    pub name: String,
    pub description: Option<String>,
    pub base_role: Option<BaseRole>,
    pub is_system: bool,
    pub is_built_in_override: bool,
    pub built_in_slot: Option<String>,
    pub assignable: bool,
}

#[derive(Debug, Serialize)]
pub struct RoleImpactUser {
    pub uuid: String,
    pub name: String,
    pub username: String,
}

#[derive(Debug, Serialize)]
pub struct RoleImpactEntity {
    pub count: i64,
    pub users: Vec<RoleImpactUser>,
}

#[derive(Debug, Serialize)]
pub struct RoleDeleted {
    pub deleted: bool,
    pub reassigned: i64,
}

#[derive(Debug, Serialize)]
pub struct RoleReverted {
    pub reverted: bool,
    pub reassigned: i64,
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;
    use mz_core::time::now_naive;

    fn role(
        code: &str,
        is_system: bool,
        base: Option<BaseRole>,
        overrides: Option<i32>,
    ) -> AccessLevel {
        AccessLevel {
            id: 1,
            uuid: "u".into(),
            code: code.into(),
            name: "n".into(),
            created_at: JsDate(now_naive()),
            updated_at: None,
            status: 1,
            company_id: None,
            is_system,
            base_role: base,
            description: None,
            created_by_id: None,
            overrides_role_id: overrides,
        }
    }

    #[test]
    fn slots() {
        assert_eq!(
            built_in_slot_of(&role("ADMIN", true, None, None)).as_deref(),
            Some("ADMIN")
        );
        assert_eq!(
            built_in_slot_of(&role("SYSTEM_ADMIN", true, None, None)),
            None
        );
        assert_eq!(
            built_in_slot_of(&role("c1_x", false, Some(BaseRole::STAFF), Some(9))).as_deref(),
            Some("STAFF")
        );
        assert_eq!(
            built_in_slot_of(&role("c1_x", false, Some(BaseRole::STAFF), None)),
            None
        );
    }

    #[test]
    fn detail_key_order() {
        let mut r = role("c1_x", false, Some(BaseRole::MANAGER), None);
        r.created_at = JsDate(chrono_epoch());
        let d = RoleDetailEntity {
            role: RoleEntity::from(&r),
            permissions: group_by_feature(&[
                ("users".into(), PermissionAction::WRITE),
                ("roles".into(), PermissionAction::READ),
                ("users".into(), PermissionAction::READ),
            ]),
        };
        assert_eq!(
            serde_json::to_string(&d).unwrap_or_default(),
            r#"{"uuid":"u","code":"c1_x","name":"n","description":null,"baseRole":"MANAGER","isSystem":false,"isBuiltInOverride":false,"builtInSlot":null,"status":1,"createdAt":"1970-01-01T00:00:00.000Z","updatedAt":null,"permissions":[{"featureKey":"users","actions":["WRITE","READ"]},{"featureKey":"roles","actions":["READ"]}]}"#
        );
    }

    fn chrono_epoch() -> chrono::NaiveDateTime {
        chrono::DateTime::from_timestamp(0, 0)
            .map(|d| d.naive_utc())
            .unwrap_or_default()
    }
}
