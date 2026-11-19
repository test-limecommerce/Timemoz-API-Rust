//! Port `store-visit-form-revise-policy.service.ts`: siapa yang boleh mengoreksi
//! submission final (docs/store_visit_form_revisers.md §4).
//!
//! HANYA mempersempit gerbang `@Roles` admin. Keputusan diambil ulang tiap request dan
//! TIDAK di-cache (§0.12) — Node pun tidak punya cache di sini (`onModuleInit` Node hanya
//! memeriksa Prisma Client usang; tidak relevan di Rust).

use axum::http::StatusCode;
use mz_core::ApiResult;
use mz_core::enums::{rank_of, sees_whole_company};
use serde_json::json;
use sqlx::PgConnection;

use crate::errors::coded_details;
use crate::revise_mode::ReviseMode;

/// `userAccessLevel.status` / `accessLevel.status`: 1 = aktif.
pub const ACTIVE: i32 = 1;

/// `REVISE_BYPASS_ROLES` — hanya SYSTEM_ADMIN (§0.8, §0.9).
pub const REVISE_BYPASS_ROLES: &[&str] = &["SYSTEM_ADMIN"];

/// `ReviseReason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviseReason {
    Bypass,
    ModeAll,
    Listed,
    NotListed,
    RoleIneligible,
    NotMember,
}

impl ReviseReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bypass => "bypass",
            Self::ModeAll => "mode-all",
            Self::Listed => "listed",
            Self::NotListed => "not-listed",
            Self::RoleIneligible => "role-ineligible",
            Self::NotMember => "not-member",
        }
    }
}

/// `ReviseDecision`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReviseDecision {
    pub allowed: bool,
    pub reason: ReviseReason,
}

impl ReviseDecision {
    const fn allow(reason: ReviseReason) -> Self {
        Self {
            allowed: true,
            reason,
        }
    }
    const fn deny(reason: ReviseReason) -> Self {
        Self {
            allowed: false,
            reason,
        }
    }
}

/// Satu `accessLevel` (`{ id, name, status, ...EFFECTIVE_ROLE_SELECT }`). `None` di
/// pemanggil = relasi `accessLevel` kosong.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct RoleRow {
    pub id: i32,
    pub name: String,
    pub status: i32,
    pub code: String,
    #[sqlx(rename = "isSystem")]
    pub is_system: bool,
    /// Nilai enum `BaseRole` mentah (`baseRole::text`).
    #[sqlx(rename = "baseRole")]
    pub base_role: Option<String>,
}

/// `effectiveRoleCodeOf(role)` — `isSystem ? code : baseRole ?? code`.
pub fn effective_role_code_of(role: &RoleRow) -> String {
    if role.is_system {
        return role.code.clone();
    }
    role.base_role.clone().unwrap_or_else(|| role.code.clone())
}

/// `pickRole(roles)` — role aktif dengan jabatan efektif TERTINGGI (seri: yang pertama).
pub fn pick_role<'a>(roles: impl IntoIterator<Item = Option<&'a RoleRow>>) -> Option<&'a RoleRow> {
    let mut best: Option<(&RoleRow, i32)> = None;
    for role in roles.into_iter().flatten() {
        if role.status != ACTIVE {
            continue;
        }
        let rank = rank_of(&effective_role_code_of(role)).map_or(-1, i32::from);
        if best.is_none_or(|(_, r)| rank > r) {
            best = Some((role, rank));
        }
    }
    best.map(|(r, _)| r)
}

/// Langkah 1–3 §4.2 (murni). `Some(decision)` = final; `None` = lanjut ke `mode`.
pub fn decide_by_role(roles: &[Option<RoleRow>]) -> Option<ReviseDecision> {
    if roles.is_empty() {
        return Some(ReviseDecision::deny(ReviseReason::NotMember));
    }
    let Some(role) = pick_role(roles.iter().map(Option::as_ref)) else {
        return Some(ReviseDecision::deny(ReviseReason::RoleIneligible));
    };
    let code = effective_role_code_of(role);
    if REVISE_BYPASS_ROLES.contains(&code.as_str()) {
        return Some(ReviseDecision::allow(ReviseReason::Bypass));
    }
    if !sees_whole_company(Some(&code)) {
        return Some(ReviseDecision::deny(ReviseReason::RoleIneligible));
    }
    None
}

/// Langkah 4: hanya `allPrivileged` yang melewatkan; kolom kosong / company hilang /
/// nilai asing → pemeriksaan daftar.
pub fn mode_passes(mode: Option<&str>) -> bool {
    mode.and_then(ReviseMode::parse) == Some(ReviseMode::AllPrivileged)
}

