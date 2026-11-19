//! Port `src/roles/roles.service.ts` (docs/custom_roles.md §11 F9 + F10).
//!
//! Pengaman §9 di berkas ini memakai aturan yang SUDAH ada di Rust:
//! - §9.6/§9.8 anti-lockout: `mz_account::anti_lockout::{begin_guard, finish_guard,
//!   lock_company_row}` (port `anti-lockout.ts`);
//! - §9.15 plafon pemberian jabatan: `mz_account::assignable_role::{resolve_assignable_role,
//!   require_actor_role, can_assign}`;
//! - matriks pelaku: `mz_core::permissions::effective_access` (cache yang sama dengan gerbang);
//! - §9.3/§9.4/§9.10/§9.11: [`crate::escalation`].
//!
//! Setiap transaksi: galat apa pun membuang SELURUH buffer audit request (`discardAudit`), dan
//! pelanggaran unique jadi `409 ROLE_SAVE_CONFLICT`. Sesudah commit: `AppState::invalidate_role`
//! (cache role+permission lokal + pesan protokol `role`), persis `PermissionsService.invalidateRole`.

use std::cmp::Ordering;
use std::sync::Arc;

use axum::http::StatusCode;
use mz_account::anti_lockout::{begin_guard, finish_guard, lock_company_row};
use mz_account::assignable_role::{
    ACCESS_LEVEL_COLUMNS, AccessLevel, AssignableRoleActor, RoleRef, can_assign,
    require_actor_role, resolve_assignable_role, role_actor_of,
};
use mz_audit::{AuditHandle, Lookup, MemberEffect, Model, Row, Touched};
use mz_core::company::require_company_id;
use mz_core::enums::{BaseRole, PermissionAction, effective_role_code, rank_of};
use mz_core::jsfmt::collate_id;
use mz_core::permissions::{EffectiveAccess, effective_access};
use mz_core::time::{JsDate, now_naive};
use mz_core::{ApiError, AppState};
use serde::Serialize;
use serde_json::json;
use sqlx::{PgConnection, PgPool};

use crate::dto::{
    CloneRoleDto, CreateRoleDto, DeleteRoleQueryDto, Field, PreviewRoleDto,
    ReplaceRolePermissionsDto, UpdateRoleDto,
};
use crate::entity::{
    AssignableRoleEntity, RoleDeleted, RoleDetailEntity, RoleEntity, RoleImpactEntity,
    RoleImpactUser, RoleReverted, built_in_slot_of, group_by_feature,
};
use crate::error::{RolesError, RolesResult};
use crate::escalation::{
    assert_base_role_within_actor, assert_grants_within_actor, normalize_and_validate,
};
use crate::preview::{Grant, RolePreview, build_role_preview};
use crate::role_code::generate_role_code;

/// `accessLevel.status` / `userAccessLevel.status`.
const ACTIVE: i32 = 1;
const DELETED: i32 = 0;

/// `accessLevel` + `rolePermissions` (`include: { rolePermissions: true }`).
pub struct RoleWithPermissions {
    pub role: AccessLevel,
    pub permissions: Vec<(String, PermissionAction)>,
}

// ── galat bernama ────────────────────────────────────────────────────────────

fn role_not_found(uuid: &str) -> RolesError {
    RolesError::coded(
        StatusCode::NOT_FOUND,
        "ROLE_NOT_FOUND",
        format!("Role '{uuid}' not found"),
    )
}

/// `rejectClientSentCode(dto)` (§9.13).
fn reject_client_sent_code(has_code: bool) -> RolesResult<()> {
    if has_code {
        return Err(RolesError::coded(
            StatusCode::UNPROCESSABLE_ENTITY,
            "CODE_IS_SERVER_GENERATED",
            "'code' is server-generated and cannot be set by the client",
        ));
    }
    Ok(())
}

fn source_has_no_base_role(name: &str) -> RolesError {
    RolesError::coded(
        StatusCode::UNPROCESSABLE_ENTITY,
        "SOURCE_HAS_NO_BASE_ROLE",
        format!("Role '{name}' has no base role and cannot be cloned into a custom role"),
    )
}

/// Ekor `transaction()` Node: P2002 → `409 ROLE_SAVE_CONFLICT` (target hanya ke log).
fn tx_error(e: RolesError) -> RolesError {
    match e {
        RolesError::UniqueViolation(target) => {
            tracing::warn!("P2002 pada role write, target={target}");
            RolesError::coded(
                StatusCode::CONFLICT,
                "ROLE_SAVE_CONFLICT",
                "The role could not be saved because it conflicts with existing data. Please try again.",
            )
        }
        other => other,
    }
}

// ── pembaca ──────────────────────────────────────────────────────────────────

async fn company_of(state: &AppState, user_id: i32) -> RolesResult<i32> {
    require_company_id(&state.db, user_id).await
}

/// `requireActor`: matriks pelaku (`PermissionsService.getEffectiveAccess`, cache bersama).
async fn require_actor(
    state: &AppState,
    user_id: i32,
    company_id: i32,
) -> RolesResult<Arc<EffectiveAccess>> {
    effective_access(state, user_id, company_id)
        .await?
        .ok_or_else(|| {
            RolesError::coded(
                StatusCode::FORBIDDEN,
                "NO_ROLE_ASSIGNED",
                "No role assigned to user in the active company",
            )
        })
}

