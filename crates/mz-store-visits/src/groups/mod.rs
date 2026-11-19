//! Port `src/store-visits/admin-store-groups.controller.ts` + `store-groups.service.ts` (§4.1):
//! CRUD region toko + penugasan sales ke region (`/v1/admin/store-groups`, 8 route).
//!
//! - Arsip (`DELETE`) berjalan `SERIALIZABLE` dengan retry (P2034 → SQLSTATE 40001/40P01, 3 kali).
//! - Setiap hitungan toko memfilter `deletedAt IS NULL`; hitungan sales hanya anggota aktif company.
//! - `storeGroup` & `storeGroupUser` (keanggotaan) ber-audit lewat `AuditHandle`.

pub mod dto;

use std::collections::{HashMap, HashSet};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, patch, post, put};
use axum::{Json, Router};
use mz_audit::{AuditHandle, Lookup, MemberEffect, Model, Row, Touched};
use mz_core::company::require_company_id;
use mz_core::enums::FeatureModule;
use mz_core::extract::{ValidatedJson, ValidatedQuery};
use mz_core::gate::guard;
use mz_core::time::{JsDate, now_naive};
use mz_core::{ApiError, ApiResult, AppState, RequestCtx, RouteSpec};
use mz_sales_geo::dto::{Field, Raw};
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool, Postgres, QueryBuilder};

use self::dto::{
    CreateStoreGroup, ListStoreGroups, UpdateStoreGroup, parse_create, parse_list,
    parse_replace_sales, parse_update,
};
use crate::enums::{SortOrder, StoreGroupSortField};

/// `SALES_ROLE_CODE` — role yang dihitung sebagai "sales lapangan" pada `unassignedSalesCount`.
const SALES_ROLE_CODE: &str = "STAFF";
/// Percobaan arsip saat transaksi `Serializable` dibatalkan PostgreSQL.
const ARCHIVE_MAX_ATTEMPTS: usize = 3;

/// `AdminStoreGroupsController` (class-level decorators).
const SPEC: RouteSpec = RouteSpec::jwt()
    .feature("store-groups")
    .modules(&[FeatureModule::SALES_TRACKER])
    .roles(crate::ADMIN_ROLES);

pub(crate) fn routes(state: &AppState) -> Router<AppState> {
    const P: &str = "/v1/admin/store-groups";
    Router::new()
        .route(P, guard(state, SPEC, get(h_find_all)))
        .route(P, guard(state, SPEC, post(h_create)))
        .route(
            &format!("{P}/{{uuid}}"),
            guard(state, SPEC, get(h_find_one)),
        )
        .route(
            &format!("{P}/{{uuid}}"),
            guard(state, SPEC, patch(h_update)),
        )
        .route(
            &format!("{P}/{{uuid}}"),
            guard(state, SPEC, delete(h_remove)),
        )
        .route(
            &format!("{P}/{{uuid}}/restore"),
            guard(state, SPEC, post(h_restore)),
        )
        .route(
            &format!("{P}/{{uuid}}/sales"),
            guard(state, SPEC, get(h_find_sales)),
        )
        .route(
            &format!("{P}/{{uuid}}/sales"),
            guard(state, SPEC, put(h_replace_sales)),
        )
}

// ───────────────────────── baris & serializer ─────────────────────────

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct StoreGroupRow {
    pub id: i32,
    pub uuid: String,
    pub code: String,
    pub name: String,
    pub description: Option<String>,
    #[sqlx(rename = "isActive")]
    pub is_active: bool,
    #[sqlx(rename = "createdAt")]
    pub created_at: JsDate,
    #[sqlx(rename = "updatedAt")]
    pub updated_at: Option<JsDate>,
}

const GROUP_COLS: &str =
    r#"id, uuid::text AS uuid, code, name, description, "isActive", "createdAt", "updatedAt""#;

