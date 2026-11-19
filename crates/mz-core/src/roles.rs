//! Port `src/roles/roles.guard.ts` + `src/permissions/role-cache.ts`.
//!
//! Untuk enam role bawaan, keputusan dan BUNYI PESANNYA identik dengan guard
//! lama (kontrak klien): `401 'Access denied: insufficient permissions'`.
//! Cabang baru (role custom) selalu 403 — 401 membuat dashboard logout.

use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use sqlx::PgExecutor;

use crate::cache::{EpochCache, Keep, shared_err};
use crate::enums::{BaseRole, Role, effective_role_code, rank_of};
use crate::{ApiError, ApiResult, AppState};

pub const ROLE_CACHE_TTL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Default)]
pub struct RoleAccessSnapshot {
    /// `code` role BAWAAN saja.
    pub codes: Vec<String>,
    /// Plafon role custom tertinggi di company ini.
    pub custom_base_role: Option<BaseRole>,
    /// Semua `accessLevel.id` yang membentuk snapshot (untuk invalidasi per role).
    pub role_ids: Vec<i32>,
}

pub type RoleCache = EpochCache<(i32, i32), Arc<RoleAccessSnapshot>>;

pub fn new_role_cache() -> RoleCache {
    EpochCache::new(100_000)
}

#[derive(sqlx::FromRow)]
struct Row {
    id: i32,
    code: String,
    #[sqlx(rename = "isSystem")]
    is_system: bool,
    #[sqlx(rename = "baseRole")]
    base_role: Option<BaseRole>,
    status: i32,
}

/// Satu query, `companyId` di `WHERE` — role company lain tidak pernah terbaca.
async fn fetch(
    db: &sqlx::PgPool,
    user_id: i32,
    company_id: i32,
) -> sqlx::Result<RoleAccessSnapshot> {
    let rows = sqlx::query_as::<_, Row>(
        r#"SELECT al.id, al.code, al."isSystem", al."baseRole"::text AS "baseRole", al.status
           FROM "userAccessLevel" ual
           JOIN "accessLevel" al ON al.id = ual."accessLevelId"
           WHERE ual."userId" = $1 AND ual."companyId" = $2 AND ual.status = 1"#,
    )
    .bind(user_id)
    .bind(company_id)
    .fetch_all(db)
    .await?;

    let mut snap = RoleAccessSnapshot::default();
    let mut ceiling: Option<u8> = None;
    for level in rows {
        snap.role_ids.push(level.id);
        if level.is_system {
            if !level.code.is_empty() {
                snap.codes.push(level.code);
            }
            continue;
        }
        let Some(base) = level.base_role.filter(|_| level.status == 1) else {
            continue;
        };
        let Some(rank) = rank_of(base.as_str()) else {
            continue;
        };
        if ceiling.is_none_or(|c| rank > c) {
            ceiling = Some(rank);
            snap.custom_base_role = Some(base);
        }
    }
    Ok(snap)
}

pub async fn user_access(
    state: &AppState,
    user_id: i32,
    company_id: i32,
) -> ApiResult<Arc<RoleAccessSnapshot>> {
    let db = state.db.clone();
    state
        .caches
        .roles
        .get_or_load(
            (user_id, company_id),
            |_| Some(Keep::ttl(ROLE_CACHE_TTL)),
            move || async move {
                fetch(&db, user_id, company_id)
                    .await
                    .map(Arc::new)
                    .map_err(shared_err)
            },
        )
        .await
        .map_err(|e| ApiError::internal(anyhow::anyhow!("{e}")))
}

/// Keputusan `RolesGuard.canActivate` sesudah company ter-resolve.
pub fn decide(
    required: &[Role],
    company_id: Option<i32>,
    snap: Option<&RoleAccessSnapshot>,
) -> ApiResult<()> {
    let (Some(_), Some(snap)) = (company_id, snap) else {
        return Err(ApiError::coded(
            StatusCode::FORBIDDEN,
            "NO_ACTIVE_COMPANY",
            "No active company found for user",
        ));
    };

    if !snap.codes.is_empty() {
        let has_role = required
            .iter()
            .any(|r| snap.codes.iter().any(|c| c == r.as_str()));
        if !has_role {
            return Err(ApiError::unauthorized(
                "Access denied: insufficient permissions",
            ));
        }
        return Ok(());
    }

    let Some(base) = snap.custom_base_role else {
        return Err(ApiError::coded(
            StatusCode::FORBIDDEN,
            "NO_ROLE_ASSIGNED",
            "No role assigned to user in the active company",
        ));
    };

    let ceiling = rank_of(base.as_str());
    let within = ceiling.is_some_and(|c| {
        required
            .iter()
            .any(|r| rank_of(r.as_str()).is_some_and(|needed| needed <= c))
    });
    if !within {
        return Err(ApiError::coded(
            StatusCode::FORBIDDEN,
            "ROLE_LEVEL_TOO_LOW",
            "Role level is below the minimum required for this endpoint",
        ));
    }
    // Lolos plafon BUKAN berarti diizinkan — keputusan role custom milik
    // gerbang permission sesudah ini.
    Ok(())
}

