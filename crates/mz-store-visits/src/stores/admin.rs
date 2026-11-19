//! Port `src/store-visits/stores.service.ts` + `admin-stores.controller.ts` (9 route
//! `/v1/admin/stores`). Model `store` ber-audit (`AuditHandle`). `@TableFields('stores', { path:
//! 'data' })` pada list/detail/create/update; export menyensor sendiri (§6.3).

use std::sync::Arc;

use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use futures::future::BoxFuture;
use mz_audit::{AuditHandle, Lookup, Model, Row};
use mz_core::company::require_company_id;
use mz_core::extract::{ValidatedJson, ValidatedQuery};
use mz_core::time::{JsDate, now_naive};
use mz_core::{ApiError, ApiResult, AppState, RequestCtx};
use mz_sales_geo::dto::{Field, Raw};
use mz_sales_geo::plan::{PlanLimit, assert_plan_limit};
use mz_table_fields::apply::censor_response_body;
use mz_table_fields::policy::{CensoredColumns, policy_for_request};
use mz_table_fields::reveal::{RevealRequest, RevealResolver, RevealTarget};
use mz_table_fields::search_sort::{allowed_search_branches, assert_sortable_column};
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, QueryBuilder};

use super::common::{
    STORE_SELECT, StoreRow, TABLE_KEY, generate_qr_token, is_qr_token_conflict, prisma_uuid,
    resolve_active_customer_id, resolve_active_group_id, store_by_id, store_not_found,
};
use super::dto::{
    BulkInput, ListStoresQuery, StoreInput, parse_bulk, parse_create_store, parse_list_stores,
    parse_update_store,
};
use super::export::{BATCH_SIZE, build_stores_xlsx, columns_for, csv_header, csv_row};
use crate::enums::{AdminStoreSortField, ExportFormat, SortOrder};
use crate::export::{
    CSV_EXPORT_MAX_ROWS, XLSX_EXPORT_MAX_ROWS, consume_export_quota, content_type_for,
};
use crate::scope::{StoreScope, push_scope_clause, resolve_store_scope, users_in_scope_for};

// ───────────────────────── pembantu ─────────────────────────

/// `sortColumnKey(sort)`: nilai `sort` → kunci kolom registry.
pub fn sort_column_key(sort: AdminStoreSortField) -> &'static str {
    match sort {
        AdminStoreSortField::code => "code",
        AdminStoreSortField::createdAt => "createdAt",
        AdminStoreSortField::lastVisitAt => "lastVisitAt",
        AdminStoreSortField::name => "name",
    }
}

fn sort_sql(sort: AdminStoreSortField, order: SortOrder) -> String {
    let col = match sort {
        AdminStoreSortField::code => "code",
        AdminStoreSortField::createdAt => r#""createdAt""#,
        AdminStoreSortField::lastVisitAt => r#""lastVisitAt""#,
        AdminStoreSortField::name => "name",
    };
    let dir = match order {
        SortOrder::asc => "ASC",
        SortOrder::desc => "DESC",
    };
    format!(" ORDER BY s.{col} {dir}")
}

fn internal(msg: &str) -> ApiError {
    ApiError::internal(anyhow::anyhow!("{msg}"))
}