/// `toStoreGroup(group, counts)`.
pub fn to_store_group(g: &StoreGroupRow, store_count: i64, sales_count: i64) -> Value {
    json!({
        "id": g.uuid,
        "code": g.code,
        "name": g.name,
        "description": g.description,
        "isActive": g.is_active,
        "storeCount": store_count,
        "salesCount": sales_count,
        "createdAt": g.created_at.timestamp_millis(),
        "updatedAt": g.updated_at.map(|d| d.timestamp_millis()),
    })
}

fn internal(msg: &str) -> ApiError {
    ApiError::internal(anyhow::anyhow!("{msg}"))
}

/// Kolom `@db.Uuid`: string non-UUID → P2023 Prisma → 500.
pub(crate) fn row_uuid(raw: &str) -> ApiResult<uuid::Uuid> {
    mz_core::prisma::parse_uuid(raw).map_err(|m| internal(&m))
}

fn not_found() -> ApiError {
    ApiError::coded(
        StatusCode::NOT_FOUND,
        "STORE_GROUP_NOT_FOUND",
        "Store group not found",
    )
}

fn code_taken(code: &str) -> ApiError {
    ApiError::coded(
        StatusCode::CONFLICT,
        "STORE_GROUP_CODE_TAKEN",
        format!("Store group code {code} is already used in this company"),
    )
}

async fn find_group_or_throw(
    conn: &mut PgConnection,
    company_id: i32,
    uuid: &str,
) -> ApiResult<StoreGroupRow> {
    let u = row_uuid(uuid)?;
    sqlx::query_as::<_, StoreGroupRow>(&format!(
        r#"SELECT {GROUP_COLS} FROM "storeGroup" WHERE uuid = $1 AND "companyId" = $2 LIMIT 1"#
    ))
    .bind(u)
    .bind(company_id)
    .fetch_optional(conn)
    .await?
    .ok_or_else(not_found)
}

// ───────────────────────── hitungan ─────────────────────────

/// `storeCounts`: toko hidup per region.
async fn store_counts(conn: &mut PgConnection, group_ids: &[i32]) -> ApiResult<HashMap<i32, i64>> {
    if group_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<(i32, i64)> = sqlx::query_as(
        r#"SELECT "groupId", COUNT(*) FROM store
            WHERE "groupId" = ANY($1) AND "deletedAt" IS NULL GROUP BY "groupId""#,
    )
    .bind(group_ids)
    .fetch_all(conn)
    .await?;
    Ok(rows.into_iter().collect())
}

/// `salesCounts`: assignment hidup per region, hanya user yang masih anggota aktif company.
async fn sales_counts(
    conn: &mut PgConnection,
    group_ids: &[i32],
    company_id: i32,
) -> ApiResult<HashMap<i32, i64>> {
    if group_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<(i32, i64)> = sqlx::query_as(
        r#"SELECT sgu."groupId", COUNT(*) FROM "storeGroupUser" sgu
            WHERE sgu."groupId" = ANY($1) AND sgu."deletedAt" IS NULL
              AND EXISTS (SELECT 1 FROM "userCompany" uc
                           WHERE uc."companyId" = $2 AND uc.status = 1 AND uc."userId" = sgu."userId")
            GROUP BY sgu."groupId""#,
    )
    .bind(group_ids)
    .bind(company_id)
    .fetch_all(conn)
    .await?;
    Ok(rows.into_iter().collect())
}

/// `activeMemberIds`: subset `user_ids` yang anggota aktif company.
async fn active_member_ids(
    db: &PgPool,
    company_id: i32,
    user_ids: &[i32],
) -> ApiResult<HashSet<i32>> {
    if user_ids.is_empty() {
        return Ok(HashSet::new());
    }
    let ids: Vec<i32> = sqlx::query_scalar(
        r#"SELECT "userId" FROM "userCompany" WHERE "companyId" = $1 AND status = 1 AND "userId" = ANY($2)"#,
    )
    .bind(company_id)
    .bind(user_ids)
    .fetch_all(db)
    .await?;
    Ok(ids.into_iter().collect())
}

