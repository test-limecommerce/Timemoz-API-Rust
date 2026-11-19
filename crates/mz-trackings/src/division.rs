//! Port bagian `divisions/*` yang dipanggil route trackings:
//! `division-scope.ts` (`resolveViewerScope`, `canViewUser`, `canViewUserData`,
//! `leaderMayReadFeature`, `DivisionScopeForbiddenException`),
//! `division-gate.service.ts` (`isOpen`) dan `plan-entitlements.ts`
//! (`hasPlanFeature(DIVISIONS)`), serta `permissions/effective-role.ts`.

use axum::http::StatusCode;
use mz_core::enums::{BaseRole, PermissionAction, effective_role_code, sees_whole_company};
use mz_core::permissions::{EffectiveAccess, effective_access};
use mz_core::{ApiError, ApiResult, AppState};
use sqlx::PgPool;

pub const DIVISION_SCOPE_FORBIDDEN: &str = "DIVISION_SCOPE_FORBIDDEN";
pub const USER_DATA_FORBIDDEN: &str = "USER_DATA_FORBIDDEN";
const ACTIVE: i32 = 1;

/// `ViewerScope` (jangkauan daftar divisi tidak dipakai route trackings).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewerScope {
    Company,
    Division { user_ids: Vec<i32> },
    SelfOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserDataDecision {
    Allow,
    DenyLeader,
    Deny,
}

/// `new DivisionScopeForbiddenException()` -> 403 `{code, message}`.
pub fn scope_forbidden() -> ApiError {
    ApiError::coded(
        StatusCode::FORBIDDEN,
        DIVISION_SCOPE_FORBIDDEN,
        "This employee is outside your division",
    )
}

/// `new ForbiddenException({ code: USER_DATA_FORBIDDEN, ... })`.
pub fn user_data_forbidden() -> ApiError {
    ApiError::coded(
        StatusCode::FORBIDDEN,
        USER_DATA_FORBIDDEN,
        "You are not allowed to view this employee data",
    )
}

/// `isLeaderEligibleRoleCode`: hanya STAFF efektif (GUEST dan lainnya tidak).
pub fn is_leader_eligible_role_code(code: Option<&str>) -> bool {
    code == Some("STAFF")
}

/// `canViewUser`.
pub fn can_view_user(scope: &ViewerScope, viewer: i32, target: i32) -> bool {
    match scope {
        ViewerScope::Company => true,
        ViewerScope::Division { user_ids } => user_ids.contains(&target),
        ViewerScope::SelfOnly => target == viewer,
    }
}

/// `leaderMayReadFeature`: tanpa akses (`null`) atau role sistem = boleh; role
/// custom wajib punya `READ` pada fitur itu.
pub fn leader_may_read_feature(access: Option<&EffectiveAccess>, feature: &str) -> bool {
    match access {
        None => true,
        Some(a) if a.is_system => true,
        Some(a) => a
            .permissions
            .get(feature)
            .is_some_and(|set| set.contains(&PermissionAction::READ)),
    }
}

/// `leaderMayReadScreenshots` (FR-024).
pub async fn leader_may_read_screenshots(
    state: &AppState,
    viewer: i32,
    company: i32,
) -> ApiResult<bool> {
    let access = effective_access(state, viewer, company).await?;
    Ok(leader_may_read_feature(access.as_deref(), "screenshots"))
}

/// `effectiveRoleCodeOf` dari kolom mentah `accessLevel`.
pub fn effective_code(code: &str, is_system: bool, base_role: Option<&str>) -> String {
    let base = base_role.and_then(|b| b.parse::<BaseRole>().ok());
    effective_role_code(code, is_system, base)
}

type RoleRow = (String, bool, Option<String>, i32);

