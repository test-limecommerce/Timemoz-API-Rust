//! Port `store-visit-form-revisers.controller.ts` + `store-visit-form-revisers.service.ts`
//! (docs/store_visit_form_revisers.md §5.1–5.2): CRUD daftar reviser + `mode` per company.
//! Endpoint ini HANYA mengatur siapa yang terdaftar; keputusan boleh/tidak tetap milik
//! [`crate::revise_policy::can_revise`].
//!
//! `@SkipThrottle()` kelas: tidak ada limiter `main.ts` untuk route ini.

use std::collections::{HashMap, HashSet};

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, put};
use axum::{Json, Router};
use mz_audit::{AuditHandle, Lookup, MemberEffect, Model, Row, Touched};
use mz_core::company::require_company_id;
use mz_core::enums::FeatureModule;
use mz_core::enums::{PermissionAction, Role, sees_whole_company};
use mz_core::extract::{Validate, ValidatedJson, ValidatedQuery, validator_js_length};
use mz_core::gate::guard;
use mz_core::plan::PlanFeature;
use mz_core::time::now_naive;
use mz_core::uuid_rules::is_uuid_v4;
use mz_core::{ApiResult, AppState, RequestCtx, RouteSpec};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgPool;

use crate::errors::{coded, coded_details};
use crate::revise_mode::{ReviseMode, STORE_VISIT_FORM_REVISE_MODES};
use crate::revise_policy::{RoleRow, effective_role_code_of, pick_role};

/// `@Roles(SYSTEM_ADMIN, SUPER_ADMIN, ADMIN)` — SENGAJA tanpa MANAGER (§0.6).
const ROLES: &[Role] = &[Role::SYSTEM_ADMIN, Role::SUPER_ADMIN, Role::ADMIN];

const BASE: RouteSpec = RouteSpec::jwt()
    .feature("store-visit-forms")
    .modules(&[FeatureModule::SALES_TRACKER])
    .plan_features(&[PlanFeature::Form])
    .roles(ROLES);
/// `@RequirePermission(PermissionAction.READ)`.
const READ: RouteSpec = BASE.action(PermissionAction::READ);
/// `@RequirePermission(PermissionAction.WRITE)`.
const WRITE: RouteSpec = BASE.action(PermissionAction::WRITE);

/// `COMPANY_WIDE_ROLE_CODES` (`effective-role.ts`).
const COMPANY_WIDE_ROLE_CODES: &[&str] = &["SYSTEM_ADMIN", "SUPER_ADMIN", "ADMIN", "MANAGER"];

/// Picker dashboard dicari, bukan di-scroll, di atas angka ini (§5.1).
const CANDIDATE_LIMIT: usize = 500;

pub fn router(state: &AppState) -> Router<AppState> {
    const PATH: &str = "/v1/admin/store-visit-forms/revisers";
    Router::new()
        .route(PATH, guard(state, READ, get(h_find)))
        .route(PATH, guard(state, WRITE, put(h_replace)))
}

// ── DTO ──────────────────────────────────────────────────────────────────────

/// `ListStoreVisitFormRevisersQueryDto`.
#[derive(Debug, Deserialize)]
pub struct ListRevisersQuery {
    #[serde(default)]
    pub q: Option<Value>,
}

impl Validate for ListRevisersQuery {
    fn validate(&self) -> Vec<String> {
        let mut e = Vec::new();
        // `@IsOptional() @IsString() @MaxLength(255)` (dievaluasi bawah → atas).
        if let Some(v) = self.q.as_ref().filter(|v| !v.is_null()) {
            if !v.as_str().is_some_and(|s| validator_js_length(s) <= 255) {
                e.push("q must be shorter than or equal to 255 characters".to_owned());
            }
            if !v.is_string() {
                e.push("q must be a string".to_owned());
            }
        }
        e
    }
}