async fn permissions_of(
    db: &PgPool,
    access_level_id: i32,
) -> RolesResult<Vec<(String, PermissionAction)>> {
    Ok(sqlx::query_as::<_, (String, PermissionAction)>(
        r#"SELECT "featureKey", action::text FROM "rolePermission"
           WHERE "accessLevelId" = $1 ORDER BY id"#,
    )
    .bind(access_level_id)
    .fetch_all(db)
    .await?)
}

/// `findVisible`: milik company pelaku ATAU bawaan.
async fn find_visible(
    db: &PgPool,
    company_id: i32,
    uuid: &str,
) -> RolesResult<RoleWithPermissions> {
    let sql = format!(
        r#"SELECT {ACCESS_LEVEL_COLUMNS} FROM "accessLevel"
           WHERE uuid = $1::uuid AND status = 1 AND ("companyId" = $2 OR "isSystem" = true)
           LIMIT 1"#
    );
    let role = sqlx::query_as::<_, AccessLevel>(&sql)
        .bind(uuid)
        .bind(company_id)
        .fetch_optional(db)
        .await?
        .ok_or_else(|| role_not_found(uuid))?;
    let permissions = permissions_of(db, role.id).await?;
    Ok(RoleWithPermissions { role, permissions })
}

/// `findOwnCustomRole` (§9.1): role bawaan tidak pernah tercapai lewat query ini.
async fn find_own_custom_role(
    db: &PgPool,
    company_id: i32,
    uuid: &str,
) -> RolesResult<AccessLevel> {
    let sql = format!(
        r#"SELECT {ACCESS_LEVEL_COLUMNS} FROM "accessLevel"
           WHERE uuid = $1::uuid AND "companyId" = $2 AND "isSystem" = false AND status = 1
           LIMIT 1"#
    );
    sqlx::query_as::<_, AccessLevel>(&sql)
        .bind(uuid)
        .bind(company_id)
        .fetch_optional(db)
        .await?
        .ok_or_else(|| role_not_found(uuid))
}

/// `findOwnOverride` — hanya override role bawaan (§F16) milik company pelaku.
async fn find_own_override(db: &PgPool, company_id: i32, uuid: &str) -> RolesResult<AccessLevel> {
    let sql = format!(
        r#"SELECT {ACCESS_LEVEL_COLUMNS} FROM "accessLevel"
           WHERE uuid = $1::uuid AND "companyId" = $2 AND "isSystem" = false
             AND "overridesRoleId" IS NOT NULL AND status = 1
           LIMIT 1"#
    );
    sqlx::query_as::<_, AccessLevel>(&sql)
        .bind(uuid)
        .bind(company_id)
        .fetch_optional(db)
        .await?
        .ok_or_else(|| role_not_found(uuid))
}

async fn find_built_in(db: &PgPool, id: i32) -> RolesResult<RoleWithPermissions> {
    let sql = format!(
        r#"SELECT {ACCESS_LEVEL_COLUMNS} FROM "accessLevel"
           WHERE id = $1 AND "isSystem" = true AND status = 1 LIMIT 1"#
    );
    let role = sqlx::query_as::<_, AccessLevel>(&sql)
        .bind(id)
        .fetch_optional(db)
        .await?
        .ok_or_else(|| {
            RolesError::coded(
                StatusCode::NOT_FOUND,
                "ROLE_NOT_FOUND",
                "Built-in role not found",
            )
        })?;
    let permissions = permissions_of(db, role.id).await?;
    Ok(RoleWithPermissions { role, permissions })
}

/// `dropOverriddenBuiltIns`: begitu company punya override atas role bawaan, baris bawaannya
/// disembunyikan (admin melihat SATU baris per slot).
async fn drop_overridden_built_ins(
    db: &PgPool,
    company_id: i32,
    rows: Vec<AccessLevel>,
) -> RolesResult<Vec<AccessLevel>> {
    let overridden: Vec<Option<i32>> = sqlx::query_scalar(
        r#"SELECT "overridesRoleId" FROM "accessLevel"
           WHERE "companyId" = $1 AND "overridesRoleId" IS NOT NULL AND status = 1"#,
    )
    .bind(company_id)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .filter(|r| !overridden.contains(&Some(r.id)))
        .collect())
}

/// `baseRoleOf(role)`: bawaan → padanan `code` (ADMIN/MANAGER/STAFF), custom → kolomnya.
fn base_role_of(role: &AccessLevel) -> Option<BaseRole> {
    if role.is_system {
        return match role.code.as_str() {
            "ADMIN" => Some(BaseRole::ADMIN),
            "MANAGER" => Some(BaseRole::MANAGER),
            "STAFF" => Some(BaseRole::STAFF),
            _ => None,
        };
    }
    role.base_role
}