/// `roleCodeInCompany`: role aktif di company itu; role custom nonaktif = `None`.
pub async fn role_code_in_company(
    db: &PgPool,
    user: i32,
    company: i32,
) -> sqlx::Result<Option<String>> {
    let row: Option<RoleRow> = sqlx::query_as(
        r#"SELECT al.code, al."isSystem", al."baseRole"::text, al.status
           FROM "userAccessLevel" ual
           JOIN "accessLevel" al ON al.id = ual."accessLevelId"
           WHERE ual."userId" = $1 AND ual."companyId" = $2 AND ual.status = 1
           ORDER BY ual.id DESC
           LIMIT 1"#,
    )
    .bind(user)
    .bind(company)
    .fetch_optional(db)
    .await?;
    Ok(row.and_then(|(code, is_system, base, status)| {
        if !is_system && status != ACTIVE {
            None
        } else {
            Some(effective_code(&code, is_system, base.as_deref()))
        }
    }))
}

/// Role efektif dari penugasan `userAccessLevel` TERBARU milik user di company
/// mana pun dan status apa pun (`userAccessLevel: { take: 1, orderBy: { id: 'desc' } }`
/// di `findAll`).
pub async fn latest_role_code_any(db: &PgPool, user: i32) -> sqlx::Result<Option<String>> {
    let row: Option<(String, bool, Option<String>)> = sqlx::query_as(
        r#"SELECT al.code, al."isSystem", al."baseRole"::text
           FROM "userAccessLevel" ual
           JOIN "accessLevel" al ON al.id = ual."accessLevelId"
           WHERE ual."userId" = $1
           ORDER BY ual.id DESC
           LIMIT 1"#,
    )
    .bind(user)
    .fetch_optional(db)
    .await?;
    Ok(row.map(|(code, is_system, base)| effective_code(&code, is_system, base.as_deref())))
}

/// `plan-entitlements.ts`: `divisions` = override ?? paket; boost SALES atau
/// TIME yang aktif menyalakannya.
pub fn divisions_entitled(tier: bool, override_value: Option<bool>, boosted: bool) -> bool {
    override_value.unwrap_or(tier) || boosted
}

/// `hasPlanFeature(DIVISIONS)`; company tanpa langganan berjalan = terbuka.
pub async fn plan_has_divisions(db: &PgPool, company: i32) -> sqlx::Result<bool> {
    // Langganan berjalan (ACTIVE/TRIAL), terbaru menang.
    let running: Option<(Option<bool>, Option<bool>, bool)> = sqlx::query_as(
        r#"SELECT pt.divisions,
                  (SELECT po.divisions FROM "companyPlanOverride" po WHERE po."companyId" = s."companyId"),
                  EXISTS (SELECT 1 FROM "companyPlanBoost" pb
                          WHERE pb."companyId" = s."companyId" AND pb.enabled = true
                            AND pb.product::text IN ('SALES', 'TIME'))
           FROM subscriptions s
           LEFT JOIN "pricingTiers" pt ON pt.id = s."pricingTierId"
           WHERE s."companyId" = $1 AND s.status::text IN ('ACTIVE', 'TRIAL')
           ORDER BY s."startDate" DESC, s.id DESC
           LIMIT 1"#,
    )
    .bind(company)
    .fetch_optional(db)
    .await?;
    if let Some((tier, ovr, boosted)) = running {
        return Ok(match tier {
            // `!subscription.pricingTier` -> null -> terbuka.
            None => true,
            Some(t) => divisions_entitled(t, ovr, boosted),
        });
    }
    // Masa tenggang MANUAL yang sudah INACTIVE: paket + override, tanpa boost.
    let latest: Option<(String, String, Option<bool>, Option<bool>)> = sqlx::query_as(
        r#"SELECT s."billingSource"::text, s.status::text, pt.divisions,
                  (SELECT po.divisions FROM "companyPlanOverride" po WHERE po."companyId" = s."companyId")
           FROM subscriptions s
           LEFT JOIN "pricingTiers" pt ON pt.id = s."pricingTierId"
           WHERE s."companyId" = $1
           ORDER BY s."startDate" DESC, s.id DESC
           LIMIT 1"#,
    )
    .bind(company)
    .fetch_optional(db)
    .await?;
    Ok(match latest {
        Some((source, status, Some(tier), ovr)) if source == "MANUAL" && status == "INACTIVE" => {
            divisions_entitled(tier, ovr, false)
        }
        _ => true,
    })
}