/// Langkah 5.
pub fn decide_by_list(listed: bool) -> ReviseDecision {
    if listed {
        ReviseDecision::allow(ReviseReason::Listed)
    } else {
        ReviseDecision::deny(ReviseReason::NotListed)
    }
}

/// Penugasan role aktif user di company (`userAccessLevel.findMany` + `accessLevel`).
/// `None` per baris = relasi `accessLevel` kosong.
pub async fn load_member_roles(
    conn: &mut PgConnection,
    user_id: i32,
    company_id: i32,
) -> sqlx::Result<Vec<Option<RoleRow>>> {
    #[derive(sqlx::FromRow)]
    struct LoadedRole {
        id: Option<i32>,
        name: Option<String>,
        status: Option<i32>,
        code: Option<String>,
        #[sqlx(rename = "isSystem")]
        is_system: Option<bool>,
        #[sqlx(rename = "baseRole")]
        base_role: Option<String>,
    }
    let rows: Vec<LoadedRole> = sqlx::query_as(
        r#"SELECT al.id, al.name, al.status, al.code, al."isSystem", al."baseRole"::text AS "baseRole"
             FROM "userAccessLevel" ual
             LEFT JOIN "accessLevel" al ON al.id = ual."accessLevelId"
            WHERE ual."userId" = $1 AND ual."companyId" = $2 AND ual.status = 1
            ORDER BY ual.id"#,
    )
    .bind(user_id)
    .bind(company_id)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            Some(RoleRow {
                id: r.id?,
                name: r.name?,
                status: r.status?,
                code: r.code?,
                is_system: r.is_system?,
                base_role: r.base_role,
            })
        })
        .collect())
}

