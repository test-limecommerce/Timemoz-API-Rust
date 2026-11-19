//! Aturan akses `tracking-timelines.service.ts`: `getCreateOfflineSetting`,
//! `canAccessOwnerData`, `ownerProjectWhere`/`isOwnerProject`,
//! `budgetRoleCodeOf` + `ProjectBudgetService.canStartTracking`,
//! `callerCompanyTimelineWhere`.

use mz_core::company::USER_COMPANY_SCOPE_SQL;
use mz_core::enums::sees_whole_company;
use sqlx::PgPool;

/// `effectiveRoleCodeOf(accessLevel)` dalam SQL (alias `al`).
pub const EFFECTIVE_ROLE_SQL: &str = r#"CASE WHEN al.id IS NULL THEN NULL
    WHEN al."isSystem" THEN al.code ELSE COALESCE(al."baseRole"::text, al.code) END"#;

/// Hasil `getCreateOfflineSetting(userId)` (+ keanggotaan pemilik untuk
/// `canAccessOwnerData`).
#[derive(Debug, sqlx::FromRow)]
pub struct CallerSetting {
    pub is_allow_manual_time: bool,
    pub is_allow_edit_time: bool,
    /// `user.userAccessLevel[0]` ada (tanpa filter status, `id desc`).
    pub has_ual: Option<bool>,
    /// `effectiveRoleCodeOf(userAccessLevel[0].accessLevel)`; `None` bila relasi kosong.
    pub role: Option<String>,
    pub has_access_level: Option<bool>,
    /// `userCompany` pemilik di company pemanggil (tanpa filter status).
    pub owner_member: bool,
}

/// `getCreateOfflineSetting(caller)` + cek keanggotaan `owner` di company yang sama.
/// `None` = pemanggil tak punya keanggotaan aktif (`findFirst` → `null`).
pub async fn caller_setting(
    db: &PgPool,
    caller: i32,
    owner: i32,
) -> sqlx::Result<Option<CallerSetting>> {
    let sql = format!(
        r#"SELECT uc."isAllowManualTime" AS is_allow_manual_time,
             uc."isAllowEditTime" AS is_allow_edit_time, r.has_ual, r.role, r.has_access_level,
             EXISTS(SELECT 1 FROM "userCompany" o WHERE o."userId" = $2 AND o."companyId" = uc."companyId") AS owner_member
           FROM "userCompany" uc
           LEFT JOIN LATERAL (
             SELECT true AS has_ual, (al.id IS NOT NULL) AS has_access_level, {EFFECTIVE_ROLE_SQL} AS role
             FROM "userAccessLevel" ual LEFT JOIN "accessLevel" al ON al.id = ual."accessLevelId"
             WHERE ual."userId" = uc."userId" ORDER BY ual.id DESC LIMIT 1
           ) r ON true
           WHERE {USER_COMPANY_SCOPE_SQL} LIMIT 1"#
    );
    sqlx::query_as::<_, CallerSetting>(&sql)
        .bind(caller)
        .bind(owner)
        .fetch_optional(db)
        .await
}

/// `canAccessOwnerData(ownerUserId, callerId)`.
pub async fn can_access_owner_data(db: &PgPool, owner: i32, caller: i32) -> sqlx::Result<bool> {
    if owner == caller {
        return Ok(true);
    }
    let Some(s) = caller_setting(db, caller, owner).await? else {
        return Ok(false);
    };
    Ok(sees_whole_company(s.role.as_deref()) && s.owner_member)
}

/// Keputusan controller `create-offline` / `update-offline/dashboard`:
/// `flag || seesWholeCompany(effectiveRoleCodeOf(userSetting.user.userAccessLevel[0].accessLevel))`.
/// `Err` = TypeError di Node (`userSetting` null, `userAccessLevel` kosong,
/// atau `accessLevel` null) → 500.
pub fn controller_allows(
    setting: Option<&CallerSetting>,
    flag: impl Fn(&CallerSetting) -> bool,
) -> Result<bool, ()> {
    let s = setting.ok_or(())?;
    if flag(s) {
        return Ok(true);
    }
    if s.has_ual != Some(true) || s.has_access_level != Some(true) {
        return Err(());
    }
    Ok(sees_whole_company(s.role.as_deref()))
}

/// `ownerProjectWhere(owner)` sebagai potongan SQL (alias project `p`, param owner = `$n`).
pub fn owner_project_sql(owner_param: &str) -> String {
    format!(
        r#"EXISTS(SELECT 1 FROM "projectCompany" opc JOIN "userCompany" ouc ON ouc."companyId" = opc."companyId"
             WHERE opc."projectId" = p.id AND ouc."userId" = {owner_param})"#
    )
}

/// Pemeriksaan sesudah pemilik diketahui, dalam SATU round-trip:
/// `isOwnerProject`, role untuk `canStartTracking`, baris budget, dan
/// baris `userWorkTimeLimit` (+ `company.workweekStartDay`) — semua baca murni,
/// dievaluasi pemanggil dalam urutan Node.
#[derive(Debug, sqlx::FromRow)]
pub struct Precheck {
    pub owner_project: bool,
    pub budget_role: Option<String>,
    pub budget_enabled: Option<bool>,
    pub budget_type: Option<String>,
    pub budget_amount: Option<f64>,
    pub current_burned_cost: Option<f64>,
    pub budget_hours: Option<f64>,
    pub current_burned_seconds: Option<i32>,
    pub enforcement: Option<String>,
    pub limit_exists: Option<bool>,
    pub daily_limit_sec: Option<i32>,
    pub weekly_limit_sec: Option<i32>,
    pub workweek_start_day: Option<i32>,
}