/// `DivisionGateService.isOpen` (tanpa cache 60 dtk; lihat deviations).
pub async fn gate_is_open(db: &PgPool, company: i32) -> sqlx::Result<bool> {
    let row: Option<(bool, bool)> = sqlx::query_as(
        r#"SELECT "divisionsEnabled", "moduleTimeTrackingEnabled" FROM company WHERE id = $1"#,
    )
    .bind(company)
    .fetch_optional(db)
    .await?;
    let (divisions_enabled, module_open) = row.unwrap_or((false, false));
    Ok(divisions_enabled && module_open && plan_has_divisions(db, company).await?)
}

/// Divisi hidup yang dipimpin viewer (untuk daftar/my-scope di mz-divisions).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedDivision {
    pub id: i32,
    pub uuid: String,
    pub name: String,
}

/// `resolveViewerScope`.
pub async fn resolve_viewer_scope(
    db: &PgPool,
    viewer: i32,
    company: i32,
) -> sqlx::Result<ViewerScope> {
    Ok(resolve_viewer_scope_with_divisions(db, viewer, company)
        .await?
        .0)
}

/// `resolveViewerScope` + daftar divisi yang dipimpin (id/uuid/nama; urut nama lalu id, tanpa
/// duplikat id; kosong kecuali hasilnya `Division`). `resolve_viewer_scope` hanya memanggil ini
/// dan membuang daftarnya, jadi `user_ids` identik di kedua pemanggil.
pub async fn resolve_viewer_scope_with_divisions(
    db: &PgPool,
    viewer: i32,
    company: i32,
) -> sqlx::Result<(ViewerScope, Vec<LedDivision>)> {
    let role = role_code_in_company(db, viewer, company).await?;
    if sees_whole_company(role.as_deref()) {
        return Ok((ViewerScope::Company, Vec::new()));
    }
    if !gate_is_open(db, company).await? {
        return Ok((ViewerScope::SelfOnly, Vec::new()));
    }
    if !is_leader_eligible_role_code(role.as_deref()) {
        return Ok((ViewerScope::SelfOnly, Vec::new()));
    }

    // Divisi yang dipimpin viewer (dibaca tiap request, tanpa cache: FR-021).
    let rows: Vec<(i32, String, String)> = sqlx::query_as(
        r#"SELECT d.id, d.uuid::text, d.name
           FROM "divisionMember" dm
           JOIN division d ON d.id = dm."divisionId" AND d."deletedAt" IS NULL
           JOIN users u ON u.id = dm."userId"
           WHERE dm."companyId" = $1 AND dm."userId" = $2
             AND dm.role::text = 'LEADER' AND dm."deletedAt" IS NULL
             AND u.status = 1
             AND EXISTS (SELECT 1 FROM "userCompany" uc
                         WHERE uc."userId" = u.id AND uc."companyId" = $1 AND uc.status = 1)
           ORDER BY d.name ASC, dm."divisionId" ASC"#,
    )
    .bind(company)
    .bind(viewer)
    .fetch_all(db)
    .await?;
    let mut divisions: Vec<LedDivision> = Vec::new();
    for (id, uuid, name) in rows {
        if divisions.iter().any(|d| d.id == id) {
            continue;
        }
        divisions.push(LedDivision { id, uuid, name });
    }
    if divisions.is_empty() {
        return Ok((ViewerScope::SelfOnly, Vec::new()));
    }

    let division_ids: Vec<i32> = divisions.iter().map(|d| d.id).collect();
    let members: Vec<i32> = sqlx::query_scalar(
        r#"SELECT dm."userId"
           FROM "divisionMember" dm
           JOIN users u ON u.id = dm."userId"
           WHERE dm."companyId" = $1 AND dm."divisionId" = ANY($2) AND dm."deletedAt" IS NULL
             AND u.status = 1
             AND EXISTS (SELECT 1 FROM "userCompany" uc
                         WHERE uc."userId" = u.id AND uc."companyId" = $1 AND uc.status = 1)"#,
    )
    .bind(company)
    .bind(&division_ids)
    .fetch_all(db)
    .await?;

    Ok((
        ViewerScope::Division {
            user_ids: dedupe_with_viewer(viewer, &members),
        },
        divisions,
    ))
}

