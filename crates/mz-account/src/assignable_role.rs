//! Port `src/permissions/assignable-role.ts` — SATU aturan "role apa yang boleh dipasangkan
//! ke seorang user" (Aturan B) dan "boleh mengubah orang itu atau tidak" (Aturan C).
//!
//! Semua fungsi DB menerima `&mut PgConnection` supaya bisa dipakai di dalam transaksi
//! pemanggil (`&mut *tx`) maupun lewat `state.db.acquire().await?`.
//! Peringkat dari `mz_core::enums::rank_of`; `None` selalu berarti TOLAK.

use mz_core::enums::{BaseRole, effective_role_code, rank_of};
use mz_core::time::JsDate;
use mz_core::{ApiError, ApiResult};
use serde::Serialize;
use sqlx::PgConnection;

/// `accessLevel.status` / `userAccessLevel.status`: 1 = aktif.
const ACTIVE: i32 = 1;
/// `RANK[Role.ADMIN]`.
const ADMIN_RANK: u8 = 3;

/// Kode error `403` yang dibaca dashboard (specs/006 R7).
pub const TARGET_ABOVE_ACTOR: &str = "TARGET_ABOVE_ACTOR";

/// Pelaku yang tingkatnya dibandingkan (`RoleActor`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleActor {
    pub effective_code: String,
}

/// `roleActorOf(access)` — dari `EffectiveAccess.effective_role_code`.
pub fn role_actor_of(effective_role_code: &str) -> RoleActor {
    RoleActor {
        effective_code: effective_role_code.to_owned(),
    }
}

/// `AssignableRoleActor`: pendaftaran mandiri (`SELF_REGISTRATION`, tidak ada pelaku,
/// Aturan B tidak berlaku) atau pelaku biasa.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssignableRoleActor {
    SelfRegistration,
    Actor(RoleActor),
}

/// Baris `accessLevel` lengkap (semua kolom skalar, bentuk JSON = Prisma).
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct AccessLevel {
    pub id: i32,
    pub uuid: String,
    pub code: String,
    pub name: String,
    #[sqlx(rename = "createdAt")]
    pub created_at: JsDate,
    #[sqlx(rename = "updatedAt")]
    pub updated_at: Option<JsDate>,
    pub status: i32,
    #[sqlx(rename = "companyId")]
    pub company_id: Option<i32>,
    #[sqlx(rename = "isSystem")]
    pub is_system: bool,
    #[sqlx(rename = "baseRole")]
    pub base_role: Option<BaseRole>,
    pub description: Option<String>,
    #[sqlx(rename = "createdById")]
    pub created_by_id: Option<i32>,
    #[sqlx(rename = "overridesRoleId")]
    pub overrides_role_id: Option<i32>,
}

impl AccessLevel {
    /// `effectiveRoleCodeOf`.
    pub fn effective_code(&self) -> String {
        effective_role_code(&self.code, self.is_system, self.base_role)
    }
}

/// Daftar kolom `accessLevel` yang dibaca [`AccessLevel`] (tanpa alias tabel).
pub const ACCESS_LEVEL_COLUMNS: &str = r#"id, uuid::text AS "uuid", code, name, "createdAt", "updatedAt", status,
    "companyId", "isSystem", "baseRole"::text AS "baseRole", description, "createdById", "overridesRoleId""#;

/// Baris penugasan (`userAccessLevel` JOIN `accessLevel`) untuk penilaian peringkat.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AssignmentRow {
    #[sqlx(rename = "userId")]
    pub user_id: i32,
    pub code: Option<String>,
    #[sqlx(rename = "isSystem")]
    pub is_system: Option<bool>,
    #[sqlx(rename = "baseRole")]
    pub base_role: Option<BaseRole>,
    /// `accessLevel.status`; `None` = relasi hilang.
    pub status: Option<i32>,
}