/// §1.6 `getTimeZone(companyId)`.
pub async fn company_time_zone(db: &PgPool, company_id: i32) -> ApiResult<String> {
    let tz: Option<String> = sqlx::query_scalar(r#"SELECT "timeZone" FROM company WHERE id = $1"#)
        .bind(company_id)
        .fetch_optional(db)
        .await?;
    let tz = tz.ok_or_else(|| ApiError::forbidden("No company found for user"))?;
    Ok(if tz.is_empty() { "UTC".to_owned() } else { tz })
}

/// `salesScopeClause(companyId, salesUuid)`: wilayah sales yang difilter (tri-state diteruskan).
/// 400 `SALES_NOT_IN_COMPANY` bila bukan anggota aktif.
pub async fn sales_scope(db: &PgPool, company_id: i32, sales_uuid: &str) -> ApiResult<StoreScope> {
    let member: Option<i32> = sqlx::query_scalar(
        r#"SELECT uc."userId" FROM "userCompany" uc JOIN users u ON u.id = uc."userId"
            WHERE uc."companyId" = $1 AND uc.status = 1 AND u.uuid = $2 LIMIT 1"#,
    )
    .bind(company_id)
    .bind(prisma_uuid(sales_uuid)?)
    .fetch_optional(db)
    .await?;
    let Some(user_id) = member else {
        return Err(ApiError::body(
            StatusCode::BAD_REQUEST,
            json!({
                "code": "SALES_NOT_IN_COMPANY",
                "message": "The filtered user is not an active employee of this company",
                "details": { "salesId": sales_uuid },
            }),
        ));
    };
    resolve_store_scope(db, user_id, company_id, None).await
}

/// `buildWhere(companyId, query, salesScope, censored)` sebagai SQL (alias `s`), mulai dengan ` WHERE`.
pub fn push_admin_where(
    qb: &mut QueryBuilder<'_, Postgres>,
    company_id: i32,
    q: &ListStoresQuery,
    sales_scope: &StoreScope,
    censored: &CensoredColumns,
) {
    qb.push(r#" WHERE s."companyId" = "#)
        .push_bind(company_id)
        .push(r#" AND s."deletedAt" IS NULL"#);
    if let Some(search) = q.search.as_deref().filter(|s| !s.is_empty()) {
        let cols = allowed_search_branches(
            [
                ("name", "name"),
                ("code", "code"),
                ("ownerName", "\"ownerName\""),
                ("address", "address"),
            ],
            censored,
        );
        if cols.is_empty() {
            qb.push(" AND FALSE");
        } else {
            qb.push(" AND (");
            for (i, col) in cols.iter().enumerate() {
                if i > 0 {
                    qb.push(" OR ");
                }
                // Prisma `contains` + insensitive = ILIKE tanpa escape `%`/`_` (sama dengan Node).
                qb.push(format!("s.{col} ILIKE '%' || "))
                    .push_bind(search.to_owned())
                    .push(" || '%'");
            }
            qb.push(")");
        }
    }
    if let Some(active) = q.is_active {
        qb.push(r#" AND s."isActive" = "#).push_bind(active);
    }
    let in_groups = |qb: &mut QueryBuilder<'_, Postgres>, ids: &Vec<String>| {
        qb.push(r#"s."groupId" IN (SELECT id FROM "storeGroup" WHERE uuid = ANY("#)
            .push_bind(ids.clone())
            .push("::uuid[]))");
    };
    let groups = q.group_id.as_ref().filter(|g| !g.is_empty());
    match (groups, q.unassigned == Some(true)) {
        (Some(ids), true) => {
            qb.push(" AND (");
            in_groups(qb, ids);
            qb.push(r#" OR s."groupId" IS NULL)"#);
        }
        (Some(ids), false) => {
            qb.push(" AND ");
            in_groups(qb, ids);
        }
        (None, true) => {
            qb.push(r#" AND s."groupId" IS NULL"#);
        }
        (None, false) => {}
    }
    let in_customers = |qb: &mut QueryBuilder<'_, Postgres>, ids: &Vec<String>| {
        qb.push(r#"s."customerId" IN (SELECT id FROM customer WHERE uuid = ANY("#)
            .push_bind(ids.clone())
            .push("::uuid[]))");
    };
    let customers = q.customer_id.as_ref().filter(|g| !g.is_empty());
    match (customers, q.unassigned_customer == Some(true)) {
        (Some(ids), true) => {
            qb.push(" AND (");
            in_customers(qb, ids);
            qb.push(r#" OR s."customerId" IS NULL)"#);
        }
        (Some(ids), false) => {
            qb.push(" AND ");
            in_customers(qb, ids);
        }
        (None, true) => {
            qb.push(r#" AND s."customerId" IS NULL"#);
        }
        (None, false) => {}
    }
    push_scope_clause(qb, sales_scope, r#"s."groupId""#);
}

async fn count_where(
    db: &PgPool,
    company_id: i32,
    q: &ListStoresQuery,
    scope: &StoreScope,
    censored: &CensoredColumns,
) -> ApiResult<i64> {
    let mut qb = QueryBuilder::new("SELECT COUNT(*) FROM store s");
    push_admin_where(&mut qb, company_id, q, scope, censored);
    Ok(qb.build_query_scalar::<i64>().fetch_one(db).await?)
}

async fn find_live(db: &PgPool, company_id: i32, uuid: &str) -> ApiResult<Option<StoreRow>> {
    Ok(sqlx::query_as(&format!(
        r#"{STORE_SELECT} WHERE s.uuid = $1 AND s."companyId" = $2 AND s."deletedAt" IS NULL LIMIT 1"#
    ))
    .bind(prisma_uuid(uuid)?)
    .bind(company_id)
    .fetch_optional(db)
    .await?)
}

// ───────────────────────── service ─────────────────────────

/// `findAll(userId, query, censored)` → `{ data, summary: { totalStores, activeStores }, meta }`.
pub async fn find_all(
    state: &AppState,
    user_id: i32,
    q: &ListStoresQuery,
    censored: &CensoredColumns,
) -> ApiResult<Value> {
    let db = &state.db;
    let company_id = require_company_id(db, user_id).await?;
    let scope = match q.sales_id.as_deref() {
        Some(s) => sales_scope(db, company_id, s).await?,
        None => None,
    };
    assert_sortable_column(TABLE_KEY, Some(sort_column_key(q.sort)), censored)?;
    let (page, limit) = (q.page, q.limit);

    let rows_fut = async {
        let mut qb = QueryBuilder::new(STORE_SELECT);
        push_admin_where(&mut qb, company_id, q, &scope, censored);
        qb.push(sort_sql(q.sort, q.order));
        qb.push(" OFFSET ").push_bind((page - 1) * limit);
        qb.push(" LIMIT ").push_bind(limit);
        Ok::<_, ApiError>(qb.build_query_as::<StoreRow>().fetch_all(db).await?)
    };
    let summary_fut = async {
        Ok::<(i64, i64), ApiError>(
            sqlx::query_as(
                r#"SELECT COUNT(*), COUNT(*) FILTER (WHERE "isActive")
                     FROM store WHERE "companyId" = $1 AND "deletedAt" IS NULL"#,
            )
            .bind(company_id)
            .fetch_one(db)
            .await?,
        )
    };
    let (rows, total, (total_stores, active_stores)) = tokio::try_join!(
        rows_fut,
        count_where(db, company_id, q, &scope, censored),
        summary_fut
    )?;
    Ok(json!({
        "data": rows.iter().map(StoreRow::to_admin).collect::<Vec<_>>(),
        "summary": { "totalStores": total_stores, "activeStores": active_stores },
        "meta": { "total": total, "page": page, "limit": limit, "totalPages": total_pages(total, limit) },
    }))
}

/// `Math.ceil(total / limit)`.
pub fn total_pages(total: i64, limit: i64) -> i64 {
    if limit <= 0 {
        0
    } else {
        (total + limit - 1) / limit
    }
}

/// `findOne(userId, id)` → `toAdminStoreDetail`.
pub async fn find_one(state: &AppState, user_id: i32, id: &str) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let row = find_live(&state.db, company_id, id)
        .await?
        .ok_or_else(store_not_found)?;
    Ok(row.to_admin_detail())
}

/// `findForReveal(companyId, actorUserId, id, actorRoleCode)` (§7.3): wilayah menyaring HANYA
/// untuk peringkat di bawah MANAGER; role tak dikenal = MANAGER ke atas. 404 `STORE_NOT_FOUND`.
pub async fn find_for_reveal(
    state: &AppState,
    company_id: i32,
    actor_user_id: i32,
    id: &str,
    actor_role_code: Option<&str>,
) -> ApiResult<RevealTarget> {
    let rank = actor_role_code.and_then(mz_core::enums::rank_of);
    let region_scoped = rank.is_some_and(|r| r < 2);
    let scope = if region_scoped {
        resolve_store_scope(&state.db, actor_user_id, company_id, None).await?
    } else {
        None
    };
    let mut qb = QueryBuilder::new(STORE_SELECT);
    qb.push(" WHERE s.uuid = ")
        .push_bind(prisma_uuid(id)?)
        .push(r#" AND s."companyId" = "#)
        .push_bind(company_id)
        .push(r#" AND s."deletedAt" IS NULL"#);
    push_scope_clause(&mut qb, &scope, r#"s."groupId""#);
    qb.push(" LIMIT 1");
    let row = qb
        .build_query_as::<StoreRow>()
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(store_not_found)?;
    Ok(RevealTarget {
        record_id: row.id,
        record_uuid: Some(row.uuid.clone()),
        record_label: Some(format!("{} — {}", row.code, row.name)),
        record: row.to_admin(),
    })
}

/// `StoresRevealResolver` (didaftarkan `stores::routes`).
pub struct StoresRevealResolver;

impl RevealResolver for StoresRevealResolver {
    fn resolve<'a>(
        &'a self,
        state: &'a AppState,
        request: RevealRequest,
    ) -> BoxFuture<'a, Result<RevealTarget, ApiError>> {
        Box::pin(async move {
            find_for_reveal(
                state,
                request.company_id,
                request.actor_user_id,
                &request.record_uuid,
                request.actor_role_code.as_deref(),
            )
            .await
        })
    }
}

/// `findEligibleSales(userId, id)` → `{ data: [{ id, name, username }] }` (urut `name`).
pub async fn find_eligible_sales(state: &AppState, user_id: i32, id: &str) -> ApiResult<Value> {
    let db = &state.db;
    let company_id = require_company_id(db, user_id).await?;
    let group_id: Option<Option<i32>> = sqlx::query_scalar(
        r#"SELECT "groupId" FROM store WHERE uuid = $1 AND "companyId" = $2 AND "deletedAt" IS NULL LIMIT 1"#,
    )
    .bind(prisma_uuid(id)?)
    .bind(company_id)
    .fetch_optional(db)
    .await?;
    let group_id = group_id.ok_or_else(store_not_found)?;
    let members: Vec<i32> = sqlx::query_scalar(
        r#"SELECT DISTINCT "userId" FROM "userCompany" WHERE "companyId" = $1 AND status = 1"#,
    )
    .bind(company_id)
    .fetch_all(db)
    .await?;
    let allowed = users_in_scope_for(db, company_id, group_id, &members).await?;
    if allowed.is_empty() {
        return Ok(json!({ "data": [] }));
    }
    let ids: Vec<i32> = allowed.into_iter().collect();
    let users: Vec<(String, String, String)> = sqlx::query_as(
        r#"SELECT uuid::text, name, username FROM users WHERE id = ANY($1) ORDER BY name ASC"#,
    )
    .bind(&ids)
    .fetch_all(db)
    .await?;
    Ok(json!({
        "data": users.into_iter().map(|(id, name, username)| json!({ "id": id, "name": name, "username": username })).collect::<Vec<_>>(),
    }))
}

fn code_exists(code: &str) -> ApiError {
    ApiError::coded(
        StatusCode::CONFLICT,
        "STORE_CODE_EXISTS",
        format!("Store code {code} is already used in this company"),
    )
}

/// `create(userId, dto)` → `toAdminStoreDetail`. Batas `maxStores` di transaksi yang sama;
/// tabrakan `qrToken` diulang (3 percobaan).
pub async fn create(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    dto: &StoreInput,
) -> ApiResult<Value> {
    let db = &state.db;
    let company_id = require_company_id(db, user_id).await?;
    let (
        Field::Val(code),
        Field::Val(name),
        Field::Val(owner_name),
        Field::Val(address),
        Field::Val(lat),
        Field::Val(lng),
    ) = (
        &dto.code,
        &dto.name,
        &dto.owner_name,
        &dto.address,
        &dto.lat,
        &dto.lng,
    )
    else {
        return Err(internal("CreateStoreDto incomplete"));
    };
    let code = code.to_uppercase();
    // `groupId: null` di create → Prisma menolak `uuid: null` → 500.
    let group_id = match &dto.group_id {
        Field::Val(g) => Some(resolve_active_group_id(db, company_id, g).await?),
        Field::Null => return Err(internal("Argument `uuid` must not be null.")),
        Field::Absent => None,
    };
    let customer_id = match &dto.customer_id {
        Field::Val(c) => Some(resolve_active_customer_id(db, company_id, c).await?),
        Field::Null => return Err(internal("Argument `uuid` must not be null.")),
        Field::Absent => None,
    };
    let phone = match &dto.phone {
        Field::Val(p) => mz_customers::phone::normalize_store_phone(Some(p)),
        _ => None,
    };
    let radius = match dto.radius_meters {
        Field::Val(r) => i32::try_from(r).ok(),
        _ => None,
    };
    let is_active = match dto.is_active {
        Field::Val(b) => b,
        _ => true,
    };
    let mut attempt = 0;
    loop {
        let result: Result<i32, CreateErr> = async {
            let mut tx = db.begin().await?;
            assert_plan_limit(&mut tx, company_id, PlanLimit::Stores).await?;
            let id: i32 = sqlx::query_scalar(
                r#"INSERT INTO store ("companyId", code, name, "ownerName", phone, address, lat, lng,
                                      "radiusMeters", "qrToken", "isActive", "createdById", "groupId", "customerId")
                   VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14) RETURNING id"#,
            )
            .bind(company_id)
            .bind(&code)
            .bind(name)
            .bind(owner_name)
            .bind(&phone)
            .bind(address)
            .bind(lat)
            .bind(lng)
            .bind(radius)
            .bind(generate_qr_token())
            .bind(is_active)
            .bind(user_id)
            .bind(group_id)
            .bind(customer_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| map_create_error(e, &code))?;
            if let Some(r) = audit.result_row(&mut *tx, Model::Store, Lookup::Id(id)).await {
                audit.record_create(Model::Store, &r).await;
            }
            tx.commit().await?;
            Ok(id)
        }
        .await;
        match result {
            Ok(id) => return Ok(store_by_id(db, id).await?.to_admin_detail()),
            Err(CreateErr::QrCollision) if attempt < 2 => attempt += 1,
            Err(CreateErr::QrCollision) => {
                return Err(ApiError::coded(
                    StatusCode::CONFLICT,
                    "STORE_QR_TOKEN_COLLISION",
                    "Could not allocate a unique QR token; please retry",
                ));
            }
            Err(CreateErr::Api(e)) => return Err(e),
        }
    }
}

/// Galat percobaan insert (`qrToken` bentrok diulang, sisanya diteruskan).
pub(crate) enum CreateErr {
    QrCollision,
    Api(ApiError),
}

impl From<ApiError> for CreateErr {
    fn from(e: ApiError) -> Self {
        Self::Api(e)
    }
}

impl From<sqlx::Error> for CreateErr {
    fn from(e: sqlx::Error) -> Self {
        Self::Api(e.into())
    }
}

fn map_create_error(e: sqlx::Error, code: &str) -> CreateErr {
    if mz_core::db::is_unique_violation(&e) {
        if is_qr_token_conflict(&e) {
            return CreateErr::QrCollision;
        }
        return CreateErr::Api(code_exists(code));
    }
    CreateErr::Api(e.into())
}

/// Kolom non-nullable yang dikirim `null` lewat `PartialType`: Prisma menolak → 500.
fn not_null<T: Clone>(f: &Field<T>, col: &str) -> ApiResult<Option<T>> {
    match f {
        Field::Absent => Ok(None),
        Field::Null => Err(internal(&format!("Argument `{col}` must not be null."))),
        Field::Val(v) => Ok(Some(v.clone())),
    }
}

/// `update(userId, id, dto)` → `toAdminStoreDetail`.
pub async fn update(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    id: &str,
    dto: &StoreInput,
) -> ApiResult<Value> {
    let db = &state.db;
    let company_id = require_company_id(db, user_id).await?;
    let store = find_live(db, company_id, id)
        .await?
        .ok_or_else(store_not_found)?;
    let group_id: Option<Option<i32>> = match &dto.group_id {
        Field::Absent => None,
        Field::Null => Some(None),
        Field::Val(g) => Some(Some(resolve_active_group_id(db, company_id, g).await?)),
    };
    let customer_id: Option<Option<i32>> = match &dto.customer_id {
        Field::Absent => None,
        Field::Null => Some(None),
        Field::Val(c) => Some(Some(resolve_active_customer_id(db, company_id, c).await?)),
    };
    // `dto.code?.toUpperCase()`: `null` → `undefined` → kolom tidak disentuh.
    let code = match &dto.code {
        Field::Val(c) => Some(c.to_uppercase()),
        _ => None,
    };
    let name = not_null(&dto.name, "name")?;
    let owner_name = not_null(&dto.owner_name, "ownerName")?;
    let address = not_null(&dto.address, "address")?;
    let lat = not_null(&dto.lat, "lat")?;
    let lng = not_null(&dto.lng, "lng")?;
    let is_active = not_null(&dto.is_active, "isActive")?;

    let mut qb = QueryBuilder::<Postgres>::new("UPDATE store SET ");
    let mut sep = qb.separated(", ");
    if let Some(c) = &code {
        sep.push("code = ").push_bind_unseparated(c.clone());
    }
    if let Some(v) = name {
        sep.push("name = ").push_bind_unseparated(v);
    }
    if let Some(v) = owner_name {
        sep.push(r#""ownerName" = "#).push_bind_unseparated(v);
    }
    match &dto.phone {
        Field::Absent => {}
        Field::Null => {
            sep.push("phone = NULL");
        }
        Field::Val(p) => {
            sep.push("phone = ")
                .push_bind_unseparated(mz_customers::phone::normalize_store_phone(Some(p)));
        }
    }
    if let Some(v) = address {
        sep.push("address = ").push_bind_unseparated(v);
    }
    if let Some(v) = lat {
        sep.push("lat = ").push_bind_unseparated(v);
    }
    if let Some(v) = lng {
        sep.push("lng = ").push_bind_unseparated(v);
    }
    match dto.radius_meters {
        Field::Absent => {}
        Field::Null => {
            sep.push(r#""radiusMeters" = NULL"#);
        }
        Field::Val(r) => {
            sep.push(r#""radiusMeters" = "#)
                .push_bind_unseparated(i32::try_from(r).unwrap_or(i32::MAX));
        }
    }
    if let Some(v) = is_active {
        sep.push(r#""isActive" = "#).push_bind_unseparated(v);
    }
    if let Some(g) = group_id {
        sep.push(r#""groupId" = "#).push_bind_unseparated(g);
    }
    if let Some(c) = customer_id {
        sep.push(r#""customerId" = "#).push_bind_unseparated(c);
    }
    sep.push(r#""updatedAt" = "#)
        .push_bind_unseparated(now_naive());
    qb.push(" WHERE id = ").push_bind(store.id);

    let pre = audit.pre_read(Model::Store, Lookup::Id(store.id)).await;
    if let Err(e) = qb.build().execute(db).await {
        if mz_core::db::is_unique_violation(&e) {
            return Err(code_exists(code.as_deref().unwrap_or("undefined")));
        }
        return Err(e.into());
    }
    if let Some(r) = audit
        .result_row(db, Model::Store, Lookup::Id(store.id))
        .await
    {
        audit.record_update(Model::Store, pre, &r).await;
    }
    Ok(store_by_id(db, store.id).await?.to_admin_detail())
}

/// `remove(userId, id)` → `{ id, deleted: true }` (soft delete: `deletedAt`, `isActive = false`).
pub async fn remove(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    id: &str,
) -> ApiResult<Value> {
    let db = &state.db;
    let company_id = require_company_id(db, user_id).await?;
    let store = find_live(db, company_id, id)
        .await?
        .ok_or_else(store_not_found)?;
    let pre = audit.pre_read(Model::Store, Lookup::Id(store.id)).await;
    let now = now_naive();
    sqlx::query(
        r#"UPDATE store SET "deletedAt" = $2, "isActive" = false, "updatedAt" = $2 WHERE id = $1"#,
    )
    .bind(store.id)
    .bind(now)
    .execute(db)
    .await?;
    if let Some(r) = audit
        .result_row(db, Model::Store, Lookup::Id(store.id))
        .await
    {
        audit.record_update(Model::Store, pre, &r).await;
    }
    Ok(json!({ "id": store.uuid, "deleted": true }))
}

fn uuids(list: &[String]) -> ApiResult<Vec<uuid::Uuid>> {
    list.iter().map(|u| prisma_uuid(u)).collect()
}

/// `updateMany` toko hidup company ini (`uuid IN`), ber-audit. Kembalikan jumlah baris.
async fn bulk_update(
    exec: &mut sqlx::PgConnection,
    audit: &AuditHandle,
    company_id: i32,
    store_uuids: &[String],
    column: &'static str,
    value: Option<i32>,
) -> ApiResult<u64> {
    let ids = uuids(store_uuids)?;
    let lookup_ids = ids.clone();
    let pre = audit
        .pre_read_many(
            Model::Store,
            Lookup::filter(move |qb| {
                qb.push("uuid = ANY(")
                    .push_bind(lookup_ids.clone())
                    .push(r#") AND "companyId" = "#)
                    .push_bind(company_id)
                    .push(r#" AND "deletedAt" IS NULL"#);
            }),
        )
        .await;
    let now = now_naive();
    let n = sqlx::query(&format!(
        r#"UPDATE store SET "{column}" = $1, "updatedAt" = $2
            WHERE uuid = ANY($3) AND "companyId" = $4 AND "deletedAt" IS NULL"#
    ))
    .bind(value)
    .bind(now)
    .bind(&ids)
    .bind(company_id)
    .execute(&mut *exec)
    .await?
    .rows_affected();
    audit
        .record_update_many(
            Model::Store,
            pre,
            n,
            &Row::new()
                .with(column, json!(value))
                .with("updatedAt", json!(JsDate(now))),
            json!({ "uuid": { "in": store_uuids }, "companyId": company_id, "deletedAt": null }),
        )
        .await;
    Ok(n)
}

/// `bulkGroup(userId, dto)` → `{ updated }`.
pub async fn bulk_group(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    dto: &BulkInput,
) -> ApiResult<Value> {
    let db = &state.db;
    let company_id = require_company_id(db, user_id).await?;
    let group_id = match &dto.target {
        None => None,
        Some(g) => Some(resolve_active_group_id(db, company_id, g).await?),
    };
    let mut conn = db.acquire().await?;
    let n = bulk_update(
        &mut conn,
        audit,
        company_id,
        &dto.store_uuids,
        "groupId",
        group_id,
    )
    .await?;
    Ok(json!({ "updated": n }))
}

/// `bulkCustomer(userId, dto)` → `{ updated, notFound }` (cocok & tulis dalam SATU transaksi).
pub async fn bulk_customer(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    dto: &BulkInput,
) -> ApiResult<Value> {
    let db = &state.db;
    let company_id = require_company_id(db, user_id).await?;
    let customer_id = match &dto.target {
        None => None,
        Some(c) => Some(resolve_active_customer_id(db, company_id, c).await?),
    };
    let ids = uuids(&dto.store_uuids)?;
    let mut tx = db.begin().await?;
    let matched: Vec<String> = sqlx::query_scalar(
        r#"SELECT uuid::text FROM store WHERE uuid = ANY($1) AND "companyId" = $2 AND "deletedAt" IS NULL"#,
    )
    .bind(&ids)
    .bind(company_id)
    .fetch_all(&mut *tx)
    .await?;
    let updated = bulk_update(
        &mut tx,
        audit,
        company_id,
        &dto.store_uuids,
        "customerId",
        customer_id,
    )
    .await?;
    tx.commit().await?;
    Ok(json!({ "updated": updated, "notFound": not_found_uuids(&dto.store_uuids, &matched) }))
}

/// uuid yang tidak cocok, urutan permintaan dipertahankan.
pub fn not_found_uuids(requested: &[String], matched: &[String]) -> Vec<String> {
    requested
        .iter()
        .filter(|u| !matched.iter().any(|m| m.eq_ignore_ascii_case(u)))
        .cloned()
        .collect()
}

/// Galat batas baris export (dicek SEBELUM kuota terpakai).
pub fn export_row_limit_error(format: ExportFormat, total: i64) -> Option<ApiError> {
    if format == ExportFormat::xlsx && total > XLSX_EXPORT_MAX_ROWS {
        return Some(ApiError::coded(
            StatusCode::BAD_REQUEST,
            "EXPORT_TOO_MANY_ROWS",
            format!(
                "xlsx export is limited to {XLSX_EXPORT_MAX_ROWS} rows (matched {total}); narrow the filters or use format=csv"
            ),
        ));
    }
    if total > CSV_EXPORT_MAX_ROWS {
        return Some(ApiError::coded(
            StatusCode::BAD_REQUEST,
            "EXPORT_TOO_MANY_ROWS",
            format!(
                "Export is limited to {CSV_EXPORT_MAX_ROWS} rows (matched {total}); narrow the filters"
            ),
        ));
    }
    None
}

/// `exportStores(userId, query, censored)` → respons file (csv di-stream per 500 baris, xlsx utuh).
pub async fn export_stores(
    state: &AppState,
    user_id: i32,
    q: ListStoresQuery,
    censored: Arc<CensoredColumns>,
) -> ApiResult<Response> {
    let db = &state.db;
    let company_id = require_company_id(db, user_id).await?;
    let tz = company_time_zone(db, company_id).await?;
    let scope = match q.sales_id.as_deref() {
        Some(s) => sales_scope(db, company_id, s).await?,
        None => None,
    };
    let total = count_where(db, company_id, &q, &scope, &censored).await?;
    if let Some(e) = export_row_limit_error(q.format, total) {
        return Err(e);
    }
    consume_export_quota(db, company_id).await?;

    if q.format == ExportFormat::xlsx {
        let mut qb = QueryBuilder::new(STORE_SELECT);
        push_admin_where(&mut qb, company_id, &q, &scope, &censored);
        qb.push(" ORDER BY s.id ASC");
        let rows = qb.build_query_as::<StoreRow>().fetch_all(db).await?;
        let buf = build_stores_xlsx(&rows, &tz, &censored).map_err(ApiError::internal)?;
        let len = buf.len();
        let mut resp = crate::export::file_response(
            content_type_for(ExportFormat::xlsx),
            "stores-export.xlsx",
            buf,
        );
        if let Ok(v) = HeaderValue::from_str(&len.to_string()) {
            resp.headers_mut().insert(header::CONTENT_LENGTH, v);
        }
        return Ok(resp);
    }

    // §14.1(4): satu batch `BATCH_SIZE` di memori, kursor `id`.
    struct Cursor {
        state: AppState,
        company_id: i32,
        q: ListStoresQuery,
        scope: StoreScope,
        censored: Arc<CensoredColumns>,
        tz: String,
        last_id: Option<i32>,
        header_sent: bool,
        done: bool,
    }
    let cursor = Cursor {
        state: state.clone(),
        company_id,
        q,
        scope,
        censored,
        tz,
        last_id: None,
        header_sent: false,
        done: false,
    };
    let stream = futures::stream::unfold(cursor, |mut c| async move {
        if c.done {
            return None;
        }
        let columns = columns_for(&c.censored);
        if !c.header_sent {
            c.header_sent = true;
            return Some((
                Ok::<Bytes, std::io::Error>(Bytes::from(csv_header(&columns))),
                c,
            ));
        }
        let mut qb = QueryBuilder::new(STORE_SELECT);
        push_admin_where(&mut qb, c.company_id, &c.q, &c.scope, &c.censored);
        if let Some(last) = c.last_id {
            qb.push(" AND s.id > ").push_bind(last);
        }
        qb.push(" ORDER BY s.id ASC LIMIT ").push_bind(BATCH_SIZE);
        match qb.build_query_as::<StoreRow>().fetch_all(&c.state.db).await {
            Err(e) => {
                c.done = true;
                Some((Err(std::io::Error::other(e)), c))
            }
            Ok(rows) => {
                c.last_id = Some(rows.last()?.id);
                if i64::try_from(rows.len()).unwrap_or(0) < BATCH_SIZE {
                    c.done = true;
                }
                let mut chunk = String::new();
                for r in &rows {
                    chunk.push_str(&csv_row(&columns, r, &c.tz, &c.censored));
                }
                Some((Ok(Bytes::from(chunk)), c))
            }
        }
    });
    // `.fuse()`: CompressionLayer bisa mem-poll body lagi sesudah stream selesai; `unfold` panic bila itu terjadi.
    let mut resp = Response::new(Body::from_stream(futures::StreamExt::fuse(stream)));
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/csv; charset=utf-8"),
    );
    h.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_static("attachment; filename=\"stores-export.csv\""),
    );
    Ok(resp)
}

// ───────────────────────── handler ─────────────────────────

/// `policies.censoredColumns(req, 'stores')`.
pub async fn censored_for(state: &AppState, ctx: &RequestCtx) -> ApiResult<Arc<CensoredColumns>> {
    Ok(policy_for_request(state, ctx, false)
        .await?
        .for_table(TABLE_KEY))
}

/// Interceptor `@TableFields('stores', { path: 'data' })`.
pub fn censor(mut body: Value, censored: &CensoredColumns) -> Json<Value> {
    censor_response_body(&mut body, TABLE_KEY, "data", censored, None);
    Json(body)
}

/// Node: src/store-visits/admin-stores.controller.ts:195 `findAll`
pub async fn h_find_all(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedQuery(q): ValidatedQuery<Raw>,
) -> ApiResult<Json<Value>> {
    let q = parse_list_stores(&q.0, false).map_err(ApiError::Validation)?;
    let censored = censored_for(&state, &ctx).await?;
    let body = find_all(&state, ctx.require_user_id()?, &q, &censored).await?;
    Ok(censor(body, &censored))
}

/// Node: src/store-visits/admin-stores.controller.ts:251 `export`
pub async fn h_export(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedQuery(q): ValidatedQuery<Raw>,
) -> ApiResult<Response> {
    let q = parse_list_stores(&q.0, true).map_err(ApiError::Validation)?;
    let censored = censored_for(&state, &ctx).await?;
    export_stores(&state, ctx.require_user_id()?, q, censored).await
}

/// Node: src/store-visits/admin-stores.controller.ts:321 `findOne`
pub async fn h_find_one(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let data = find_one(&state, ctx.require_user_id()?, &id).await?;
    let censored = censored_for(&state, &ctx).await?;
    Ok(censor(json!({ "data": data }), &censored))
}

/// Node: src/store-visits/admin-stores.controller.ts:354 `findEligibleSales`
pub async fn h_eligible_sales(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    Ok(Json(
        find_eligible_sales(&state, ctx.require_user_id()?, &id).await?,
    ))
}

/// Node: src/store-visits/admin-stores.controller.ts:396 `create` (201)
pub async fn h_create(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    ValidatedJson(body): ValidatedJson<Raw>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let dto = parse_create_store(&body.0).map_err(ApiError::Validation)?;
    let data = create(&state, &audit, ctx.require_user_id()?, &dto).await?;
    let censored = censored_for(&state, &ctx).await?;
    Ok((
        StatusCode::CREATED,
        censor(json!({ "data": data }), &censored),
    ))
}

/// Node: src/store-visits/admin-stores.controller.ts:472 `bulkGroup`
pub async fn h_bulk_group(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    ValidatedJson(body): ValidatedJson<Raw>,
) -> ApiResult<Json<Value>> {
    let dto = parse_bulk(&body.0, "storeUuids", "groupId").map_err(ApiError::Validation)?;
    let data = bulk_group(&state, &audit, ctx.require_user_id()?, &dto).await?;
    Ok(Json(json!({ "data": data })))
}

/// Node: src/store-visits/admin-stores.controller.ts:516 `bulkCustomer`
pub async fn h_bulk_customer(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    ValidatedJson(body): ValidatedJson<Raw>,
) -> ApiResult<Json<Value>> {
    let dto = parse_bulk(&body.0, "storeIds", "customerId").map_err(ApiError::Validation)?;
    let data = bulk_customer(&state, &audit, ctx.require_user_id()?, &dto).await?;
    Ok(Json(json!({ "data": data })))
}

/// Node: src/store-visits/admin-stores.controller.ts:573 `update`
pub async fn h_update(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    Path(id): Path<String>,
    ValidatedJson(body): ValidatedJson<Raw>,
) -> ApiResult<Json<Value>> {
    let dto = parse_update_store(&body.0).map_err(ApiError::Validation)?;
    let data = update(&state, &audit, ctx.require_user_id()?, &id, &dto).await?;
    let censored = censored_for(&state, &ctx).await?;
    Ok(censor(json!({ "data": data }), &censored))
}

/// Node: src/store-visits/admin-stores.controller.ts:671 `remove`
pub async fn h_remove(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let data = remove(&state, &audit, ctx.require_user_id()?, &id).await?;
    Ok(Json(json!({ "data": data })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mz_table_fields::registry::FieldVisibilityMode;

    fn q() -> ListStoresQuery {
        parse_list_stores(&serde_json::Map::new(), false).unwrap()
    }

    fn sql(q: &ListStoresQuery, scope: &StoreScope, censored: &CensoredColumns) -> String {
        let mut qb = QueryBuilder::<Postgres>::new("SELECT 1 FROM store s");
        push_admin_where(&mut qb, 1, q, scope, censored);
        qb.sql().to_owned()
    }

    #[test]
    fn where_scopes_company_and_live_rows() {
        let s = sql(&q(), &None, &CensoredColumns::new());
        assert_eq!(
            s,
            r#"SELECT 1 FROM store s WHERE s."companyId" = $1 AND s."deletedAt" IS NULL"#
        );
    }

    #[test]
    fn search_drops_censored_branches() {
        let mut query = q();
        query.search = Some("x".into());
        let s = sql(&query, &None, &CensoredColumns::new());
        for c in [
            "s.name ILIKE",
            "s.code ILIKE",
            r#"s."ownerName" ILIKE"#,
            "s.address ILIKE",
        ] {
            assert!(s.contains(c), "{c}");
        }
        let censored: CensoredColumns =
            [("ownerName".to_owned(), FieldVisibilityMode::Hidden)].into();
        let s = sql(&query, &None, &censored);
        assert!(!s.contains("ownerName") && s.contains("s.address ILIKE"));
        // Kolom tersensor yang bukan kolom search tidak mengubah klausa.
        let censored: CensoredColumns = [("phone".to_owned(), FieldVisibilityMode::Masked)].into();
        assert!(sql(&query, &None, &censored).contains(r#"s."ownerName" ILIKE"#));
    }

    #[test]
    fn group_and_customer_filters_are_unions_and_independent() {
        let mut query = q();
        query.group_id = Some(vec!["a".into()]);
        query.unassigned = Some(true);
        query.unassigned_customer = Some(true);
        let s = sql(&query, &None, &CensoredColumns::new());
        assert!(s.contains(r#"AND (s."groupId" IN (SELECT id FROM "storeGroup" WHERE uuid = ANY($2::uuid[])) OR s."groupId" IS NULL)"#));
        assert!(s.ends_with(r#" AND s."customerId" IS NULL"#));
        let mut query = q();
        query.unassigned = Some(true);
        assert!(
            sql(&query, &None, &CensoredColumns::new()).ends_with(r#" AND s."groupId" IS NULL"#)
        );
    }

    #[test]
    fn sales_scope_narrows_on_top() {
        let scope = Some(crate::scope::Scope {
            group_ids: vec![4],
            strict: false,
        });
        let mut query = q();
        query.group_id = Some(vec!["a".into()]);
        let s = sql(&query, &scope, &CensoredColumns::new());
        assert!(s.ends_with(r#" AND (s."groupId" = ANY($3) OR s."groupId" IS NULL)"#));
        let strict = Some(crate::scope::Scope {
            group_ids: vec![],
            strict: true,
        });
        assert!(
            sql(&q(), &strict, &CensoredColumns::new())
                .ends_with(r#" AND (s."groupId" = ANY($2))"#)
        );
    }

    #[test]
    fn sort_keys_and_limits() {
        assert_eq!(
            sort_column_key(AdminStoreSortField::lastVisitAt),
            "lastVisitAt"
        );
        assert_eq!(
            sort_sql(AdminStoreSortField::createdAt, SortOrder::desc),
            r#" ORDER BY s."createdAt" DESC"#
        );
        let censored: CensoredColumns =
            [("lastVisitAt".to_owned(), FieldVisibilityMode::Hidden)].into();
        assert!(assert_sortable_column(TABLE_KEY, Some("lastVisitAt"), &censored).is_err());
        assert!(assert_sortable_column(TABLE_KEY, Some("name"), &censored).is_ok());
        assert!(export_row_limit_error(ExportFormat::xlsx, 5000).is_none());
        assert!(export_row_limit_error(ExportFormat::xlsx, 5001).is_some());
        assert!(export_row_limit_error(ExportFormat::csv, 50_000).is_none());
        assert!(export_row_limit_error(ExportFormat::csv, 50_001).is_some());
        assert_eq!(total_pages(41, 20), 3);
        assert_eq!(total_pages(0, 20), 0);
    }

    #[test]
    fn not_found_keeps_request_order() {
        let req = vec!["b".to_owned(), "a".to_owned(), "c".to_owned()];
        assert_eq!(not_found_uuids(&req, &["a".to_owned()]), vec!["b", "c"]);
    }
}