/// Leader selalu termasuk; dedupe dengan urutan kemunculan (FR-018).
pub fn dedupe_with_viewer(viewer: i32, members: &[i32]) -> Vec<i32> {
    let mut out = vec![viewer];
    for m in members {
        if !out.contains(m) {
            out.push(*m);
        }
    }
    out
}

/// `canViewUserData` (dengan callback `leaderMayRead` = screenshots:READ).
pub async fn can_view_user_data(
    state: &AppState,
    viewer: i32,
    target: i32,
) -> ApiResult<UserDataDecision> {
    if viewer == target {
        return Ok(UserDataDecision::Allow);
    }
    // `requireCompanyId` -> 403 'No company found for user'.
    let company = mz_core::company::require_company_id(&state.db, viewer).await?;

    let membership: Option<i32> = sqlx::query_scalar(
        r#"SELECT id FROM "userCompany" WHERE "userId" = $1 AND "companyId" = $2 LIMIT 1"#,
    )
    .bind(target)
    .bind(company)
    .fetch_optional(&state.db)
    .await?;
    if membership.is_none() {
        return Ok(UserDataDecision::Deny);
    }

    let scope = resolve_viewer_scope(&state.db, viewer, company).await?;
    decide(&scope, viewer, target, || {
        leader_may_read_screenshots(state, viewer, company)
    })
    .await
}

/// Cabang keputusan `canViewUserData` setelah scope diketahui (dipisah agar teruji).
pub async fn decide<F, Fut>(
    scope: &ViewerScope,
    viewer: i32,
    target: i32,
    leader_may_read: F,
) -> ApiResult<UserDataDecision>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = ApiResult<bool>>,
{
    Ok(match scope {
        ViewerScope::Company => UserDataDecision::Allow,
        ViewerScope::Division { .. } => {
            if !leader_may_read().await? {
                UserDataDecision::Deny
            } else if can_view_user(scope, viewer, target) {
                UserDataDecision::Allow
            } else {
                UserDataDecision::DenyLeader
            }
        }
        ViewerScope::SelfOnly => UserDataDecision::Deny,
    })
}