/// `PutStoreVisitFormRevisersDto`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PutRevisersDto {
    #[serde(default)]
    pub mode: Option<Value>,
    #[serde(default)]
    pub user_ids: Option<Value>,
}

impl Validate for PutRevisersDto {
    fn validate(&self) -> Vec<String> {
        let mut e = Vec::new();
        // `@IsIn(STORE_VISIT_FORM_REVISE_MODES)`.
        if !crate::revise_mode::is_store_visit_form_revise_mode(self.mode.as_ref()) {
            e.push(format!(
                "mode must be one of the following values: {}",
                STORE_VISIT_FORM_REVISE_MODES.join(", ")
            ));
        }
        // `@IsArray() @ArrayMaxSize(100) @IsUUID('4', { each: true })` (bawah → atas).
        let is_uuid = |v: &Value| v.as_str().is_some_and(is_uuid_v4);
        match self.user_ids.as_ref() {
            Some(Value::Array(a)) => {
                if !a.iter().all(is_uuid) {
                    e.push("each value in userIds must be a UUID".to_owned());
                }
                if a.len() > 100 {
                    e.push("userIds must contain no more than 100 elements".to_owned());
                }
            }
            other => {
                if !other.is_some_and(is_uuid) {
                    e.push("each value in userIds must be a UUID".to_owned());
                }
                e.push("userIds must contain no more than 100 elements".to_owned());
                e.push("userIds must be an array".to_owned());
            }
        }
        e
    }
}

impl PutRevisersDto {
    fn mode(&self) -> ReviseMode {
        self.mode
            .as_ref()
            .and_then(Value::as_str)
            .and_then(ReviseMode::parse)
            .unwrap_or(ReviseMode::AllPrivileged)
    }

    fn user_ids(&self) -> Vec<String> {
        self.user_ids
            .as_ref()
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }
}

// ── Handler ──────────────────────────────────────────────────────────────────

/// Node: src/store-visit-forms/store-visit-form-revisers.controller.ts:92 `findRevisers`.
async fn h_find(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedQuery(query): ValidatedQuery<ListRevisersQuery>,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    let q = query.q.as_ref().and_then(Value::as_str);
    Ok(Json(find_revisers(&state.db, user_id, q).await?))
}

/// Node: src/store-visit-forms/store-visit-form-revisers.controller.ts:152 `replaceRevisers`.
async fn h_replace(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    ValidatedJson(dto): ValidatedJson<PutRevisersDto>,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    Ok(Json(
        replace_revisers(&state.db, &audit, user_id, &dto).await?,
    ))
}

// ── Service ──────────────────────────────────────────────────────────────────

/// Baris `userAccessLevel` aktif + `accessLevel` (LEFT JOIN: relasi boleh kosong).
#[derive(sqlx::FromRow)]
struct MemberRow {
    #[sqlx(rename = "userId")]
    user_id: i32,
    uuid: Option<String>,
    id: Option<i32>,
    name: Option<String>,
    status: Option<i32>,
    code: Option<String>,
    #[sqlx(rename = "isSystem")]
    is_system: Option<bool>,
    #[sqlx(rename = "baseRole")]
    base_role: Option<String>,
}

impl MemberRow {
    fn role(&self) -> Option<RoleRow> {
        Some(RoleRow {
            id: self.id?,
            name: self.name.clone()?,
            status: self.status?,
            code: self.code.clone()?,
            is_system: self.is_system?,
            base_role: self.base_role.clone(),
        })
    }
}

const MEMBER_SELECT: &str = r#"SELECT ual."userId", u.uuid::text AS uuid,
        al.id, al.name, al.status, al.code, al."isSystem", al."baseRole"::text AS "baseRole"
   FROM "userAccessLevel" ual
   LEFT JOIN users u ON u.id = ual."userId"
   LEFT JOIN "accessLevel" al ON al.id = ual."accessLevelId"
  WHERE ual."companyId" = $1 AND ual.status = 1"#;