/// §0.20 `unassignedSalesCount`: STAFF aktif (`roleCodeFilter(['STAFF'])`) tanpa assignment hidup.
async fn unassigned_sales_count(db: &PgPool, company_id: i32) -> ApiResult<i64> {
    Ok(sqlx::query_scalar(
        r#"SELECT COUNT(DISTINCT uc."userId") FROM "userCompany" uc
            WHERE uc."companyId" = $1 AND uc.status = 1
              AND EXISTS (SELECT 1 FROM "userAccessLevel" ual
                            JOIN "accessLevel" al ON al.id = ual."accessLevelId"
                           WHERE ual."userId" = uc."userId" AND ual.status = 1 AND ual."companyId" = $1
                             AND ((al."isSystem" = true AND al.code = $2)
                                  OR (al."isSystem" = false AND al."baseRole"::text = $2)))
              AND NOT EXISTS (SELECT 1 FROM "storeGroupUser" sgu
                                JOIN "storeGroup" g ON g.id = sgu."groupId"
                               WHERE sgu."userId" = uc."userId" AND sgu."deletedAt" IS NULL
                                 AND g."companyId" = $1)"#,
    )
    .bind(company_id)
    .bind(SALES_ROLE_CODE)
    .fetch_one(db)
    .await?)
}

async fn counts_for(db: &PgPool, group_id: i32, company_id: i32) -> ApiResult<(i64, i64)> {
    let mut conn = db.acquire().await?;
    let s = store_counts(&mut conn, &[group_id]).await?;
    let sa = sales_counts(&mut conn, &[group_id], company_id).await?;
    Ok((
        s.get(&group_id).copied().unwrap_or(0),
        sa.get(&group_id).copied().unwrap_or(0),
    ))
}

// ───────────────────────── service ─────────────────────────

/// `create(userId, dto)`.
pub async fn create(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    dto: &CreateStoreGroup,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let res = sqlx::query_as::<_, StoreGroupRow>(&format!(
        r#"INSERT INTO "storeGroup" ("companyId", code, name, description, "createdById")
           VALUES ($1, $2, $3, $4, $5) RETURNING {GROUP_COLS}"#
    ))
    .bind(company_id)
    .bind(&dto.code)
    .bind(&dto.name)
    .bind(&dto.description)
    .bind(user_id)
    .fetch_one(&state.db)
    .await;
    let group = match res {
        Ok(g) => g,
        Err(e) if mz_core::db::is_unique_violation(&e) => return Err(code_taken(&dto.code)),
        Err(e) => return Err(e.into()),
    };
    if let Some(r) = audit
        .result_row(&state.db, Model::StoreGroup, Lookup::Id(group.id))
        .await
    {
        audit.record_create(Model::StoreGroup, &r).await;
    }
    Ok(to_store_group(&group, 0, 0))
}

