//! Port `src/permissions/role-summary.ts` — objek `role` (§7.2) di respons login/refresh/me.

use mz_core::enums::BaseRole;
use serde::Serialize;

/// Bentuk persis `RoleSummary`: `{ uuid, code, name, isSystem, baseRole }`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RoleSummary {
    pub uuid: String,
    pub code: String,
    pub name: String,
    pub is_system: bool,
    pub base_role: Option<BaseRole>,
}

/// Baris `accessLevel` minimum untuk [`role_summary_of`] (juga dipakai sebagai baris query).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct RoleSummaryRow {
    pub uuid: String,
    pub code: String,
    pub name: String,
    #[sqlx(rename = "isSystem")]
    pub is_system: bool,
    #[sqlx(rename = "baseRole")]
    pub base_role: Option<BaseRole>,
}

/// SQL fragment kolom untuk memuat [`RoleSummaryRow`] dari alias `al` (`accessLevel`).
pub const ROLE_SUMMARY_COLUMNS: &str = r#"al.uuid::text AS "uuid", al.code, al.name, al."isSystem", al."baseRole"::text AS "baseRole""#;

/// `roleSummaryOf`: `None` kalau user tidak punya penugasan aktif.
pub fn role_summary_of(row: Option<&RoleSummaryRow>) -> Option<RoleSummary> {
    row.map(|r| RoleSummary {
        uuid: r.uuid.clone(),
        code: r.code.clone(),
        name: r.name.clone(),
        is_system: r.is_system,
        base_role: r.base_role,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shape_matches_node() {
        let row = RoleSummaryRow {
            uuid: "u".into(),
            code: "STAFF".into(),
            name: "Staff".into(),
            is_system: true,
            base_role: None,
        };
        let v = serde_json::to_value(role_summary_of(Some(&row))).unwrap_or_default();
        assert_eq!(
            v,
            serde_json::json!({"uuid":"u","code":"STAFF","name":"Staff","isSystem":true,"baseRole":null})
        );
        assert!(role_summary_of(None).is_none());
    }
}