/// `TrackingService.canAccessUser` (jalur tulis).
pub async fn can_access_user(db: &PgPool, caller: i32, target: i32) -> sqlx::Result<bool> {
    if caller == target {
        return Ok(true);
    }
    let Some(company) = mz_core::company::find_company_id(db, caller).await? else {
        return Ok(false);
    };
    let role: Option<(String, bool, Option<String>)> = sqlx::query_as(
        r#"SELECT al.code, al."isSystem", al."baseRole"::text
           FROM "userAccessLevel" ual
           JOIN "accessLevel" al ON al.id = ual."accessLevelId"
           WHERE ual."userId" = $1 AND ual."companyId" = $2 AND ual.status = 1
           ORDER BY ual.id DESC
           LIMIT 1"#,
    )
    .bind(caller)
    .bind(company)
    .fetch_optional(db)
    .await?;
    let code = role.map(|(c, s, b)| effective_code(&c, s, b.as_deref()));
    if !sees_whole_company(code.as_deref()) {
        return Ok(false);
    }
    let membership: Option<i32> = sqlx::query_scalar(
        r#"SELECT id FROM "userCompany"
           WHERE "userId" = $1 AND "companyId" = $2 AND status = 1 LIMIT 1"#,
    )
    .bind(target)
    .bind(company)
    .fetch_optional(db)
    .await?;
    Ok(membership.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    fn access(is_system: bool, perms: &[(&str, PermissionAction)]) -> EffectiveAccess {
        let mut permissions: HashMap<String, HashSet<PermissionAction>> = HashMap::new();
        for (f, a) in perms {
            permissions.entry((*f).to_owned()).or_default().insert(*a);
        }
        EffectiveAccess {
            role_id: 1,
            role_code: "X".into(),
            role_name: "X".into(),
            is_system,
            base_role: None,
            effective_role_code: "STAFF".into(),
            company_id: 1,
            permissions,
        }
    }

    #[test]
    fn view_scope_rules() {
        let div = ViewerScope::Division {
            user_ids: vec![5, 7],
        };
        assert!(can_view_user(&ViewerScope::Company, 1, 99));
        assert!(can_view_user(&div, 5, 7));
        assert!(!can_view_user(&div, 5, 8));
        assert!(can_view_user(&ViewerScope::SelfOnly, 3, 3));
        assert!(!can_view_user(&ViewerScope::SelfOnly, 3, 4));
    }

    #[test]
    fn leader_feature_gate() {
        assert!(leader_may_read_feature(None, "screenshots"));
        assert!(leader_may_read_feature(
            Some(&access(true, &[])),
            "screenshots"
        ));
        assert!(!leader_may_read_feature(
            Some(&access(false, &[])),
            "screenshots"
        ));
        assert!(!leader_may_read_feature(
            Some(&access(false, &[("screenshots", PermissionAction::WRITE)])),
            "screenshots"
        ));
        assert!(leader_may_read_feature(
            Some(&access(false, &[("screenshots", PermissionAction::READ)])),
            "screenshots"
        ));
    }

    #[test]
    fn leader_eligibility_is_whitelist() {
        assert!(is_leader_eligible_role_code(Some("STAFF")));
        assert!(!is_leader_eligible_role_code(Some("GUEST")));
        assert!(!is_leader_eligible_role_code(Some("c1_custom")));
        assert!(!is_leader_eligible_role_code(None));
    }

    #[test]
    fn plan_entitlement_precedence() {
        assert!(!divisions_entitled(false, None, false));
        assert!(divisions_entitled(true, None, false));
        assert!(!divisions_entitled(true, Some(false), false));
        assert!(divisions_entitled(false, Some(false), true));
    }

    #[test]
    fn viewer_listed_first_and_deduped() {
        assert_eq!(dedupe_with_viewer(1, &[2, 1, 3, 2]), vec![1, 2, 3]);
    }

    #[test]
    fn effective_code_uses_base_role_for_custom() {
        assert_eq!(effective_code("c1_x", false, Some("MANAGER")), "MANAGER");
        assert_eq!(effective_code("ADMIN", true, Some("STAFF")), "ADMIN");
        assert_eq!(effective_code("c1_x", false, None), "c1_x");
    }

    #[tokio::test]
    async fn decision_branches() {
        let div = ViewerScope::Division {
            user_ids: vec![1, 2],
        };
        let ok = |v: bool| move || async move { Ok::<bool, ApiError>(v) };
        assert_eq!(
            decide(&ViewerScope::Company, 1, 9, ok(false)).await.ok(),
            Some(UserDataDecision::Allow)
        );
        assert_eq!(
            decide(&ViewerScope::SelfOnly, 1, 2, ok(true)).await.ok(),
            Some(UserDataDecision::Deny)
        );
        assert_eq!(
            decide(&div, 1, 2, ok(true)).await.ok(),
            Some(UserDataDecision::Allow)
        );
        assert_eq!(
            decide(&div, 1, 9, ok(true)).await.ok(),
            Some(UserDataDecision::DenyLeader)
        );
        assert_eq!(
            decide(&div, 1, 2, ok(false)).await.ok(),
            Some(UserDataDecision::Deny)
        );
    }
}