fn detail_entity(r: &RoleWithPermissions) -> RoleDetailEntity {
    RoleDetailEntity {
        role: RoleEntity::from(&r.role),
        permissions: group_by_feature(&r.permissions),
    }
}

async fn holders_count(db: &PgPool, role_id: i32, company_id: i32) -> RolesResult<i64> {
    Ok(sqlx::query_scalar(
        r#"SELECT COUNT(*) FROM "userAccessLevel"
           WHERE "accessLevelId" = $1 AND "companyId" = $2 AND status = 1"#,
    )
    .bind(role_id)
    .bind(company_id)
    .fetch_one(db)
    .await?)
}

// ── penulis ber-audit ────────────────────────────────────────────────────────

/// Kolom `accessLevel.create` role custom.
struct NewRole<'a> {
    code: &'a str,
    name: &'a str,
    description: Option<&'a str>,
    company_id: i32,
    base_role: BaseRole,
    overrides_role_id: Option<i32>,
    created_by_id: i32,
}

/// `tx.accessLevel.create({ data })` (tanpa `select` → audit memakai baris lengkap).
async fn insert_role(
    conn: &mut PgConnection,
    audit: &AuditHandle,
    new: NewRole<'_>,
) -> RolesResult<AccessLevel> {
    let sql = format!(
        r#"INSERT INTO "accessLevel"
             (code, name, description, "companyId", "isSystem", "baseRole", "overridesRoleId", "createdById", status)
           VALUES ($1, $2, $3, $4, false, $5::"BaseRole", $6, $7, $8)
           RETURNING {ACCESS_LEVEL_COLUMNS}"#
    );
    let role = sqlx::query_as::<_, AccessLevel>(&sql)
        .bind(new.code)
        .bind(new.name)
        .bind(new.description)
        .bind(new.company_id)
        .bind(new.base_role)
        .bind(new.overrides_role_id)
        .bind(new.created_by_id)
        .bind(ACTIVE)
        .fetch_one(&mut *conn)
        .await?;
    if let Some(row) = audit
        .result_row(&mut *conn, Model::AccessLevel, Lookup::Id(role.id))
        .await
    {
        audit.record_create(Model::AccessLevel, &row).await;
    }
    Ok(role)
}

/// `tx.accessLevel.update({ where: { id }, data: { status: 0, updatedAt } })`.
async fn soft_delete_role(
    conn: &mut PgConnection,
    audit: &AuditHandle,
    role_id: i32,
) -> RolesResult<()> {
    let pre = audit
        .pre_read(Model::AccessLevel, Lookup::Id(role_id))
        .await;
    sqlx::query(r#"UPDATE "accessLevel" SET status = $2, "updatedAt" = $3 WHERE id = $1"#)
        .bind(role_id)
        .bind(DELETED)
        .bind(now_naive())
        .execute(&mut *conn)
        .await?;
    if let Some(row) = audit
        .result_row(&mut *conn, Model::AccessLevel, Lookup::Id(role_id))
        .await
    {
        audit.record_update(Model::AccessLevel, pre, &row).await;
    }
    Ok(())
}

/// `tx.rolePermission.createMany({ data })` — tabel keanggotaan ber-audit (§4.2).
async fn insert_permissions(
    conn: &mut PgConnection,
    audit: &AuditHandle,
    access_level_id: i32,
    rows: &[(String, PermissionAction)],
) -> RolesResult<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let data: Vec<Row> = rows
        .iter()
        .map(|(f, a)| {
            Row::new()
                .with("accessLevelId", access_level_id)
                .with("featureKey", f.as_str())
                .with("action", a.as_str())
        })
        .collect();
    let pre = audit
        .membership_pre(
            Model::RolePermission,
            MemberEffect::Add,
            Touched::Rows(data),
        )
        .await;
    let features: Vec<&str> = rows.iter().map(|(f, _)| f.as_str()).collect();
    let actions: Vec<&str> = rows.iter().map(|(_, a)| a.as_str()).collect();
    sqlx::query(
        r#"INSERT INTO "rolePermission" ("accessLevelId", "featureKey", action)
           SELECT $1, t.f, t.a::"PermissionAction"
           FROM UNNEST($2::text[], $3::text[]) WITH ORDINALITY AS t(f, a, n)
           ORDER BY t.n"#,
    )
    .bind(access_level_id)
    .bind(&features)
    .bind(&actions)
    .execute(&mut *conn)
    .await?;
    audit.record_membership(pre).await;
    Ok(())
}