/// `findAll(userId, query)` → `{ data, summary, meta }`.
pub async fn find_all(state: &AppState, user_id: i32, q: &ListStoreGroups) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let push_where = |qb: &mut QueryBuilder<'_, Postgres>| {
        qb.push(r#" WHERE "companyId" = "#).push_bind(company_id);
        if let Some(a) = q.is_active {
            qb.push(r#" AND "isActive" = "#).push_bind(a);
        }
        if let Some(s) = q.search.as_deref().filter(|s| !s.is_empty()) {
            qb.push(" AND (");
            for (i, col) in ["code", "name", "description"].iter().enumerate() {
                if i > 0 {
                    qb.push(" OR ");
                }
                // Prisma `contains` + `insensitive` = ILIKE tanpa escape `%`/`_`.
                qb.push(format!(r#""{col}" ILIKE '%' || "#))
                    .push_bind(s.to_owned())
                    .push(" || '%'");
            }
            qb.push(")");
        }
    };
    let col = match q.sort {
        StoreGroupSortField::code => "code",
        StoreGroupSortField::createdAt => "\"createdAt\"",
        StoreGroupSortField::name => "name",
    };
    let dir = match q.order {
        SortOrder::asc => "ASC",
        SortOrder::desc => "DESC",
    };
    let mut qb = QueryBuilder::<Postgres>::new(format!(r#"SELECT {GROUP_COLS} FROM "storeGroup""#));
    push_where(&mut qb);
    qb.push(format!(" ORDER BY {col} {dir}, id ASC OFFSET "))
        .push_bind((q.page - 1) * q.limit)
        .push(" LIMIT ")
        .push_bind(q.limit);
    let mut cq = QueryBuilder::<Postgres>::new(r#"SELECT COUNT(*) FROM "storeGroup""#);
    push_where(&mut cq);

    let groups: Vec<StoreGroupRow> = qb.build_query_as().fetch_all(&state.db).await?;
    let total: i64 = cq.build_query_scalar().fetch_one(&state.db).await?;
    let unassigned_store_count: i64 = sqlx::query_scalar(
        r#"SELECT COUNT(*) FROM store WHERE "companyId" = $1 AND "deletedAt" IS NULL AND "groupId" IS NULL"#,
    )
    .bind(company_id)
    .fetch_one(&state.db)
    .await?;
    let unassigned_sales = unassigned_sales_count(&state.db, company_id).await?;

    let ids: Vec<i32> = groups.iter().map(|g| g.id).collect();
    let mut conn = state.db.acquire().await?;
    let stores = store_counts(&mut conn, &ids).await?;
    let sales = sales_counts(&mut conn, &ids, company_id).await?;
    drop(conn);

    let data: Vec<Value> = groups
        .iter()
        .map(|g| {
            to_store_group(
                g,
                stores.get(&g.id).copied().unwrap_or(0),
                sales.get(&g.id).copied().unwrap_or(0),
            )
        })
        .collect();
    Ok(json!({
        "data": data,
        "summary": { "unassignedStoreCount": unassigned_store_count, "unassignedSalesCount": unassigned_sales },
        "meta": { "total": total, "page": q.page, "limit": q.limit, "totalPages": total_pages(total, q.limit) },
    }))
}

/// `Math.ceil(total / limit)`.
pub(crate) fn total_pages(total: i64, limit: i64) -> i64 {
    if limit <= 0 {
        return 0;
    }
    (total + limit - 1) / limit
}

/// `findOne(userId, uuid)` → group + `sales`.
pub async fn find_one(state: &AppState, user_id: i32, uuid: &str) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let mut conn = state.db.acquire().await?;
    let group = find_group_or_throw(&mut conn, company_id, uuid).await?;
    drop(conn);
    let (sc, sa) = counts_for(&state.db, group.id, company_id).await?;
    let members = serialize_members(&state.db, group.id, company_id).await?;
    let mut v = to_store_group(&group, sc, sa);
    if let Some(o) = v.as_object_mut() {
        o.insert("sales".into(), Value::Array(members));
    }
    Ok(v)
}

/// `update(userId, uuid, dto)` (`isActive` tidak tersentuh).
pub async fn update(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    uuid: &str,
    dto: &UpdateStoreGroup,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let mut conn = state.db.acquire().await?;
    let group = find_group_or_throw(&mut conn, company_id, uuid).await?;
    drop(conn);
    // `code: null`/`name: null` lolos `@IsOptional` lalu ditolak Prisma (kolom wajib) → 500.
    if matches!(dto.code, Field::Null) || matches!(dto.name, Field::Null) {
        return Err(internal("Argument must not be null"));
    }
    let pre = audit
        .pre_read(Model::StoreGroup, Lookup::Id(group.id))
        .await;
    let mut qb = QueryBuilder::<Postgres>::new(r#"UPDATE "storeGroup" SET "updatedAt" = "#);
    qb.push_bind(now_naive());
    if let Field::Val(c) = &dto.code {
        qb.push(", code = ").push_bind(c.clone());
    }
    if let Field::Val(n) = &dto.name {
        qb.push(", name = ").push_bind(n.clone());
    }
    match &dto.description {
        Field::Val(d) => {
            qb.push(", description = ").push_bind(Some(d.clone()));
        }
        Field::Null => {
            qb.push(", description = NULL");
        }
        Field::Absent => {}
    }
    qb.push(" WHERE id = ").push_bind(group.id);
    qb.push(format!(" RETURNING {GROUP_COLS}"));
    let updated: StoreGroupRow = match qb.build_query_as().fetch_one(&state.db).await {
        Ok(r) => r,
        Err(e) if mz_core::db::is_unique_violation(&e) => {
            let code = match &dto.code {
                Field::Val(c) => c.clone(),
                _ => group.code.clone(),
            };
            return Err(code_taken(&code));
        }
        Err(e) => return Err(e.into()),
    };
    if let Some(r) = audit
        .result_row(&state.db, Model::StoreGroup, Lookup::Id(group.id))
        .await
    {
        audit.record_update(Model::StoreGroup, pre, &r).await;
    }
    let (sc, sa) = counts_for(&state.db, group.id, company_id).await?;
    Ok(to_store_group(&updated, sc, sa))
}

/// P2034 Prisma (konflik tulis/deadlock) = SQLSTATE 40001/40P01.
fn is_serialization_failure(e: &ApiError) -> bool {
    let ApiError::Internal(err) = e else {
        return false;
    };
    err.downcast_ref::<sqlx::Error>()
        .and_then(|e| e.as_database_error())
        .and_then(|d| d.code())
        .is_some_and(|c| c == "40001" || c == "40P01")
}

/// Satu percobaan arsip dalam transaksi `SERIALIZABLE`.
async fn archive_attempt(
    state: &AppState,
    audit: &AuditHandle,
    company_id: i32,
    uuid: &str,
) -> ApiResult<Value> {
    let mut tx = state.db.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
        .execute(&mut *tx)
        .await?;
    let group = find_group_or_throw(&mut tx, company_id, uuid).await?;
    let stores = store_counts(&mut tx, &[group.id]).await?;
    let sales = sales_counts(&mut tx, &[group.id], company_id).await?;
    let store_count = stores.get(&group.id).copied().unwrap_or(0);
    let sales_count = sales.get(&group.id).copied().unwrap_or(0);
    if store_count > 0 || sales_count > 0 {
        return Err(ApiError::body(
            StatusCode::CONFLICT,
            json!({
                "code": "STORE_GROUP_NOT_EMPTY",
                "message": "Move its stores and sales elsewhere before archiving this store group",
                "details": { "storeCount": store_count, "salesCount": sales_count },
            }),
        ));
    }
    let pre = audit
        .pre_read(Model::StoreGroup, Lookup::Id(group.id))
        .await;
    sqlx::query(r#"UPDATE "storeGroup" SET "isActive" = false, "updatedAt" = $2 WHERE id = $1"#)
        .bind(group.id)
        .bind(now_naive())
        .execute(&mut *tx)
        .await?;
    if let Some(r) = audit
        .result_row(&mut *tx, Model::StoreGroup, Lookup::Id(group.id))
        .await
    {
        audit.record_update(Model::StoreGroup, pre, &r).await;
    }
    tx.commit().await?;
    Ok(json!({ "id": group.uuid, "archived": true }))
}

/// `remove(userId, uuid)` — §0.9/§5.4 arsip, ditolak kalau masih berisi.
pub async fn remove(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    uuid: &str,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let mut attempt = 1;
    loop {
        match archive_attempt(state, audit, company_id, uuid).await {
            Ok(v) => return Ok(v),
            Err(e) => {
                // §9.1 — percobaan yang gagal sudah rollback; barisnya ikut pergi.
                audit.discard();
                if is_serialization_failure(&e) && attempt < ARCHIVE_MAX_ATTEMPTS {
                    attempt += 1;
                    continue;
                }
                return Err(e);
            }
        }
    }
}

/// `restore(userId, uuid)` — tanpa pemeriksaan kode (§0.16).
pub async fn restore(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    uuid: &str,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let mut conn = state.db.acquire().await?;
    let group = find_group_or_throw(&mut conn, company_id, uuid).await?;
    drop(conn);
    let pre = audit
        .pre_read(Model::StoreGroup, Lookup::Id(group.id))
        .await;
    let updated: StoreGroupRow = sqlx::query_as(&format!(
        r#"UPDATE "storeGroup" SET "isActive" = true, "updatedAt" = $2 WHERE id = $1 RETURNING {GROUP_COLS}"#
    ))
    .bind(group.id)
    .bind(now_naive())
    .fetch_one(&state.db)
    .await?;
    if let Some(r) = audit
        .result_row(&state.db, Model::StoreGroup, Lookup::Id(group.id))
        .await
    {
        audit.record_update(Model::StoreGroup, pre, &r).await;
    }
    let (sc, sa) = counts_for(&state.db, group.id, company_id).await?;
    Ok(to_store_group(&updated, sc, sa))
}

/// `serializeMembers(groupId, companyId)`: assignment hidup (urut `id`), yang sudah bukan anggota
/// aktif ditandai `isActiveMember: false`.
async fn serialize_members(db: &PgPool, group_id: i32, company_id: i32) -> ApiResult<Vec<Value>> {
    let rows: Vec<(i32, JsDate)> = sqlx::query_as(
        r#"SELECT "userId", "createdAt" FROM "storeGroupUser"
            WHERE "groupId" = $1 AND "deletedAt" IS NULL ORDER BY id ASC"#,
    )
    .bind(group_id)
    .fetch_all(db)
    .await?;
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let mut ids: Vec<i32> = rows.iter().map(|(u, _)| *u).collect();
    ids.sort_unstable();
    ids.dedup();
    let users: Vec<(i32, String, String, String)> =
        sqlx::query_as(r#"SELECT id, uuid::text, name, username FROM users WHERE id = ANY($1)"#)
            .bind(&ids)
            .fetch_all(db)
            .await?;
    let active = active_member_ids(db, company_id, &ids).await?;
    let by_id: HashMap<i32, &(i32, String, String, String)> =
        users.iter().map(|u| (u.0, u)).collect();
    Ok(rows
        .iter()
        .map(|(uid, created)| {
            let u = by_id.get(uid);
            json!({
                "id": u.map(|u| u.1.clone()),
                "userId": uid,
                "name": u.map(|u| u.2.clone()),
                "username": u.map(|u| u.3.clone()),
                "isActiveMember": active.contains(uid),
                "assignedAt": created.timestamp_millis(),
            })
        })
        .collect())
}

/// `findSales(userId, uuid)` → `{ data }`.
pub async fn find_sales(state: &AppState, user_id: i32, uuid: &str) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let mut conn = state.db.acquire().await?;
    let group = find_group_or_throw(&mut conn, company_id, uuid).await?;
    drop(conn);
    Ok(json!({ "data": serialize_members(&state.db, group.id, company_id).await? }))
}

/// `replaceSales(userId, uuid, dto)` — ganti seluruh daftar (diff), satu transaksi.
pub async fn replace_sales(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    uuid: &str,
    user_uuids: &[String],
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let mut conn = state.db.acquire().await?;
    let group = find_group_or_throw(&mut conn, company_id, uuid).await?;
    drop(conn);

    let mut requested: Vec<String> = Vec::new();
    for u in user_uuids {
        if !requested.contains(u) {
            requested.push(u.clone());
        }
    }
    let mut user_ids: Vec<i32> = Vec::new();
    if !requested.is_empty() {
        let parsed: Vec<uuid::Uuid> = requested
            .iter()
            .map(|u| row_uuid(u))
            .collect::<ApiResult<_>>()?;
        let members: Vec<(i32, String)> = sqlx::query_as(
            r#"SELECT uc."userId", u.uuid::text FROM "userCompany" uc JOIN users u ON u.id = uc."userId"
                WHERE uc."companyId" = $1 AND uc.status = 1 AND u.uuid = ANY($2) ORDER BY uc.id"#,
        )
        .bind(company_id)
        .bind(&parsed)
        .fetch_all(&state.db)
        .await?;
        let found: HashMap<String, i32> = members.iter().map(|(id, u)| (u.clone(), *id)).collect();
        let missing: Vec<&String> = requested
            .iter()
            .filter(|u| !found.contains_key(*u))
            .collect();
        if !missing.is_empty() {
            return Err(ApiError::body(
                StatusCode::UNPROCESSABLE_ENTITY,
                json!({
                    "code": "STORE_GROUP_ASSIGNEE_INVALID",
                    "message": "Every assignee must be an active employee of this company",
                    "details": { "unknownUserIds": missing },
                }),
            ));
        }
        // Node: `[...new Set(foundByUuid.values())]` — urutan baris `userCompany` (findMany tanpa
        // `orderBy` ≈ urutan heap; di sini `ORDER BY uc.id`), BUKAN urutan request.
        for (id, _) in &members {
            if !user_ids.contains(id) {
                user_ids.push(*id);
            }
        }
    }

    let res: ApiResult<()> = async {
        let mut tx = state.db.begin().await?;
        if !user_ids.is_empty() {
            let active: Option<bool> =
                sqlx::query_scalar(r#"SELECT "isActive" FROM "storeGroup" WHERE id = $1"#)
                    .bind(group.id)
                    .fetch_optional(&mut *tx)
                    .await?;
            if active != Some(true) {
                return Err(ApiError::coded(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "STORE_GROUP_INACTIVE",
                    "Cannot assign sales to an archived store group",
                ));
            }
        }
        let current: Vec<(i32, i32)> = sqlx::query_as(
            r#"SELECT id, "userId" FROM "storeGroupUser" WHERE "groupId" = $1 AND "deletedAt" IS NULL ORDER BY id"#,
        )
        .bind(group.id)
        .fetch_all(&mut *tx)
        .await?;
        let wanted: HashSet<i32> = user_ids.iter().copied().collect();
        let revoked: Vec<i32> = current
            .iter()
            .filter(|(_, u)| !wanted.contains(u))
            .map(|(id, _)| *id)
            .collect();
        if !revoked.is_empty() {
            let pre = audit
                .membership_pre(
                    Model::StoreGroupUser,
                    MemberEffect::Remove,
                    Touched::Where(Lookup::Ids(revoked.clone())),
                )
                .await;
            sqlx::query(r#"UPDATE "storeGroupUser" SET "deletedAt" = $2 WHERE id = ANY($1)"#)
                .bind(&revoked)
                .bind(now_naive())
                .execute(&mut *tx)
                .await?;
            audit.record_membership(pre).await;
        }
        let kept: HashSet<i32> = current.iter().map(|(_, u)| *u).collect();
        let added: Vec<i32> = user_ids.iter().copied().filter(|u| !kept.contains(u)).collect();
        if !added.is_empty() {
            let rows: Vec<Row> = added
                .iter()
                .map(|u| Row::new().with("groupId", group.id).with("userId", *u))
                .collect();
            let pre = audit
                .membership_pre(Model::StoreGroupUser, MemberEffect::Add, Touched::Rows(rows))
                .await;
            sqlx::query(
                r#"INSERT INTO "storeGroupUser" ("groupId", "userId")
                   SELECT $1, u FROM UNNEST($2::int[]) AS u ON CONFLICT DO NOTHING"#,
            )
            .bind(group.id)
            .bind(&added)
            .execute(&mut *tx)
            .await?;
            audit.record_membership(pre).await;
        }
        tx.commit().await?;
        Ok(())
    }
    .await;
    if let Err(e) = res {
        audit.discard();
        return Err(e);
    }
    Ok(json!({ "data": serialize_members(&state.db, group.id, company_id).await? }))
}

// ───────────────────────── handler ─────────────────────────

/// Node: src/store-visits/admin-store-groups.controller.ts:85 `findAll`
async fn h_find_all(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedQuery(q): ValidatedQuery<Raw>,
) -> ApiResult<Json<Value>> {
    let q = parse_list(&q.0).map_err(ApiError::Validation)?;
    Ok(Json(find_all(&state, ctx.require_user_id()?, &q).await?))
}

/// Node: src/store-visits/admin-store-groups.controller.ts:110 `create` (201)
async fn h_create(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    ValidatedJson(body): ValidatedJson<Raw>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let dto = parse_create(&body.0).map_err(ApiError::Validation)?;
    let data = create(&state, &audit, ctx.require_user_id()?, &dto).await?;
    Ok((StatusCode::CREATED, Json(json!({ "data": data }))))
}

/// Node: src/store-visits/admin-store-groups.controller.ts:158 `findOne`
async fn h_find_one(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(uuid): Path<String>,
) -> ApiResult<Json<Value>> {
    let data = find_one(&state, ctx.require_user_id()?, &uuid).await?;
    Ok(Json(json!({ "data": data })))
}

/// Node: src/store-visits/admin-store-groups.controller.ts:178 `update`
async fn h_update(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    Path(uuid): Path<String>,
    ValidatedJson(body): ValidatedJson<Raw>,
) -> ApiResult<Json<Value>> {
    let dto = parse_update(&body.0).map_err(ApiError::Validation)?;
    let data = update(&state, &audit, ctx.require_user_id()?, &uuid, &dto).await?;
    Ok(Json(json!({ "data": data })))
}

/// Node: src/store-visits/admin-store-groups.controller.ts:219 `remove` (200)
async fn h_remove(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    Path(uuid): Path<String>,
) -> ApiResult<Json<Value>> {
    let data = remove(&state, &audit, ctx.require_user_id()?, &uuid).await?;
    Ok(Json(json!({ "data": data })))
}

/// Node: src/store-visits/admin-store-groups.controller.ts:255 `restore` (200)
async fn h_restore(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    Path(uuid): Path<String>,
) -> ApiResult<Json<Value>> {
    let data = restore(&state, &audit, ctx.require_user_id()?, &uuid).await?;
    Ok(Json(json!({ "data": data })))
}

/// Node: src/store-visits/admin-store-groups.controller.ts:275 `findSales`
async fn h_find_sales(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(uuid): Path<String>,
) -> ApiResult<Json<Value>> {
    Ok(Json(
        find_sales(&state, ctx.require_user_id()?, &uuid).await?,
    ))
}

/// Node: src/store-visits/admin-store-groups.controller.ts:290 `replaceSales` (200)
async fn h_replace_sales(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    Path(uuid): Path<String>,
    ValidatedJson(body): ValidatedJson<Raw>,
) -> ApiResult<Json<Value>> {
    let user_uuids = parse_replace_sales(&body.0).map_err(ApiError::Validation)?;
    Ok(Json(
        replace_sales(&state, &audit, ctx.require_user_id()?, &uuid, &user_uuids).await?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializer_shape() {
        let g = StoreGroupRow {
            id: 1,
            uuid: "u".into(),
            code: "JOG".into(),
            name: "Jogja".into(),
            description: None,
            is_active: true,
            created_at: JsDate::from_millis(1_786_000_000_000).unwrap(),
            updated_at: None,
        };
        let v = to_store_group(&g, 0, 0);
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            vec![
                "id",
                "code",
                "name",
                "description",
                "isActive",
                "storeCount",
                "salesCount",
                "createdAt",
                "updatedAt"
            ]
        );
        assert_eq!(v["createdAt"], json!(1_786_000_000_000_i64));
        assert_eq!(v["updatedAt"], Value::Null);
    }

    #[test]
    fn pages() {
        assert_eq!(total_pages(0, 20), 0);
        assert_eq!(total_pages(41, 20), 3);
        assert_eq!(total_pages(40, 20), 2);
    }
}