/// Kode role efektif seluruh penugasan aktif user (`userAccessLevel` status 1 ->
/// `effectiveRoleCodesOf`). Sengaja lintas company, persis kueri Node `projects.service.ts`
/// (`findAll`/`findOne`; dipakai `mz_projects::model::is_elevated`). BUKAN untuk guard `@UUID`:
/// guard itu hanya memakai role di company aktif ([`role_codes_in_company`], sejak 2026-11-17).
pub async fn role_codes<'e>(db: impl PgExecutor<'e>, user_id: i32) -> sqlx::Result<Vec<String>> {
    let rows: Vec<(String, bool, Option<String>)> = sqlx::query_as(
        r#"SELECT al.code, al."isSystem", al."baseRole"::text
           FROM "userAccessLevel" ual
           JOIN "accessLevel" al ON al.id = ual."accessLevelId"
           WHERE ual."userId" = $1 AND ual.status = 1
           ORDER BY ual.id"#,
    )
    .bind(user_id)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(code, is_system, base)| {
            effective_role_code(
                &code,
                is_system,
                base.and_then(|b| b.parse::<BaseRole>().ok()),
            )
        })
        .collect())
}

/// Seperti [`role_codes`] tetapi HANYA penugasan di `company_id` (company aktif pelaku). Dipakai
/// guard `@UUID` (`mz_core::uuid_guard`); Node: `userAccessLevel.findMany({ where: { userId,
/// companyId, status: 1 } })` di `src/uuid/uuid.decorator.ts:60`. Role di company lain tidak membuat pelaku elevated di sini.
pub async fn role_codes_in_company<'e>(
    db: impl PgExecutor<'e>,
    user_id: i32,
    company_id: i32,
) -> sqlx::Result<Vec<String>> {
    let rows: Vec<(String, bool, Option<String>)> = sqlx::query_as(
        r#"SELECT al.code, al."isSystem", al."baseRole"::text
           FROM "userAccessLevel" ual
           JOIN "accessLevel" al ON al.id = ual."accessLevelId"
           WHERE ual."userId" = $1 AND ual."companyId" = $2 AND ual.status = 1
           ORDER BY ual.id"#,
    )
    .bind(user_id)
    .bind(company_id)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(code, is_system, base)| {
            effective_role_code(
                &code,
                is_system,
                base.and_then(|b| b.parse::<BaseRole>().ok()),
            )
        })
        .collect())
}

/// `builtInBaseRoleOf` — hanya ADMIN/MANAGER/STAFF punya padanan `BaseRole`.
pub fn built_in_base_role_of(code: &str) -> Option<&'static str> {
    match code {
        "ADMIN" => Some("ADMIN"),
        "MANAGER" => Some("MANAGER"),
        "STAFF" => Some("STAFF"),
        _ => None,
    }
}

/// Bagian `baseRole` dari `roleCodeFilter(codes)`: `{ isSystem: false, baseRole: { in: [...] } }`.
/// (Cabang `{ isSystem: true, code: { in: codes } }` memakai `codes` apa adanya.)
pub fn role_code_filter_base_roles(codes: &[&str]) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    for code in codes {
        if let Some(base) = built_in_base_role_of(code)
            && !out.contains(&base)
        {
            out.push(base);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_roles_role_code_filter() {
        assert_eq!(
            role_code_filter_base_roles(&["SYSTEM_ADMIN", "SUPER_ADMIN", "ADMIN", "MANAGER"]),
            vec!["ADMIN", "MANAGER"]
        );
        assert_eq!(
            role_code_filter_base_roles(&["SUPER_ADMIN", "ADMIN"]),
            vec!["ADMIN"]
        );
        assert_eq!(built_in_base_role_of("GUEST"), None);
    }

    fn snap(codes: &[&str], base: Option<BaseRole>) -> RoleAccessSnapshot {
        RoleAccessSnapshot {
            codes: codes.iter().map(|s| (*s).to_owned()).collect(),
            custom_base_role: base,
            role_ids: vec![],
        }
    }

    #[test]
    fn built_in_role_uses_legacy_401() {
        let err = decide(&[Role::ADMIN], Some(1), Some(&snap(&["STAFF"], None)));
        assert!(
            matches!(err, Err(ApiError::Message { status, .. }) if status == StatusCode::UNAUTHORIZED)
        );
        assert!(
            decide(
                &[Role::ADMIN, Role::STAFF],
                Some(1),
                Some(&snap(&["STAFF"], None))
            )
            .is_ok()
        );
    }

    #[test]
    fn custom_role_ceiling() {
        let manager = snap(&[], Some(BaseRole::MANAGER));
        assert!(decide(&[Role::ADMIN, Role::MANAGER], Some(1), Some(&manager)).is_ok());
        assert!(matches!(
            decide(&[Role::SUPER_ADMIN, Role::ADMIN], Some(1), Some(&manager)),
            Err(ApiError::Body { status, .. }) if status == StatusCode::FORBIDDEN
        ));
    }

    #[test]
    fn no_role_is_403() {
        assert!(matches!(
            decide(&[Role::STAFF], Some(1), Some(&snap(&[], None))),
            Err(ApiError::Body { status, .. }) if status == StatusCode::FORBIDDEN
        ));
    }
}