/// Jabatan efektif terpilih per user.
#[derive(Debug, Clone)]
struct EffectiveRole {
    code: String,
    name: String,
}

/// `pickRolesByUser` — urutan = kemunculan pertama user (urutan `Map` JS).
fn pick_roles_by_user(rows: &[MemberRow]) -> Vec<(i32, EffectiveRole)> {
    let mut order: Vec<i32> = Vec::new();
    let mut by_user: HashMap<i32, Vec<RoleRow>> = HashMap::new();
    for row in rows {
        let Some(role) = row.role() else {
            continue;
        };
        by_user
            .entry(row.user_id)
            .or_insert_with(|| {
                order.push(row.user_id);
                Vec::new()
            })
            .push(role);
    }
    order
        .into_iter()
        .filter_map(|uid| {
            let roles = by_user.get(&uid)?;
            let role = pick_role(roles.iter().map(Some))?;
            Some((
                uid,
                EffectiveRole {
                    code: effective_role_code_of(role),
                    name: role.name.clone(),
                },
            ))
        })
        .collect()
}

/// `findRevisers(userId, q)` — `{ data: { mode, revisers, candidates } }`.
/// Node: src/store-visit-forms/store-visit-form-revisers.service.ts:97
pub async fn find_revisers(db: &PgPool, user_id: i32, q: Option<&str>) -> ApiResult<Value> {
    let company_id = require_company_id(db, user_id).await?;

    let mode: Option<String> =
        sqlx::query_scalar(r#"SELECT "storeVisitFormReviseMode"::text FROM company WHERE id = $1"#)
            .bind(company_id)
            .fetch_optional(db)
            .await?
            .flatten();

    #[derive(sqlx::FromRow)]
    struct ReviserRow {
        uuid: String,
        #[sqlx(rename = "userId")]
        user_id: i32,
        #[sqlx(rename = "createdById")]
        created_by_id: i32,
        #[sqlx(rename = "createdAt")]
        created_at: chrono::NaiveDateTime,
    }
    let reviser_rows: Vec<ReviserRow> = sqlx::query_as(
        r#"SELECT uuid::text AS uuid, "userId", "createdById", "createdAt"
             FROM "storeVisitFormReviser"
            WHERE "companyId" = $1 AND "deletedAt" IS NULL
            ORDER BY "createdAt" ASC, id ASC"#,
    )
    .bind(company_id)
    .fetch_all(db)
    .await?;

    let role_rows: Vec<MemberRow> = sqlx::query_as(&format!("{MEMBER_SELECT} ORDER BY ual.id"))
        .bind(company_id)
        .fetch_all(db)
        .await?;
    let roles = pick_roles_by_user(&role_rows);
    let role_by_user: HashMap<i32, &EffectiveRole> =
        roles.iter().map(|(uid, r)| (*uid, r)).collect();

    let mut relevant: HashSet<i32> = HashSet::new();
    for r in &reviser_rows {
        relevant.insert(r.user_id);
        relevant.insert(r.created_by_id);
    }
    relevant.extend(roles.iter().map(|(uid, _)| *uid));
    let ids: Vec<i32> = relevant.into_iter().collect();

    #[derive(sqlx::FromRow)]
    struct UserRow {
        id: i32,
        uuid: String,
        name: String,
    }
    let users: Vec<UserRow> =
        sqlx::query_as(r#"SELECT id, uuid::text AS uuid, name FROM users WHERE id = ANY($1)"#)
            .bind(&ids)
            .fetch_all(db)
            .await?;
    let user_by_id: HashMap<i32, &UserRow> = users.iter().map(|u| (u.id, u)).collect();

    let person = |id: i32| {
        let u = user_by_id.get(&id);
        json!({ "id": u.map(|u| u.uuid.clone()), "name": u.map(|u| u.name.clone()) })
    };
    let revisers: Vec<Value> = reviser_rows
        .iter()
        .map(|row| {
            let role = role_by_user.get(&row.user_id);
            json!({
                "id": row.uuid,
                "user": person(row.user_id),
                "roleCode": role.map(|r| r.code.clone()),
                "roleName": role.map(|r| r.name.clone()),
                "eligible": role.is_some_and(|r| sees_whole_company(Some(&r.code))),
                "addedAt": row.created_at.and_utc().timestamp_millis(),
                "addedBy": person(row.created_by_id),
            })
        })
        .collect();

    let search = q
        .map(|s| crate::value_util::js_trim(s).to_lowercase())
        .filter(|s| !s.is_empty());
    let mut candidates: Vec<(&UserRow, &EffectiveRole)> = roles
        .iter()
        .filter(|(_, role)| sees_whole_company(Some(&role.code)))
        .filter_map(|(uid, role)| user_by_id.get(uid).map(|u| (*u, role)))
        .filter(|(u, _)| {
            search
                .as_deref()
                .is_none_or(|s| u.name.to_lowercase().contains(s))
        })
        .collect();
    candidates.sort_by(|a, b| mz_core::jsfmt::collate_id(&a.0.name, &b.0.name));
    let candidates: Vec<Value> = candidates
        .into_iter()
        .take(CANDIDATE_LIMIT)
        .map(|(u, role)| {
            json!({
                "id": u.uuid,
                "name": u.name,
                "roleCode": role.code,
                "roleName": role.name,
            })
        })
        .collect();

    Ok(json!({
        "data": {
            "mode": mode.unwrap_or_else(|| ReviseMode::AllPrivileged.as_str().to_owned()),
            "revisers": revisers,
            "candidates": candidates,
        }
    }))
}

/// `replaceRevisers(userId, dto)` — semantik REPLACE; validasi sebelum transaksi.
/// Node: src/store-visit-forms/store-visit-form-revisers.service.ts:191
pub async fn replace_revisers(
    db: &PgPool,
    audit: &AuditHandle,
    user_id: i32,
    dto: &PutRevisersDto,
) -> ApiResult<Value> {
    let company_id = require_company_id(db, user_id).await?;
    let mode = dto.mode();
    let mut requested: Vec<String> = Vec::new();
    for u in dto.user_ids() {
        if !requested.contains(&u) {
            requested.push(u);
        }
    }

    if mode == ReviseMode::Selected && requested.is_empty() {
        return Err(coded(
            StatusCode::UNPROCESSABLE_ENTITY,
            "FORM_REVISER_LIST_EMPTY",
            "At least one reviser is required when mode is selected",
        ));
    }

    let mut user_ids: Vec<i32> = Vec::new();
    if !requested.is_empty() {
        let members: Vec<MemberRow> = sqlx::query_as(&format!(
            "{MEMBER_SELECT} AND u.uuid = ANY($2::uuid[]) ORDER BY ual.id"
        ))
        .bind(company_id)
        .bind(&requested)
        .fetch_all(db)
        .await?;

        let mut roles_by_user: HashMap<i32, Vec<RoleRow>> = HashMap::new();
        let mut user_id_by_uuid: HashMap<String, i32> = HashMap::new();
        for row in &members {
            let Some(uuid) = row.uuid.clone() else {
                continue;
            };
            user_id_by_uuid.insert(uuid, row.user_id);
            if let Some(role) = row.role() {
                roles_by_user.entry(row.user_id).or_default().push(role);
            }
        }

        let missing: Vec<&String> = requested
            .iter()
            .filter(|u| !user_id_by_uuid.contains_key(*u))
            .collect();
        if !missing.is_empty() {
            return Err(coded_details(
                StatusCode::UNPROCESSABLE_ENTITY,
                "FORM_REVISER_INVALID",
                "Every reviser must be an active member of this company",
                json!({ "unknownUserIds": missing }),
            ));
        }

        let ineligible: Vec<&String> = requested
            .iter()
            .filter(|uuid| {
                let uid = user_id_by_uuid.get(*uuid).copied().unwrap_or_default();
                let roles = roles_by_user.get(&uid).map(Vec::as_slice).unwrap_or(&[]);
                pick_role(roles.iter().map(Some))
                    .is_none_or(|role| !sees_whole_company(Some(&effective_role_code_of(role))))
            })
            .collect();
        if !ineligible.is_empty() {
            return Err(coded_details(
                StatusCode::UNPROCESSABLE_ENTITY,
                "FORM_REVISER_ROLE_INELIGIBLE",
                format!(
                    "Every reviser must hold one of: {}",
                    COMPANY_WIDE_ROLE_CODES.join(", ")
                ),
                json!({ "userIds": ineligible }),
            ));
        }

        for uuid in &requested {
            if let Some(uid) = user_id_by_uuid.get(uuid)
                && !user_ids.contains(uid)
            {
                user_ids.push(*uid);
            }
        }
    }

    // Transaksi: `mode` + diff daftar bersama (§0.14). Rollback → audit dibuang.
    let discard_on_error = audit.discard_guard();
    let mut tx = db.begin().await?;

    let current_mode: Option<String> =
        sqlx::query_scalar(r#"SELECT "storeVisitFormReviseMode"::text FROM company WHERE id = $1"#)
            .bind(company_id)
            .fetch_optional(&mut *tx)
            .await?
            .flatten();
    if current_mode.as_deref() != Some(mode.as_str()) {
        // `tx.company.update({ where: { id }, data })` tanpa `select`.
        let pre = audit.pre_read(Model::Company, Lookup::Id(company_id)).await;
        let updated = sqlx::query(
            r#"UPDATE company SET "storeVisitFormReviseMode" = $1::"StoreVisitFormReviseMode"
                WHERE id = $2"#,
        )
        .bind(mode.as_str())
        .bind(company_id)
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() == 0 {
            // Prisma P2025 (`update` pada baris yang hilang) → 500 di Node.
            return Err(mz_core::ApiError::internal(anyhow::anyhow!(
                "company {company_id} not found"
            )));
        }
        if let Some(row) = audit
            .result_row(&mut *tx, Model::Company, Lookup::Id(company_id))
            .await
        {
            audit.record_update(Model::Company, pre, &row).await;
        }
    }

    let current: Vec<(i32, i32)> = sqlx::query_as(
        r#"SELECT id, "userId" FROM "storeVisitFormReviser"
            WHERE "companyId" = $1 AND "deletedAt" IS NULL"#,
    )
    .bind(company_id)
    .fetch_all(&mut *tx)
    .await?;

    let wanted: HashSet<i32> = user_ids.iter().copied().collect();
    let revoked: Vec<i32> = current
        .iter()
        .filter(|(_, uid)| !wanted.contains(uid))
        .map(|(id, _)| *id)
        .collect();
    if !revoked.is_empty() {
        // `updateMany({ where: { id: { in } }, data: { deletedAt } })` = anggota keluar.
        let membership = audit
            .membership_pre(
                Model::StoreVisitFormReviser,
                MemberEffect::Remove,
                Touched::Where(Lookup::Ids(revoked.clone())),
            )
            .await;
        sqlx::query(r#"UPDATE "storeVisitFormReviser" SET "deletedAt" = $2 WHERE id = ANY($1)"#)
            .bind(&revoked)
            .bind(now_naive())
            .execute(&mut *tx)
            .await?;
        audit.record_membership(membership).await;
    }

    let kept: HashSet<i32> = current.iter().map(|(_, uid)| *uid).collect();
    let added: Vec<i32> = user_ids
        .iter()
        .copied()
        .filter(|id| !kept.contains(id))
        .collect();
    if !added.is_empty() {
        // `createMany({ data, skipDuplicates: true })` = anggota masuk.
        let rows: Vec<Row> = added
            .iter()
            .map(|uid| {
                Row::new()
                    .with("companyId", company_id)
                    .with("userId", *uid)
                    .with("createdById", user_id)
            })
            .collect();
        let membership = audit
            .membership_pre(
                Model::StoreVisitFormReviser,
                MemberEffect::Add,
                Touched::Rows(rows),
            )
            .await;
        sqlx::query(
            r#"INSERT INTO "storeVisitFormReviser" ("companyId", "userId", "createdById")
               SELECT $1, u, $3 FROM UNNEST($2::int[]) AS t(u)
               ON CONFLICT DO NOTHING"#,
        )
        .bind(company_id)
        .bind(&added)
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
        audit.record_membership(membership).await;
    }

    tx.commit().await?;
    discard_on_error.disarm();

    find_revisers(db, user_id, None).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(v: Value) -> Vec<String> {
        serde_json::from_value::<PutRevisersDto>(v)
            .unwrap()
            .validate()
    }

    fn member(uid: i32, code: &str, base: Option<&str>, status: i32, name: &str) -> MemberRow {
        MemberRow {
            user_id: uid,
            uuid: Some(format!("uuid-{uid}")),
            id: Some(uid * 10),
            name: Some(name.to_owned()),
            status: Some(status),
            code: Some(code.to_owned()),
            is_system: Some(base.is_none()),
            base_role: base.map(str::to_owned),
        }
    }

    #[test]
    fn put_dto_messages() {
        assert!(
            put(json!({"mode": "selected", "userIds": ["9a8b7c6d-5e4f-4a3b-8c2d-1e0f9a8b7c6d"]}))
                .is_empty()
        );
        assert_eq!(
            put(json!({})),
            vec![
                "mode must be one of the following values: allPrivileged, selected",
                "each value in userIds must be a UUID",
                "userIds must contain no more than 100 elements",
                "userIds must be an array",
            ]
        );
        assert_eq!(
            put(json!({"mode": "allPrivileged", "userIds": ["x"]})),
            vec!["each value in userIds must be a UUID"]
        );
        let many: Vec<String> = (0..101)
            .map(|i| format!("9a8b7c6d-5e4f-4a3b-8c2d-{i:012}"))
            .collect();
        assert_eq!(
            put(json!({"mode": "allPrivileged", "userIds": many})),
            vec!["userIds must contain no more than 100 elements"]
        );
    }

    #[test]
    fn query_dto_messages() {
        let q = |v: Value| {
            serde_json::from_value::<ListRevisersQuery>(v)
                .unwrap()
                .validate()
        };
        assert!(q(json!({})).is_empty());
        assert!(q(json!({"q": "budi"})).is_empty());
        assert_eq!(
            q(json!({"q": "a".repeat(256)})),
            vec!["q must be shorter than or equal to 255 characters"]
        );
        assert_eq!(
            q(json!({"q": ["a", "b"]})),
            vec![
                "q must be shorter than or equal to 255 characters",
                "q must be a string"
            ]
        );
    }

    #[test]
    fn picks_highest_role_per_user_in_first_seen_order() {
        let rows = vec![
            member(2, "STAFF", None, 1, "Staff"),
            member(1, "ADMIN", None, 1, "Admin"),
            member(2, "MANAGER", None, 1, "Manager"),
            member(3, "custom", Some("ADMIN"), 0, "Nonaktif"),
        ];
        let picked = pick_roles_by_user(&rows);
        let codes: Vec<(i32, &str)> = picked.iter().map(|(u, r)| (*u, r.code.as_str())).collect();
        assert_eq!(codes, vec![(2, "MANAGER"), (1, "ADMIN")]);
        // Relasi accessLevel kosong tidak memberi apa pun.
        let mut empty = member(4, "ADMIN", None, 1, "x");
        empty.id = None;
        assert!(pick_roles_by_user(&[empty]).is_empty());
    }
}