/// `canRevise(client, userId, companyId)` — pemanggil jalur tulis WAJIB mengoper
/// koneksi transaksinya (§4.4).
/// Node: src/store-visit-forms/store-visit-form-revise-policy.service.ts:148
pub async fn can_revise(
    conn: &mut PgConnection,
    user_id: i32,
    company_id: i32,
) -> sqlx::Result<ReviseDecision> {
    let roles = load_member_roles(conn, user_id, company_id).await?;
    if let Some(decision) = decide_by_role(&roles) {
        return Ok(decision);
    }
    let mode: Option<String> =
        sqlx::query_scalar(r#"SELECT "storeVisitFormReviseMode"::text FROM company WHERE id = $1"#)
            .bind(company_id)
            .fetch_optional(&mut *conn)
            .await?
            .flatten();
    if mode_passes(mode.as_deref()) {
        return Ok(ReviseDecision::allow(ReviseReason::ModeAll));
    }
    let listed: Option<i32> = sqlx::query_scalar(
        r#"SELECT id FROM "storeVisitFormReviser"
            WHERE "companyId" = $1 AND "userId" = $2 AND "deletedAt" IS NULL
            LIMIT 1"#,
    )
    .bind(company_id)
    .bind(user_id)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(decide_by_list(listed.is_some()))
}

/// 403 `FORM_REVISE_FORBIDDEN` dengan `details.reason` (§5.4).
pub fn forbidden(decision: ReviseDecision) -> mz_core::ApiError {
    coded_details(
        StatusCode::FORBIDDEN,
        "FORM_REVISE_FORBIDDEN",
        "You are not allowed to correct form submissions",
        json!({ "reason": decision.reason.as_str() }),
    )
}

/// `assertCanRevise(client, userId, companyId)`.
/// Node: src/store-visit-forms/store-visit-form-revise-policy.service.ts:226
pub async fn assert_can_revise(
    conn: &mut PgConnection,
    user_id: i32,
    company_id: i32,
) -> ApiResult<()> {
    let decision = can_revise(conn, user_id, company_id).await?;
    if decision.allowed {
        Ok(())
    } else {
        Err(forbidden(decision))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::revise_mode::STORE_VISIT_FORM_REVISE_MODES;

    fn system(code: &str) -> Option<RoleRow> {
        Some(RoleRow {
            id: 1,
            name: code.to_owned(),
            status: 1,
            code: code.to_owned(),
            is_system: true,
            base_role: None,
        })
    }

    fn custom(base: Option<&str>, status: i32) -> Option<RoleRow> {
        Some(RoleRow {
            id: 9,
            name: "Supervisor".into(),
            status,
            code: "custom_supervisor".into(),
            is_system: false,
            base_role: base.map(str::to_owned),
        })
    }

    /// Simulasi `canRevise` penuh tanpa DB.
    fn decide(roles: &[Option<RoleRow>], mode: Option<&str>, listed: bool) -> ReviseDecision {
        decide_by_role(roles).unwrap_or_else(|| {
            if mode_passes(mode) {
                ReviseDecision::allow(ReviseReason::ModeAll)
            } else {
                decide_by_list(listed)
            }
        })
    }

    #[test]
    fn modes_match_prisma_enum() {
        let schema = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../db/schema.prisma"
        ));
        if let Ok(schema) = schema {
            let block = schema
                .split("enum StoreVisitFormReviseMode {")
                .nth(1)
                .and_then(|b| b.split('}').next())
                .unwrap();
            let values: Vec<&str> = block
                .lines()
                .map(|l| l.split("//").next().unwrap().trim())
                .filter(|l| !l.is_empty())
                .collect();
            assert_eq!(values, STORE_VISIT_FORM_REVISE_MODES);
        }
        assert!(crate::revise_mode::is_store_visit_form_revise_mode(Some(
            &json!("selected")
        )));
        assert!(!crate::revise_mode::is_store_visit_form_revise_mode(Some(
            &json!("SELECTED")
        )));
        assert!(!crate::revise_mode::is_store_visit_form_revise_mode(Some(
            &json!(1)
        )));
        assert!(!crate::revise_mode::is_store_visit_form_revise_mode(None));
    }

    #[test]
    fn bypass_only_system_admin() {
        assert_eq!(REVISE_BYPASS_ROLES, ["SYSTEM_ADMIN"]);
    }

    #[test]
    fn seven_branches() {
        use ReviseReason::*;
        assert_eq!(decide(&[], Some("allPrivileged"), true).reason, NotMember);
        let d = decide(&[system("SYSTEM_ADMIN")], Some("selected"), false);
        assert_eq!((d.allowed, d.reason), (true, Bypass));
        let d = decide(&[system("STAFF")], Some("allPrivileged"), true);
        assert_eq!((d.allowed, d.reason), (false, RoleIneligible));
        let d = decide(&[system("MANAGER")], Some("allPrivileged"), false);
        assert_eq!((d.allowed, d.reason), (true, ModeAll));
        let d = decide(&[system("ADMIN")], Some("selected"), true);
        assert_eq!((d.allowed, d.reason), (true, Listed));
        let d = decide(&[system("ADMIN")], Some("selected"), false);
        assert_eq!((d.allowed, d.reason), (false, NotListed));
        let d = decide(&[system("SUPER_ADMIN")], Some("selected"), false);
        assert_eq!((d.allowed, d.reason), (false, NotListed));
    }

    #[test]
    fn custom_roles() {
        use ReviseReason::*;
        assert_eq!(
            decide(&[custom(Some("MANAGER"), 1)], Some("allPrivileged"), false).reason,
            ModeAll
        );
        assert_eq!(
            decide(&[custom(Some("ADMIN"), 1)], Some("allPrivileged"), false).reason,
            ModeAll
        );
        assert_eq!(
            decide(&[custom(Some("STAFF"), 1)], Some("allPrivileged"), true).reason,
            RoleIneligible
        );
        // baseRole rusak → kode mentah `custom_supervisor` tidak dikenal → ditolak.
        assert_eq!(
            decide(&[custom(None, 1)], Some("allPrivileged"), true).reason,
            RoleIneligible
        );
        // Role custom nonaktif tidak memberi apa pun.
        assert_eq!(
            decide(&[custom(Some("ADMIN"), 0)], Some("allPrivileged"), true).reason,
            RoleIneligible
        );
    }

    #[test]
    fn duplicates_and_missing_relation() {
        use ReviseReason::*;
        let d = decide(
            &[system("STAFF"), system("ADMIN")],
            Some("allPrivileged"),
            false,
        );
        assert_eq!(d.reason, ModeAll);
        let rows = [system("STAFF"), system("ADMIN"), system("ADMIN")];
        let picked = pick_role(rows.iter().map(Option::as_ref));
        assert_eq!(picked.unwrap().code, "ADMIN");
        assert_eq!(
            decide(&[None], Some("allPrivileged"), true).reason,
            RoleIneligible
        );
    }

    #[test]
    fn unknown_mode_is_narrower() {
        use ReviseReason::*;
        assert_eq!(
            decide(&[system("ADMIN")], Some("everyone"), false).reason,
            NotListed
        );
        assert_eq!(decide(&[system("ADMIN")], None, false).reason, NotListed);
        assert_eq!(decide(&[system("ADMIN")], None, true).reason, Listed);
    }

    #[test]
    fn forbidden_body() {
        let e = forbidden(ReviseDecision::deny(ReviseReason::RoleIneligible));
        let mz_core::ApiError::Body { status, body } = e else {
            panic!("bentuk galat");
        };
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(
            body,
            json!({
                "code": "FORM_REVISE_FORBIDDEN",
                "message": "You are not allowed to correct form submissions",
                "details": { "reason": "role-ineligible" }
            })
        );
    }
}