pub async fn precheck(
    db: &PgPool,
    project_id: i32,
    owner: i32,
    company_id: Option<i32>,
) -> sqlx::Result<Precheck> {
    let sql = format!(
        r#"SELECT
             EXISTS(SELECT 1 FROM "projectCompany" pc JOIN "userCompany" uc ON uc."companyId" = pc."companyId"
                    WHERE pc."projectId" = $1 AND uc."userId" = $2) AS owner_project,
             (SELECT {EFFECTIVE_ROLE_SQL} FROM "userAccessLevel" ual
                LEFT JOIN "accessLevel" al ON al.id = ual."accessLevelId"
                WHERE ual."userId" = $2 AND ual.status = 1 ORDER BY ual.id DESC LIMIT 1) AS budget_role,
             b."isEnabled" AS budget_enabled, b."budgetType"::text AS budget_type,
             b."budgetAmount" AS budget_amount, b."currentBurnedCost" AS current_burned_cost,
             b."budgetHours" AS budget_hours, b."currentBurnedSeconds" AS current_burned_seconds,
             b.enforcement::text AS enforcement,
             (l.id IS NOT NULL) AS limit_exists, l."dailyLimitSec" AS daily_limit_sec,
             l."weeklyLimitSec" AS weekly_limit_sec, c."workweekStartDay" AS workweek_start_day
           FROM (SELECT 1) one
           LEFT JOIN "projectBudget" b ON b."projectId" = $1
           LEFT JOIN LATERAL (SELECT id, "dailyLimitSec", "weeklyLimitSec" FROM "userWorkTimeLimit"
                WHERE "userId" = $2 AND "companyId" = $3 AND status = 1 LIMIT 1) l ON $3::int IS NOT NULL
           LEFT JOIN company c ON c.id = $3"#
    );
    sqlx::query_as::<_, Precheck>(&sql)
        .bind(project_id)
        .bind(owner)
        .bind(company_id)
        .fetch_one(db)
        .await
}

impl Precheck {
    /// `budgetRoleCodeOf(owner) ?? 'STAFF'` lalu `canStartTracking(projectId, role)`
    /// (keputusan di `mz_projects::budget::can_start_tracking_block`).
    /// `Some(reason)` = ditolak.
    pub fn budget_block(&self) -> Option<&'static str> {
        mz_projects::budget::can_start_tracking_block(
            self.budget_role.as_deref().unwrap_or("STAFF"),
            self.budget_enabled,
            self.budget_type.as_deref(),
            self.budget_amount,
            self.current_burned_cost,
            self.budget_hours,
            self.current_burned_seconds,
            self.enforcement.as_deref(),
        )
    }

    pub fn limit(&self) -> Option<crate::worklimit::Limit> {
        (self.limit_exists == Some(true)).then(|| crate::worklimit::Limit {
            daily: self.daily_limit_sec,
            weekly: self.weekly_limit_sec,
            workweek_start_day: self.workweek_start_day.unwrap_or(1),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setting(manual: bool, has_ual: Option<bool>, role: Option<&str>) -> CallerSetting {
        CallerSetting {
            is_allow_manual_time: manual,
            is_allow_edit_time: false,
            has_ual,
            role: role.map(str::to_owned),
            has_access_level: has_ual.map(|_| role.is_some()),
            owner_member: true,
        }
    }

    #[test]
    fn controller_gate_mirrors_type_errors() {
        assert_eq!(controller_allows(None, |s| s.is_allow_manual_time), Err(()));
        // Flag benar: `userAccessLevel[0]` tidak pernah dibaca.
        let s = setting(true, None, None);
        assert_eq!(
            controller_allows(Some(&s), |s| s.is_allow_manual_time),
            Ok(true)
        );
        let s = setting(false, None, None);
        assert_eq!(
            controller_allows(Some(&s), |s| s.is_allow_manual_time),
            Err(())
        );
        let s = setting(false, Some(true), Some("MANAGER"));
        assert_eq!(
            controller_allows(Some(&s), |s| s.is_allow_manual_time),
            Ok(true)
        );
        let s = setting(false, Some(true), Some("STAFF"));
        assert_eq!(
            controller_allows(Some(&s), |s| s.is_allow_manual_time),
            Ok(false)
        );
    }

    fn budget(role: Option<&str>, hours: f64, burned: i32, enforcement: &str) -> Precheck {
        Precheck {
            owner_project: true,
            budget_role: role.map(str::to_owned),
            budget_enabled: Some(true),
            budget_type: Some("HOURS".into()),
            budget_amount: None,
            current_burned_cost: Some(0.0),
            budget_hours: Some(hours),
            current_burned_seconds: Some(burned),
            enforcement: Some(enforcement.into()),
            limit_exists: Some(false),
            daily_limit_sec: None,
            weekly_limit_sec: None,
            workweek_start_day: None,
        }
    }

    #[test]
    fn budget_gate_only_blocks_staff_at_stop_tracking() {
        assert!(
            budget(None, 1.0, 3600, "STOP_TRACKING")
                .budget_block()
                .is_some()
        );
        assert!(
            budget(Some("STAFF"), 1.0, 3599, "STOP_TRACKING")
                .budget_block()
                .is_none()
        );
        assert!(
            budget(Some("ADMIN"), 1.0, 7200, "STOP_TRACKING")
                .budget_block()
                .is_none()
        );
        assert!(
            budget(Some("STAFF"), 1.0, 7200, "ALERT_ONLY")
                .budget_block()
                .is_none()
        );
        assert!(
            budget(Some("STAFF"), 0.0, 7200, "STOP_TRACKING")
                .budget_block()
                .is_none()
        );
    }
}