impl AssignmentRow {
    /// Kode efektif bila role-nya ada DAN aktif; `None` selain itu.
    fn active_effective_code(&self) -> Option<String> {
        let code = self.code.as_deref()?;
        if self.status != Some(ACTIVE) {
            return None;
        }
        Some(effective_role_code(
            code,
            self.is_system.unwrap_or(false),
            self.base_role,
        ))
    }
}

const ASSIGNMENT_SQL: &str = r#"SELECT ual."userId", al.code, al."isSystem", al."baseRole"::text AS "baseRole", al.status
    FROM "userAccessLevel" ual LEFT JOIN "accessLevel" al ON al.id = ual."accessLevelId"
    WHERE ual."companyId" = $2 AND ual.status = 1 AND ual."userId" = ANY($1)"#;

async fn assignment_rows(
    conn: &mut PgConnection,
    user_ids: &[i32],
    company_id: i32,
) -> ApiResult<Vec<AssignmentRow>> {
    Ok(sqlx::query_as::<_, AssignmentRow>(ASSIGNMENT_SQL)
        .bind(user_ids)
        .bind(company_id)
        .fetch_all(&mut *conn)
        .await?)
}

/// Jabatan tertinggi di antara baris aktif (peringkat tak dikenal dilewati).
fn best_of<'a>(rows: impl IntoIterator<Item = &'a AssignmentRow>) -> Option<(String, u8)> {
    let mut best: Option<(String, u8)> = None;
    for row in rows {
        let Some(code) = row.active_effective_code() else {
            continue;
        };
        let Some(rank) = rank_of(&code) else { continue };
        if best.as_ref().is_none_or(|(_, r)| rank > *r) {
            best = Some((code, rank));
        }
    }
    best
}

/// `requireActorRole(prisma, actorId, companyId)`: jabatan efektif pelaku di company-nya
/// (yang TERTINGGI bila ada beberapa baris aktif). 403
/// `'User data not found or access level missing'` bila tidak ada.
pub async fn require_actor_role(
    conn: &mut PgConnection,
    actor_id: i32,
    company_id: i32,
) -> ApiResult<RoleActor> {
    let rows = assignment_rows(conn, &[actor_id], company_id).await?;
    match best_of(&rows) {
        Some((code, _)) => Ok(RoleActor {
            effective_code: code,
        }),
        None => Err(ApiError::forbidden(
            "User data not found or access level missing",
        )),
    }
}

/// `canAssign(actorRank, row)`.
pub fn can_assign(actor_rank: u8, effective_code: &str) -> bool {
    rank_of(effective_code).is_some_and(|r| r <= actor_rank)
}

/// Rujukan role: `code` atau `uuid` (`ref: { code?, uuid? }`).
#[derive(Debug, Clone, Copy, Default)]
pub struct RoleRef<'a> {
    pub code: Option<&'a str>,
    pub uuid: Option<&'a str>,
}

/// `redirectToCompanyOverride`: role bawaan yang sudah di-customize company diganti baris override-nya.
pub async fn redirect_to_company_override(
    conn: &mut PgConnection,
    row: AccessLevel,
    company_id: i32,
) -> ApiResult<AccessLevel> {
    if !row.is_system {
        return Ok(row);
    }
    let sql = format!(
        r#"SELECT {ACCESS_LEVEL_COLUMNS} FROM "accessLevel"
           WHERE "companyId" = $1 AND "overridesRoleId" = $2 AND status = 1 ORDER BY id LIMIT 1"#
    );
    let over = sqlx::query_as::<_, AccessLevel>(&sql)
        .bind(company_id)
        .bind(row.id)
        .fetch_optional(&mut *conn)
        .await?;
    Ok(over.unwrap_or(row))
}