/// `tx.rolePermission.deleteMany({ where: { accessLevelId } })`.
async fn delete_permissions(
    conn: &mut PgConnection,
    audit: &AuditHandle,
    access_level_id: i32,
) -> RolesResult<()> {
    let pre = audit
        .membership_pre(
            Model::RolePermission,
            MemberEffect::Remove,
            Touched::Where(Lookup::filter(move |qb| {
                qb.push(r#""accessLevelId" = "#).push_bind(access_level_id);
            })),
        )
        .await;
    sqlx::query(r#"DELETE FROM "rolePermission" WHERE "accessLevelId" = $1"#)
        .bind(access_level_id)
        .execute(&mut *conn)
        .await?;
    audit.record_membership(pre).await;
    Ok(())
}

/// `tx.userAccessLevel.updateMany({ where: { accessLevelId: from, companyId, status: 1 },
/// data: { accessLevelId: to, updatedAt } })` → jumlah baris.
async fn reassign_holders(
    conn: &mut PgConnection,
    audit: &AuditHandle,
    from: i32,
    to: i32,
    company_id: i32,
) -> RolesResult<i64> {
    let pre = audit
        .pre_read_many(
            Model::UserAccessLevel,
            Lookup::filter(move |qb| {
                qb.push(r#""accessLevelId" = "#).push_bind(from);
                qb.push(r#" AND "companyId" = "#).push_bind(company_id);
                qb.push(" AND status = ").push_bind(ACTIVE);
            }),
        )
        .await;
    let now = now_naive();
    let affected = sqlx::query(
        r#"UPDATE "userAccessLevel" SET "accessLevelId" = $2, "updatedAt" = $4
           WHERE "accessLevelId" = $1 AND "companyId" = $3 AND status = 1"#,
    )
    .bind(from)
    .bind(to)
    .bind(company_id)
    .bind(now)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    audit
        .record_update_many(
            Model::UserAccessLevel,
            pre,
            affected,
            &Row::new()
                .with("accessLevelId", to)
                .with("updatedAt", json!(JsDate(now))),
            json!({ "accessLevelId": from, "companyId": company_id, "status": ACTIVE }),
        )
        .await;
    Ok(i64::try_from(affected).unwrap_or(i64::MAX))
}

fn grant_rows(grants: &[Grant]) -> Vec<(String, PermissionAction)> {
    grants
        .iter()
        .flat_map(|g| g.actions.iter().map(|a| (g.feature_key.clone(), *a)))
        .collect()
}

/// `groupByFeature(source.rolePermissions)` sebagai permintaan §9.3 (`new Set(actions)`).
fn grants_of(rows: &[(String, PermissionAction)]) -> Vec<Grant> {
    group_by_feature(rows)
        .into_iter()
        .map(|p| {
            let mut actions: Vec<PermissionAction> = Vec::new();
            for a in p.actions {
                if !actions.contains(&a) {
                    actions.push(a);
                }
            }
            Grant {
                feature_key: p.feature_key,
                actions,
            }
        })
        .collect()
}

// ── service ──────────────────────────────────────────────────────────────────

/// `list(userId)` — `GET /v1/admin/roles`.
pub async fn list(state: &AppState, user_id: i32) -> RolesResult<Vec<RoleEntity>> {
    let company_id = company_of(state, user_id).await?;
    let sql = format!(
        r#"SELECT {ACCESS_LEVEL_COLUMNS} FROM "accessLevel"
           WHERE status = 1 AND ("companyId" = $1 OR "isSystem" = true) AND NOT (code = 'SYSTEM_ADMIN')
           ORDER BY "isSystem" DESC, name ASC"#
    );
    let rows = sqlx::query_as::<_, AccessLevel>(&sql)
        .bind(company_id)
        .fetch_all(&state.db)
        .await?;
    let visible = drop_overridden_built_ins(&state.db, company_id, rows).await?;
    Ok(visible.iter().map(RoleEntity::from).collect())
}

/// Perkiraan `String.prototype.localeCompare` (ICU, locale bawaan): `mz_core::jsfmt::collate_id`
/// ditambah pemecah seri panjang string (selisih perilaku lama versi roles; praktis tak pernah
/// tercapai, dipertahankan eksplisit).
fn locale_compare(a: &str, b: &str) -> Ordering {
    collate_id(a, b).then_with(|| a.len().cmp(&b.len()))
}

const ASSIGNABLE_SLOT_ORDER: &[&str] = &["ADMIN", "MANAGER", "STAFF", "GUEST"];

/// `compareAssignable`.
fn compare_assignable(a: &AssignableRoleEntity, b: &AssignableRoleEntity) -> Ordering {
    let pos = |e: &AssignableRoleEntity| {
        e.built_in_slot
            .as_deref()
            .and_then(|s| ASSIGNABLE_SLOT_ORDER.iter().position(|x| *x == s))
            .unwrap_or(ASSIGNABLE_SLOT_ORDER.len())
    };
    pos(a)
        .cmp(&pos(b))
        .then_with(|| locale_compare(&a.name, &b.name))
        .then_with(|| locale_compare(&a.code, &b.code))
}

/// `listAssignable(userId)` — `GET /v1/roles/assignable`.
pub async fn list_assignable(
    state: &AppState,
    user_id: i32,
) -> RolesResult<Vec<AssignableRoleEntity>> {
    let company_id = company_of(state, user_id).await?;
    let actor = {
        let mut conn = state.db.acquire().await?;
        require_actor_role(&mut conn, user_id, company_id).await?
    };
    let actor_rank = rank_of(&actor.effective_code);
    let sql = format!(
        r#"SELECT {ACCESS_LEVEL_COLUMNS} FROM "accessLevel"
           WHERE status = 1 AND ("companyId" = $1 OR "isSystem" = true)
             AND NOT (code IN ('SYSTEM_ADMIN', 'SUPER_ADMIN'))
           ORDER BY id"#
    );
    let rows = sqlx::query_as::<_, AccessLevel>(&sql)
        .bind(company_id)
        .fetch_all(&state.db)
        .await?;
    let visible = drop_overridden_built_ins(&state.db, company_id, rows).await?;
    let mut out: Vec<AssignableRoleEntity> = visible
        .iter()
        .map(|row| AssignableRoleEntity {
            uuid: row.uuid.clone(),
            code: row.code.clone(),
            name: row.name.clone(),
            description: row.description.clone(),
            base_role: row.base_role,
            is_system: row.is_system,
            is_built_in_override: row.overrides_role_id.is_some(),
            built_in_slot: built_in_slot_of(row),
            assignable: actor_rank.is_some_and(|r| {
                can_assign(
                    r,
                    &effective_role_code(&row.code, row.is_system, row.base_role),
                )
            }),
        })
        .collect();
    out.sort_by(compare_assignable);
    Ok(out)
}

/// `detail(userId, uuid)` — `GET /v1/admin/roles/:uuid` (bawaan ikut, read-only).
pub async fn detail(state: &AppState, user_id: i32, uuid: &str) -> RolesResult<RoleDetailEntity> {
    let company_id = company_of(state, user_id).await?;
    let role = find_visible(&state.db, company_id, uuid).await?;
    Ok(detail_entity(&role))
}

/// `create(userId, dto)` — `POST /v1/admin/roles`.
pub async fn create(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    dto: &CreateRoleDto,
) -> RolesResult<RoleDetailEntity> {
    reject_client_sent_code(dto.has_code())?;
    let company_id = company_of(state, user_id).await?;
    let actor = require_actor(state, user_id, company_id).await?;
    let permissions = normalize_and_validate(&dto.permissions())?;
    let base = dto.base_role();
    assert_base_role_within_actor(&actor, base)?;
    assert_grants_within_actor(&actor, &permissions)?;
    let Some(base) = base else {
        return Err(ApiError::internal(anyhow::anyhow!(
            "baseRole lolos validasi tanpa nilai"
        )));
    };

    let name = dto.name();
    let description = dto.description();
    let discard = audit.discard_guard();
    let created: RolesResult<AccessLevel> = async {
        let mut tx = state.db.begin().await?;
        lock_company_row(&mut tx, company_id).await?;
        let code = generate_role_code(&mut tx, company_id, &name).await?;
        let role = insert_role(
            &mut tx,
            audit,
            NewRole {
                code: &code,
                name: &name,
                description: description.as_deref(),
                company_id,
                base_role: base,
                overrides_role_id: None,
                created_by_id: user_id,
            },
        )
        .await?;
        insert_permissions(&mut tx, audit, role.id, &grant_rows(&permissions)).await?;
        tx.commit().await?;
        Ok(role)
    }
    .await;
    let created = created.map_err(tx_error)?;
    discard.disarm();
    detail(state, user_id, &created.uuid).await
}

/// `update(userId, uuid, dto)` — `PATCH /v1/admin/roles/:uuid`.
pub async fn update(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    uuid: &str,
    dto: &UpdateRoleDto,
) -> RolesResult<RoleDetailEntity> {
    reject_client_sent_code(dto.has_code())?;
    let company_id = company_of(state, user_id).await?;
    let role = find_own_custom_role(&state.db, company_id, uuid).await?;
    let base = dto.base_role();

    if role.overrides_role_id.is_some() && base.is_defined() {
        return Err(RolesError::coded(
            StatusCode::UNPROCESSABLE_ENTITY,
            "OVERRIDE_BASE_ROLE_FIXED",
            "The base role of a built-in override is fixed to its source role and cannot be changed. Renaming it or editing its description is still allowed.",
        ));
    }
    if base.is_defined() {
        // §9.4 juga berlaku untuk MENAIKKAN role yang sudah ada.
        let actor = require_actor(state, user_id, company_id).await?;
        let wanted = match &base {
            Field::Set(b) => Some(*b),
            Field::Absent | Field::Null => None,
        };
        assert_base_role_within_actor(&actor, wanted)?;
    }
    // Prisma menolak `name: null` (kolom wajib) dengan PrismaClientValidationError → 500.
    let name = dto.name();
    if name == Field::Null {
        return Err(ApiError::internal(anyhow::anyhow!(
            "Argument `name` must not be null (accessLevel.update)"
        )));
    }

    let pre = audit
        .pre_read(Model::AccessLevel, Lookup::Id(role.id))
        .await;
    let mut qb = sqlx::QueryBuilder::<sqlx::Postgres>::new(r#"UPDATE "accessLevel" SET "#);
    {
        let mut set = qb.separated(", ");
        if let Field::Set(n) = &name {
            set.push("name = ").push_bind_unseparated(n.clone());
        }
        match dto.description() {
            Field::Set(d) => {
                set.push("description = ").push_bind_unseparated(d);
            }
            Field::Null => {
                set.push("description = NULL");
            }
            Field::Absent => {}
        }
        if let Field::Set(b) = &base {
            set.push(r#""baseRole" = "#)
                .push_bind_unseparated(*b)
                .push_unseparated(r#"::"BaseRole""#);
        }
        set.push(r#""updatedAt" = "#)
            .push_bind_unseparated(now_naive());
    }
    qb.push(" WHERE id = ").push_bind(role.id);
    qb.build().execute(&state.db).await?;
    if let Some(row) = audit
        .result_row(&state.db, Model::AccessLevel, Lookup::Id(role.id))
        .await
    {
        audit.record_update(Model::AccessLevel, pre, &row).await;
    }

    state.invalidate_role(role.id);
    detail(state, user_id, uuid).await
}

/// `replacePermissions(userId, uuid, dto)` — `PUT /v1/admin/roles/:uuid/permissions` (REPLACE).
pub async fn replace_permissions(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    uuid: &str,
    dto: &ReplaceRolePermissionsDto,
) -> RolesResult<RoleDetailEntity> {
    let company_id = company_of(state, user_id).await?;
    let actor = require_actor(state, user_id, company_id).await?;
    let role = find_own_custom_role(&state.db, company_id, uuid).await?;
    let permissions = normalize_and_validate(&dto.permissions())?;
    assert_grants_within_actor(&actor, &permissions)?;

    let discard = audit.discard_guard();
    let done: RolesResult<()> = async {
        let mut tx = state.db.begin().await?;
        // §9.6: mencabut `roles:WRITE` dari role admin terakhir = menghapus admin terakhir.
        let before = begin_guard(&mut tx, company_id).await?;
        delete_permissions(&mut tx, audit, role.id).await?;
        insert_permissions(&mut tx, audit, role.id, &grant_rows(&permissions)).await?;
        finish_guard(&mut tx, company_id, &before).await?;
        tx.commit().await?;
        Ok(())
    }
    .await;
    done.map_err(tx_error)?;
    discard.disarm();

    state.invalidate_role(role.id);
    detail(state, user_id, uuid).await
}

/// `clone(userId, uuid, dto)` — `POST /v1/admin/roles/:uuid/clone`.
pub async fn clone(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    uuid: &str,
    dto: &CloneRoleDto,
) -> RolesResult<RoleDetailEntity> {
    reject_client_sent_code(dto.has_code())?;
    let company_id = company_of(state, user_id).await?;
    let actor = require_actor(state, user_id, company_id).await?;
    let source = find_visible(&state.db, company_id, uuid).await?;
    let Some(base) = base_role_of(&source.role) else {
        return Err(source_has_no_base_role(&source.role.name));
    };
    // Clone = `create` dengan bahan baris lain: pemeriksaan §9.3/§9.4 yang sama.
    assert_base_role_within_actor(&actor, Some(base))?;
    assert_grants_within_actor(&actor, &grants_of(&source.permissions))?;

    let name = dto.name();
    let discard = audit.discard_guard();
    let cloned: RolesResult<AccessLevel> = async {
        let mut tx = state.db.begin().await?;
        lock_company_row(&mut tx, company_id).await?;
        let code = generate_role_code(&mut tx, company_id, &name).await?;
        let role = insert_role(
            &mut tx,
            audit,
            NewRole {
                code: &code,
                name: &name,
                description: source.role.description.as_deref(),
                company_id,
                base_role: base,
                overrides_role_id: None,
                created_by_id: user_id,
            },
        )
        .await?;
        insert_permissions(&mut tx, audit, role.id, &source.permissions).await?;
        tx.commit().await?;
        Ok(role)
    }
    .await;
    let cloned = cloned.map_err(tx_error)?;
    discard.disarm();
    detail(state, user_id, &cloned.uuid).await
}

/// `customizeBuiltIn(userId, uuid, dto)` — `PUT /v1/admin/roles/:uuid/customize` (§F16).
pub async fn customize_built_in(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    uuid: &str,
    dto: &ReplaceRolePermissionsDto,
) -> RolesResult<RoleDetailEntity> {
    let company_id = company_of(state, user_id).await?;
    let actor = require_actor(state, user_id, company_id).await?;
    let target = find_visible(&state.db, company_id, uuid).await?;
    // Penyimpanan kedua datang dengan uuid override: diselesaikan ke pasangan yang sama.
    let target_is_override = !target.role.is_system && target.role.overrides_role_id.is_some();
    let source = match target.role.overrides_role_id {
        Some(id) if !target.role.is_system => find_built_in(&state.db, id).await?.role,
        _ => target.role.clone(),
    };
    let base = base_role_of(&source);
    let Some(base) = base.filter(|_| source.is_system) else {
        return Err(source_has_no_base_role(&source.name));
    };

    let permissions = normalize_and_validate(&dto.permissions())?;
    assert_grants_within_actor(&actor, &permissions)?;

    let existing_override = if target_is_override {
        Some(target.role.clone())
    } else {
        let sql = format!(
            r#"SELECT {ACCESS_LEVEL_COLUMNS} FROM "accessLevel"
               WHERE "companyId" = $1 AND "overridesRoleId" = $2 AND status = 1 LIMIT 1"#
        );
        sqlx::query_as::<_, AccessLevel>(&sql)
            .bind(company_id)
            .bind(source.id)
            .fetch_optional(&state.db)
            .await?
    };

    let discard = audit.discard_guard();
    let saved: RolesResult<AccessLevel> = async {
        let mut tx = state.db.begin().await?;
        let before = begin_guard(&mut tx, company_id).await?;
        let row = match existing_override {
            Some(row) => row,
            None => {
                lock_company_row(&mut tx, company_id).await?;
                let code = generate_role_code(&mut tx, company_id, &source.name).await?;
                let row = insert_role(
                    &mut tx,
                    audit,
                    NewRole {
                        code: &code,
                        name: &source.name,
                        description: source.description.as_deref(),
                        company_id,
                        base_role: base,
                        overrides_role_id: Some(source.id),
                        created_by_id: user_id,
                    },
                )
                .await?;
                // Pemegang role bawaan MILIK COMPANY INI SAJA pindah ke override.
                reassign_holders(&mut tx, audit, source.id, row.id, company_id).await?;
                row
            }
        };
        delete_permissions(&mut tx, audit, row.id).await?;
        insert_permissions(&mut tx, audit, row.id, &grant_rows(&permissions)).await?;
        finish_guard(&mut tx, company_id, &before).await?;
        tx.commit().await?;
        Ok(row)
    }
    .await;
    let override_row = saved.map_err(tx_error)?;
    discard.disarm();

    state.invalidate_role(source.id);
    state.invalidate_role(override_row.id);
    detail(state, user_id, &override_row.uuid).await
}

/// `revertBuiltInOverride(userId, uuid)` — `POST /v1/admin/roles/:uuid/revert`.
pub async fn revert_built_in_override(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    uuid: &str,
) -> RolesResult<RoleReverted> {
    let company_id = company_of(state, user_id).await?;
    let override_row = find_own_override(&state.db, company_id, uuid).await?;
    let source_id = override_row.overrides_role_id.unwrap_or_default();

    let discard = audit.discard_guard();
    let done: RolesResult<i64> = async {
        let mut tx = state.db.begin().await?;
        let before = begin_guard(&mut tx, company_id).await?;
        let count =
            reassign_holders(&mut tx, audit, override_row.id, source_id, company_id).await?;
        soft_delete_role(&mut tx, audit, override_row.id).await?;
        finish_guard(&mut tx, company_id, &before).await?;
        tx.commit().await?;
        Ok(count)
    }
    .await;
    let reassigned = done.map_err(tx_error)?;
    discard.disarm();

    state.invalidate_role(source_id);
    state.invalidate_role(override_row.id);
    Ok(RoleReverted {
        reverted: true,
        reassigned,
    })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RoleInUse {
    code: &'static str,
    affected_users: i64,
    message: String,
}

/// `remove(userId, uuid, query)` — `DELETE /v1/admin/roles/:uuid` (§9.5 + §9.6).
pub async fn remove(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    uuid: &str,
    query: &DeleteRoleQueryDto,
) -> RolesResult<RoleDeleted> {
    let company_id = company_of(state, user_id).await?;
    let actor = require_actor(state, user_id, company_id).await?;
    let role = find_own_custom_role(&state.db, company_id, uuid).await?;

    if role.overrides_role_id.is_some() {
        return Err(RolesError::coded(
            StatusCode::UNPROCESSABLE_ENTITY,
            "OVERRIDE_USE_REVERT",
            "A built-in override cannot be deleted. Use the revert endpoint to restore the built-in role instead.",
        ));
    }

    let holders = holders_count(&state.db, role.id, company_id).await?;
    let mut replacement: Option<AccessLevel> = None;
    if holders > 0 {
        let Some(reassign_to) = query.reassign_to() else {
            return Err(RolesError::serialized(
                StatusCode::UNPROCESSABLE_ENTITY,
                &RoleInUse {
                    code: "ROLE_IN_USE",
                    affected_users: holders,
                    message: format!(
                        "Role '{}' is still assigned to {holders} user(s). Pass 'reassignTo' with the uuid of the role they should get instead.",
                        role.name
                    ),
                },
            ));
        };
        let target = {
            let mut conn = state.db.acquire().await?;
            resolve_assignable_role(
                &mut conn,
                &AssignableRoleActor::Actor(role_actor_of(&actor.effective_role_code)),
                company_id,
                RoleRef {
                    code: None,
                    uuid: Some(&reassign_to),
                },
            )
            .await?
        };
        if target.id == role.id {
            return Err(RolesError::coded(
                StatusCode::UNPROCESSABLE_ENTITY,
                "REASSIGN_TO_SAME_ROLE",
                "The replacement role cannot be the role being deleted",
            ));
        }
        replacement = Some(target);
    }

    let reassign_target = replacement.as_ref().map(|r| r.id);
    let discard = audit.discard_guard();
    let done: RolesResult<()> = async {
        let mut tx = state.db.begin().await?;
        let before = begin_guard(&mut tx, company_id).await?;
        if let Some(to) = reassign_target {
            reassign_holders(&mut tx, audit, role.id, to, company_id).await?;
        }
        soft_delete_role(&mut tx, audit, role.id).await?;
        finish_guard(&mut tx, company_id, &before).await?;
        tx.commit().await?;
        Ok(())
    }
    .await;
    done.map_err(tx_error)?;
    discard.disarm();

    state.invalidate_role(role.id);
    Ok(RoleDeleted {
        deleted: true,
        reassigned: if reassign_target.is_some() {
            holders
        } else {
            0
        },
    })
}

/// `impact(userId, uuid)` — `GET /v1/admin/roles/:uuid/impact` (dalam company pelaku saja).
pub async fn impact(state: &AppState, user_id: i32, uuid: &str) -> RolesResult<RoleImpactEntity> {
    let company_id = company_of(state, user_id).await?;
    let role = find_visible(&state.db, company_id, uuid).await?;
    let count = holders_count(&state.db, role.role.id, company_id).await?;
    let users = sqlx::query_as::<_, (String, String, String)>(
        r#"SELECT u.uuid::text, u.name, u.username
           FROM "userAccessLevel" ual JOIN users u ON u.id = ual."userId"
           WHERE ual."accessLevelId" = $1 AND ual."companyId" = $2 AND ual.status = 1
           ORDER BY ual.id
           LIMIT 200"#,
    )
    .bind(role.role.id)
    .bind(company_id)
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .map(|(uuid, name, username)| RoleImpactUser {
        uuid,
        name,
        username,
    })
    .collect();
    Ok(RoleImpactEntity { count, users })
}

/// `preview(userId, dto)` — `POST /v1/admin/roles/preview` (§15.7), belum disimpan.
pub async fn preview(
    state: &AppState,
    user_id: i32,
    dto: &PreviewRoleDto,
) -> RolesResult<RolePreview> {
    let company_id = company_of(state, user_id).await?;
    let permissions = normalize_and_validate(&dto.permissions())?;
    let mut affected_users = 0;
    if let Some(role_uuid) = dto.role_uuid() {
        let role_id: Option<i32> = sqlx::query_scalar(
            r#"SELECT id FROM "accessLevel" WHERE uuid = $1::uuid AND "companyId" = $2 AND status = 1 LIMIT 1"#,
        )
        .bind(&role_uuid)
        .bind(company_id)
        .fetch_optional(&state.db)
        .await?;
        if let Some(id) = role_id {
            affected_users = holders_count(&state.db, id, company_id).await?;
        }
    }
    let Some(base) = dto.base_role() else {
        return Err(ApiError::internal(anyhow::anyhow!(
            "baseRole lolos validasi tanpa nilai"
        )));
    };
    Ok(build_role_preview(base, &permissions, affected_users))
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn entity(slot: Option<&str>, name: &str, code: &str) -> AssignableRoleEntity {
        AssignableRoleEntity {
            uuid: String::new(),
            code: code.into(),
            name: name.into(),
            description: None,
            base_role: None,
            is_system: false,
            is_built_in_override: false,
            built_in_slot: slot.map(str::to_owned),
            assignable: true,
        }
    }

    #[test]
    fn assignable_sort_slots_then_name() {
        let mut v = [
            entity(None, "zeta", "c1_z"),
            entity(Some("STAFF"), "Staff", "STAFF"),
            entity(None, "Alpha", "c1_a"),
            entity(Some("ADMIN"), "Admin", "ADMIN"),
            entity(Some("GUEST"), "Guest", "GUEST"),
            entity(Some("MANAGER"), "Manager", "MANAGER"),
        ];
        v.sort_by(compare_assignable);
        let codes: Vec<&str> = v.iter().map(|e| e.code.as_str()).collect();
        assert_eq!(
            codes,
            ["ADMIN", "MANAGER", "STAFF", "GUEST", "c1_a", "c1_z"]
        );
    }

    #[test]
    fn base_role_of_built_ins() {
        let mut r = AccessLevel {
            id: 1,
            uuid: String::new(),
            code: "MANAGER".into(),
            name: String::new(),
            created_at: JsDate(now_naive()),
            updated_at: None,
            status: 1,
            company_id: None,
            is_system: true,
            base_role: None,
            description: None,
            created_by_id: None,
            overrides_role_id: None,
        };
        assert_eq!(base_role_of(&r), Some(BaseRole::MANAGER));
        r.code = "SUPER_ADMIN".into();
        assert_eq!(base_role_of(&r), None);
        r.is_system = false;
        r.base_role = Some(BaseRole::STAFF);
        assert_eq!(base_role_of(&r), Some(BaseRole::STAFF));
    }

    #[test]
    fn conflict_body() {
        let e = tx_error(RolesError::UniqueViolation("x".into()));
        assert_eq!(e.status(), StatusCode::CONFLICT);
        let RolesError::Body { body, .. } = e else {
            return assert_eq!(1, 0);
        };
        assert_eq!(
            body.to_string(),
            r#"{"code":"ROLE_SAVE_CONFLICT","message":"The role could not be saved because it conflicts with existing data. Please try again."}"#
        );
    }
}