/// `resolveAssignableRole(tx, actor, companyId, ref)` (Aturan B). Galat 422:
/// `'Access level is required'`, `"Access Level '<x>' not found"`,
/// `"Access Level '<x>' is above your own access level"`.
pub async fn resolve_assignable_role(
    conn: &mut PgConnection,
    actor: &AssignableRoleActor,
    company_id: i32,
    role_ref: RoleRef<'_>,
) -> ApiResult<AccessLevel> {
    let code = role_ref.code.filter(|c| !c.is_empty());
    let uuid = role_ref.uuid.filter(|u| !u.is_empty());
    if code.is_none() && uuid.is_none() {
        return Err(ApiError::unprocessable("Access level is required"));
    }
    // `ref.code ?? ref.uuid`
    let label = role_ref.code.or(role_ref.uuid).unwrap_or_default();

    let found = match uuid {
        Some(u) => {
            let sql = format!(
                r#"SELECT {ACCESS_LEVEL_COLUMNS} FROM "accessLevel"
                   WHERE status = 1 AND ("isSystem" = true OR "companyId" = $1) AND uuid = $2::uuid
                   ORDER BY id LIMIT 1"#
            );
            sqlx::query_as::<_, AccessLevel>(&sql)
                .bind(company_id)
                .bind(u)
                .fetch_optional(&mut *conn)
                .await?
        }
        None => {
            let sql = format!(
                r#"SELECT {ACCESS_LEVEL_COLUMNS} FROM "accessLevel"
                   WHERE status = 1 AND ("isSystem" = true OR "companyId" = $1) AND code = $2
                   ORDER BY id LIMIT 1"#
            );
            sqlx::query_as::<_, AccessLevel>(&sql)
                .bind(company_id)
                .bind(code.unwrap_or_default())
                .fetch_optional(&mut *conn)
                .await?
        }
    };
    let Some(found) = found else {
        return Err(ApiError::unprocessable(format!(
            "Access Level '{label}' not found"
        )));
    };

    let target = redirect_to_company_override(conn, found, company_id).await?;

    let AssignableRoleActor::Actor(actor) = actor else {
        return Ok(target);
    };
    let allowed = rank_of(&actor.effective_code)
        .is_some_and(|rank| can_assign(rank, &target.effective_code()));
    if !allowed {
        return Err(ApiError::unprocessable(format!(
            "Access Level '{label}' is above your own access level"
        )));
    }
    Ok(target)
}

fn target_above_actor() -> ApiError {
    ApiError::coded(
        axum::http::StatusCode::FORBIDDEN,
        TARGET_ABOVE_ACTOR,
        "Cannot modify a user whose role is above your own.",
    )
}

/// `assertRowsWithinRank`: penilaian peringkat satu target dari baris aktifnya.
/// Peringkat tak dikenal (tanpa baris aktif / role nonaktif / baseRole hilang) = TOLAK untuk
/// pelaku di bawah ADMIN (fail closed).
pub fn assert_rows_within_rank(actor_rank: u8, rows: &[&AssignmentRow]) -> ApiResult<()> {
    let mut highest: Option<u8> = None;
    let mut has_unknown = rows.is_empty();
    for row in rows {
        let Some(rank) = row.active_effective_code().and_then(|c| rank_of(&c)) else {
            has_unknown = true;
            continue;
        };
        if highest.is_none_or(|h| rank > h) {
            highest = Some(rank);
        }
    }
    if highest.is_some_and(|h| h > actor_rank) {
        return Err(target_above_actor());
    }
    if has_unknown && actor_rank < ADMIN_RANK {
        return Err(target_above_actor());
    }
    Ok(())
}

/// `assertTargetWithinRank` (Aturan C): pelaku tidak boleh mengubah orang yang jabatannya
/// di atasnya. Setingkat boleh. Jalankan SEBELUM transaksi tulis.
pub async fn assert_target_within_rank(
    conn: &mut PgConnection,
    actor: &RoleActor,
    target_user_id: i32,
    company_id: i32,
) -> ApiResult<()> {
    let actor_rank = rank_of(&actor.effective_code).ok_or_else(target_above_actor)?;
    let rows = assignment_rows(conn, &[target_user_id], company_id).await?;
    let refs: Vec<&AssignmentRow> = rows.iter().collect();
    assert_rows_within_rank(actor_rank, &refs)
}

/// `assertTargetsWithinRank`: versi massal, satu query. Daftar kosong tidak menyentuh DB.
pub async fn assert_targets_within_rank(
    conn: &mut PgConnection,
    actor: &RoleActor,
    target_user_ids: &[i32],
    company_id: i32,
) -> ApiResult<()> {
    let mut ids: Vec<i32> = target_user_ids.to_vec();
    ids.sort_unstable();
    ids.dedup();
    if ids.is_empty() {
        return Ok(());
    }
    let actor_rank = rank_of(&actor.effective_code).ok_or_else(target_above_actor)?;
    let rows = assignment_rows(conn, &ids, company_id).await?;
    for id in &ids {
        let mine: Vec<&AssignmentRow> = rows.iter().filter(|r| r.user_id == *id).collect();
        assert_rows_within_rank(actor_rank, &mine)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(
        code: &str,
        is_system: bool,
        base: Option<BaseRole>,
        status: Option<i32>,
    ) -> AssignmentRow {
        AssignmentRow {
            user_id: 1,
            code: Some(code.into()),
            is_system: Some(is_system),
            base_role: base,
            status,
        }
    }

    fn rank_check(actor: u8, rows: &[AssignmentRow]) -> bool {
        let refs: Vec<&AssignmentRow> = rows.iter().collect();
        assert_rows_within_rank(actor, &refs).is_ok()
    }

    #[test]
    fn can_assign_is_inclusive_and_fails_closed() {
        assert!(can_assign(3, "ADMIN"));
        assert!(can_assign(3, "STAFF"));
        assert!(!can_assign(2, "ADMIN"));
        assert!(!can_assign(5, "c12_custom"));
        assert!(!can_assign(5, ""));
    }

    #[test]
    fn custom_role_ranks_by_base_role() {
        let custom = row("c12_spv", false, Some(BaseRole::MANAGER), Some(1));
        assert_eq!(custom.active_effective_code().as_deref(), Some("MANAGER"));
        assert!(rank_check(2, &[custom]));
    }

    #[test]
    fn target_above_actor_rejected() {
        assert!(!rank_check(2, &[row("ADMIN", true, None, Some(1))]));
        assert!(rank_check(3, &[row("ADMIN", true, None, Some(1))]));
        // yang tertinggi dari beberapa baris yang menentukan
        assert!(!rank_check(
            2,
            &[
                row("STAFF", true, None, Some(1)),
                row("SUPER_ADMIN", true, None, Some(1))
            ]
        ));
    }

    #[test]
    fn unknown_rank_fails_closed_below_admin() {
        // tanpa baris aktif
        assert!(!rank_check(2, &[]));
        assert!(rank_check(3, &[]));
        // role custom nonaktif
        assert!(!rank_check(
            2,
            &[row("c1_x", false, Some(BaseRole::STAFF), Some(0))]
        ));
        // custom tanpa baseRole -> code mentah, tak dikenal
        assert!(!rank_check(2, &[row("c1_x", false, None, Some(1))]));
        assert!(rank_check(4, &[row("c1_x", false, None, Some(1))]));
    }

    #[test]
    fn best_of_picks_highest_and_skips_inactive() {
        let rows = [
            row("STAFF", true, None, Some(1)),
            row("MANAGER", true, None, Some(1)),
            row("SYSTEM_ADMIN", true, None, Some(0)),
        ];
        assert_eq!(best_of(&rows), Some(("MANAGER".to_owned(), 2)));
        assert_eq!(best_of(&[] as &[AssignmentRow]), None);
    }

    #[test]
    fn target_above_actor_error_shape() {
        let ApiError::Body { status, body } = target_above_actor() else {
            return assert_eq!(0, 1);
        };
        assert_eq!(status, axum::http::StatusCode::FORBIDDEN);
        assert_eq!(
            body,
            serde_json::json!({"code":"TARGET_ABOVE_ACTOR","message":"Cannot modify a user whose role is above your own."})
        );
    }
}
