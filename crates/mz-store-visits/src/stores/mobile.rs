//! Port `src/store-visits/mobile-stores.service.ts` + `mobile-stores.controller.ts` (4 route
//! `/mobile/stores`). Pembatasan wilayah lewat [`crate::scope`]; jarak lewat `geolib` port.

use std::cmp::Ordering;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use mz_audit::{AuditHandle, Lookup, Model};
use mz_core::company::require_company_id;
use mz_core::extract::{ValidatedJson, ValidatedQuery};
use mz_core::jsfmt::{collate_id, num_string};
use mz_core::time::JsDate;
use mz_core::{ApiError, ApiResult, AppState, RequestCtx};
use mz_sales_geo::dto::{Field, Raw};
use mz_sales_geo::plan::{PlanLimit, assert_plan_limit};
use mz_table_fields::policy::CensoredColumns;
use mz_table_fields::search_sort::{allowed_search_branches, assert_sortable_column};
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, QueryBuilder};

use super::admin::{censor, censored_for, total_pages};
use super::common::{
    STORE_SELECT, StoreRow, TABLE_KEY, find_group, generate_qr_token, group_inactive,
    group_not_found, is_qr_token_conflict, prisma_uuid, store_by_id, store_not_found,
};
use super::dto::{
    CreateMobileStore, ListMobileStoresQuery, ResolveStoreQr, parse_create_mobile_store,
    parse_list_mobile_stores, parse_resolve_qr,
};
use crate::enums::MobileStoreSortField;
use crate::geo_util::{DEFAULT_RADIUS_KM, MAX_PREFILTER_ROWS, bounding_box, get_distance};
use crate::scope::{
    ScopeDenial, ScopePosition, ScopeStore, StoreScope, assert_in_scope, push_scope_clause,
    resolve_store_scope,
};

/// Awalan kode toko buatan mobile.
pub const CODE_PREFIX: &str = "SLS";
/// Alfabet tanpa 0/O/1/I.
pub const CODE_ALPHABET: &str = "23456789ABCDEFGHJKMNPQRSTUVWXYZ";
const CODE_SUFFIX_LENGTH: usize = 5;
/// Percobaan insert saat kode/qrToken undian bentrok.
pub const CREATE_MAX_ATTEMPTS: u32 = 5;
/// Nama sama DAN dalam radius ini = toko yang sama.
pub const DUPLICATE_RADIUS_METERS: f64 = 50.0;

/// `generateStoreCode()`: `SLS-XXXXX`.
pub fn generate_store_code() -> String {
    use rand::Rng;
    let alphabet = CODE_ALPHABET.as_bytes();
    let mut rng = rand::rng();
    let suffix: String = (0..CODE_SUFFIX_LENGTH)
        .filter_map(|_| {
            alphabet
                .get(rng.random_range(0..alphabet.len()))
                .map(|b| char::from(*b))
        })
        .collect();
    format!("{CODE_PREFIX}-{suffix}")
}

/// `sortColumnKey(sort)` mobile: `lastVisit` → `lastVisitAt`, `distance` → tanpa kolom.
pub fn sort_column_key(sort: MobileStoreSortField) -> Option<&'static str> {
    match sort {
        MobileStoreSortField::code => Some("code"),
        MobileStoreSortField::lastVisit => Some("lastVisitAt"),
        MobileStoreSortField::distance => None,
        MobileStoreSortField::name => Some("name"),
    }
}

fn order_sql(sort: MobileStoreSortField) -> &'static str {
    match sort {
        MobileStoreSortField::code => " ORDER BY s.code ASC",
        MobileStoreSortField::lastVisit => r#" ORDER BY s."lastVisitAt" DESC NULLS LAST"#,
        MobileStoreSortField::name | MobileStoreSortField::distance => " ORDER BY s.name ASC",
    }
}

/// `compareInMemory(sort)` atas `(row, distanceMeters)`.
pub fn compare_in_memory(
    sort: MobileStoreSortField,
    a: &(StoreRow, i64),
    b: &(StoreRow, i64),
) -> Ordering {
    match sort {
        MobileStoreSortField::distance => a.1.cmp(&b.1),
        MobileStoreSortField::code => collate_id(&a.0.code, &b.0.code),
        MobileStoreSortField::lastVisit => match (a.0.last_visit_at, b.0.last_visit_at) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Greater,
            (Some(_), None) => Ordering::Less,
            (Some(x), Some(y)) => y.cmp(&x),
        },
        MobileStoreSortField::name => collate_id(&a.0.name, &b.0.name),
    }
}

fn coords_together() -> ApiError {
    ApiError::bad_request("lat and lng must be sent together, or not at all")
}

/// `resolveGroupFilter`: uuid tak dikenal di company → 404 `STORE_GROUP_NOT_FOUND`.
async fn resolve_group_filter(
    db: &PgPool,
    company_id: i32,
    group_uuid: Option<&str>,
) -> ApiResult<Option<i32>> {
    let Some(g) = group_uuid else {
        return Ok(None);
    };
    match find_group(db, company_id, g).await? {
        Some((id, _)) => Ok(Some(id)),
        None => Err(group_not_found()),
    }
}

/// `where` daftar mobile (alias `s`), mulai dengan ` WHERE`.
pub fn push_mobile_where(
    qb: &mut QueryBuilder<'_, Postgres>,
    company_id: i32,
    search: Option<&str>,
    scope: &StoreScope,
    group_filter: Option<i32>,
    censored: &CensoredColumns,
) {
    qb.push(r#" WHERE s."companyId" = "#)
        .push_bind(company_id)
        .push(r#" AND s."deletedAt" IS NULL AND s."isActive" = true"#);
    if let Some(search) = search.filter(|s| !s.is_empty()) {
        // Tanpa `address` (beda dengan admin).
        let cols = allowed_search_branches(
            [
                ("name", "name"),
                ("code", "code"),
                ("ownerName", "\"ownerName\""),
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
                qb.push(format!("s.{col} ILIKE '%' || "))
                    .push_bind(search.to_owned())
                    .push(" || '%'");
            }
            qb.push(")");
        }
    }
    push_scope_clause(qb, scope, r#"s."groupId""#);
    if let Some(g) = group_filter {
        qb.push(r#" AND s."groupId" = "#).push_bind(g);
    }
}

/// `findAll(userId, query, censored)` → `{ data, meta }`.
pub async fn find_all(
    state: &AppState,
    user_id: i32,
    q: &ListMobileStoresQuery,
    censored: &CensoredColumns,
) -> ApiResult<Value> {
    let db = &state.db;
    let company_id = require_company_id(db, user_id).await?;
    let (page, limit) = (q.page, q.limit);
    if q.lat.is_some() != q.lng.is_some() {
        return Err(coords_together());
    }
    if q.sort == MobileStoreSortField::distance && q.lat.is_none() {
        return Err(ApiError::bad_request("sort=distance requires lat and lng"));
    }
    assert_sortable_column(TABLE_KEY, sort_column_key(q.sort), censored)?;
    let (scope, group_filter) = tokio::try_join!(
        resolve_store_scope(db, user_id, company_id, None),
        resolve_group_filter(db, company_id, q.group_id.as_deref())
    )?;
    let base = |qb: &mut QueryBuilder<'_, Postgres>| {
        push_mobile_where(
            qb,
            company_id,
            q.search.as_deref(),
            &scope,
            group_filter,
            censored,
        );
    };

    let (Some(lat), Some(lng)) = (q.lat, q.lng) else {
        let rows_fut = async {
            let mut qb = QueryBuilder::new(STORE_SELECT);
            base(&mut qb);
            qb.push(order_sql(q.sort));
            qb.push(" OFFSET ").push_bind((page - 1) * limit);
            qb.push(" LIMIT ").push_bind(limit);
            Ok::<_, ApiError>(qb.build_query_as::<StoreRow>().fetch_all(db).await?)
        };
        let count_fut = async {
            let mut qb = QueryBuilder::new("SELECT COUNT(*) FROM store s");
            base(&mut qb);
            Ok::<_, ApiError>(qb.build_query_scalar::<i64>().fetch_one(db).await?)
        };
        let (rows, total) = tokio::try_join!(rows_fut, count_fut)?;
        return Ok(json!({
            "data": rows.iter().map(|r| r.to_mobile(None)).collect::<Vec<_>>(),
            "meta": { "total": total, "page": page, "limit": limit, "totalPages": total_pages(total, limit) },
        }));
    };

    let radius_km = q.radius_km.unwrap_or(DEFAULT_RADIUS_KM);
    let bx = bounding_box(lat, lng, radius_km);
    let mut qb = QueryBuilder::new(STORE_SELECT);
    base(&mut qb);
    qb.push(" AND s.lat >= ").push_bind(bx.lat.gte);
    qb.push(" AND s.lat <= ").push_bind(bx.lat.lte);
    qb.push(" AND s.lng >= ").push_bind(bx.lng.gte);
    qb.push(" AND s.lng <= ").push_bind(bx.lng.lte);
    qb.push(" ORDER BY s.id LIMIT ")
        .push_bind(MAX_PREFILTER_ROWS + 1);
    let rows = qb.build_query_as::<StoreRow>().fetch_all(db).await?;
    if i64::try_from(rows.len()).unwrap_or(i64::MAX) > MAX_PREFILTER_ROWS {
        return Err(ApiError::bad_request(format!(
            "Too many stores within radiusKm={} (more than {MAX_PREFILTER_ROWS}). Narrow radiusKm or add a search term.",
            num_string(radius_km)
        )));
    }
    let (total, page_rows) = proximity_page(rows, lat, lng, radius_km, q.sort, page, limit);
    Ok(json!({
        "data": page_rows.iter().map(|(r, d)| r.to_mobile(Some(*d))).collect::<Vec<_>>(),
        "meta": { "total": total, "page": page, "limit": limit, "totalPages": total_pages(total, limit) },
    }))
}

/// Bagian murni jalur jarak: hitung jarak, buang sudut kotak, urutkan, paginasi.
pub fn proximity_page(
    rows: Vec<StoreRow>,
    lat: f64,
    lng: f64,
    radius_km: f64,
    sort: MobileStoreSortField,
    page: i64,
    limit: i64,
) -> (i64, Vec<(StoreRow, i64)>) {
    let radius_m = radius_km * 1000.0;
    let mut with: Vec<(StoreRow, i64)> = rows
        .into_iter()
        .map(|r| {
            let d = get_distance(lat, lng, r.lat, r.lng);
            (r, d)
        })
        .filter(|(_, d)| {
            #[allow(clippy::cast_precision_loss)]
            let d = *d as f64;
            d <= radius_m
        })
        .collect();
    with.sort_by(|a, b| compare_in_memory(sort, a, b));
    let total = i64::try_from(with.len()).unwrap_or(i64::MAX);
    let start = usize::try_from((page - 1).saturating_mul(limit)).unwrap_or(usize::MAX);
    let page_rows = with
        .into_iter()
        .skip(start)
        .take(usize::try_from(limit).unwrap_or(0))
        .collect();
    (total, page_rows)
}

/// `resolveQr(userId, dto)`: token → toko (tanpa menggemakan `qrToken`).
pub async fn resolve_qr(state: &AppState, user_id: i32, dto: &ResolveStoreQr) -> ApiResult<Value> {
    let db = &state.db;
    let company_id = require_company_id(db, user_id).await?;
    if dto.lat.is_some() != dto.lng.is_some() {
        return Err(coords_together());
    }
    let store_fut = async {
        Ok::<Option<StoreRow>, ApiError>(
            sqlx::query_as(&format!(
                r#"{STORE_SELECT} WHERE s."qrToken" = $1 AND s."companyId" = $2 AND s."deletedAt" IS NULL LIMIT 1"#
            ))
            .bind(&dto.qr_token)
            .bind(company_id)
            .fetch_optional(db)
            .await?,
        )
    };
    let (store, scope) = tokio::try_join!(
        store_fut,
        resolve_store_scope(db, user_id, company_id, None)
    )?;
    let store = store.ok_or_else(store_not_found)?;
    if !store.is_active {
        return Err(ApiError::coded(
            StatusCode::UNPROCESSABLE_ENTITY,
            "STORE_INACTIVE",
            "Store is inactive and cannot be visited",
        ));
    }
    let position = match (dto.lat, dto.lng) {
        (Some(lat), Some(lng)) => Some(ScopePosition {
            lat,
            lng,
            accuracy_meters: 0.0,
            is_mock: None,
        }),
        _ => None,
    };
    assert_in_scope(
        db,
        ScopeDenial {
            scope: &scope,
            company_id,
            user_id,
            store: ScopeStore {
                id: store.id,
                group_id: store.group_id,
                lat: store.lat,
                lng: store.lng,
            },
            position,
            client_request_id: None,
        },
    )
    .await?;
    let distance = position.map(|p| get_distance(p.lat, p.lng, store.lat, store.lng));
    Ok(store.to_mobile(distance))
}

/// `findOne(userId, id)`: toko aktif + 5 kunjungan terakhir milik caller.
pub async fn find_one(state: &AppState, user_id: i32, id: &str) -> ApiResult<Value> {
    let db = &state.db;
    let company_id = require_company_id(db, user_id).await?;
    let uuid = prisma_uuid(id)?;
    let store_fut = async {
        Ok::<Option<StoreRow>, ApiError>(
            sqlx::query_as(&format!(
                r#"{STORE_SELECT} WHERE s.uuid = $1 AND s."companyId" = $2 AND s."deletedAt" IS NULL
                     AND s."isActive" = true LIMIT 1"#
            ))
            .bind(uuid)
            .bind(company_id)
            .fetch_optional(db)
            .await?,
        )
    };
    let (store, scope) = tokio::try_join!(
        store_fut,
        resolve_store_scope(db, user_id, company_id, None)
    )?;
    let store = store.ok_or_else(store_not_found)?;
    assert_in_scope(
        db,
        ScopeDenial {
            scope: &scope,
            company_id,
            user_id,
            store: ScopeStore {
                id: store.id,
                group_id: store.group_id,
                lat: store.lat,
                lng: store.lng,
            },
            position: None,
            client_request_id: None,
        },
    )
    .await?;
    let visits: Vec<(String, JsDate, Option<String>, Option<i32>)> = sqlx::query_as(
        r#"SELECT uuid::text, "checkInAt", status::text, "durationSeconds" FROM "storeVisit"
            WHERE "storeId" = $1 AND "userId" = $2 AND "companyId" = $3
            ORDER BY "checkInAt" DESC LIMIT 5"#,
    )
    .bind(store.id)
    .bind(user_id)
    .bind(company_id)
    .fetch_all(db)
    .await?;
    let mut v = store.to_mobile(None);
    if let Some(o) = v.as_object_mut() {
        o.insert(
            "recentVisits".into(),
            Value::Array(
                visits
                    .into_iter()
                    .map(|(id, at, status, dur)| {
                        json!({ "id": id, "checkInAt": at.timestamp_millis(), "status": status, "durationSeconds": dur })
                    })
                    .collect(),
            ),
        );
    }
    Ok(v)
}

/// `resolveTargetGroupId`: 404 / 422 / 403 `STORE_GROUP_OUT_OF_SCOPE`.
async fn resolve_target_group_id(
    db: &PgPool,
    company_id: i32,
    group_uuid: &Field<String>,
    scope: &StoreScope,
) -> ApiResult<Option<i32>> {
    let g = match group_uuid {
        Field::Absent => return Ok(None),
        // `uuid: null` → Prisma menolak → 500.
        Field::Null => {
            return Err(ApiError::internal(anyhow::anyhow!(
                "Argument `uuid` must not be null."
            )));
        }
        Field::Val(g) => g,
    };
    let (id, active) = find_group(db, company_id, g)
        .await?
        .ok_or_else(group_not_found)?;
    if !active {
        return Err(group_inactive());
    }
    if let Some(s) = scope
        && !s.group_ids.contains(&id)
    {
        return Err(ApiError::coded(
            StatusCode::FORBIDDEN,
            "STORE_GROUP_OUT_OF_SCOPE",
            "This store group is outside the region assigned to you",
        ));
    }
    Ok(Some(id))
}

/// `assertNoNearbyDuplicate`: nama sama (insensitive, persis) dalam 50 m → 409 `STORE_DUPLICATE_NEARBY`.
async fn assert_no_nearby_duplicate(
    db: &PgPool,
    company_id: i32,
    dto: &CreateMobileStore,
) -> ApiResult<()> {
    let bx = bounding_box(dto.lat, dto.lng, DUPLICATE_RADIUS_METERS / 1000.0);
    let candidates: Vec<(String, f64, f64)> = sqlx::query_as(
        r#"SELECT uuid::text, lat, lng FROM store
            WHERE "companyId" = $1 AND "deletedAt" IS NULL AND name ILIKE $2
              AND lat >= $3 AND lat <= $4 AND lng >= $5 AND lng <= $6
            LIMIT 10"#,
    )
    .bind(company_id)
    .bind(&dto.name)
    .bind(bx.lat.gte)
    .bind(bx.lat.lte)
    .bind(bx.lng.gte)
    .bind(bx.lng.lte)
    .fetch_all(db)
    .await?;
    if let Some((uuid, d)) = first_duplicate(dto.lat, dto.lng, &candidates) {
        return Err(ApiError::body(
            StatusCode::CONFLICT,
            json!({
                "code": "STORE_DUPLICATE_NEARBY",
                "message": "A store with this name already exists at this location",
                "details": { "storeId": uuid, "distanceMeters": d },
            }),
        ));
    }
    Ok(())
}

/// Bagian murni: kandidat pertama dalam `DUPLICATE_RADIUS_METERS` (sudut kotak dibuang).
pub fn first_duplicate(
    lat: f64,
    lng: f64,
    candidates: &[(String, f64, f64)],
) -> Option<(String, i64)> {
    candidates.iter().find_map(|(uuid, clat, clng)| {
        let d = get_distance(lat, lng, *clat, *clng);
        #[allow(clippy::cast_precision_loss)]
        let within = (d as f64) <= DUPLICATE_RADIUS_METERS;
        within.then(|| (uuid.clone(), d))
    })
}

/// `create(userId, dto)`: kode `SLS-XXXXX` & qrToken buatan server, radius `null`, aktif.
pub async fn create(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    dto: &CreateMobileStore,
) -> ApiResult<Value> {
    let db = &state.db;
    let company_id = require_company_id(db, user_id).await?;
    let (scope, ()) = tokio::try_join!(
        resolve_store_scope(db, user_id, company_id, None),
        assert_no_nearby_duplicate(db, company_id, dto)
    )?;
    let group_id = resolve_target_group_id(db, company_id, &dto.group_id, &scope).await?;
    let phone = mz_customers::phone::normalize_store_phone(dto.phone.as_deref());
    let mut attempt = 1;
    loop {
        let code = generate_store_code();
        let result: Result<i32, InsertErr> = async {
            let mut tx = db.begin().await?;
            assert_plan_limit(&mut tx, company_id, PlanLimit::Stores).await?;
            let id: i32 = sqlx::query_scalar(
                r#"INSERT INTO store ("companyId", code, name, "ownerName", phone, address, lat, lng,
                                      "radiusMeters", "qrToken", "isActive", "createdById", "groupId")
                   VALUES ($1, $2, $3, $4, $5, $6, $7, $8, NULL, $9, true, $10, $11) RETURNING id"#,
            )
            .bind(company_id)
            .bind(&code)
            .bind(&dto.name)
            .bind(&dto.owner_name)
            .bind(&phone)
            .bind(&dto.address)
            .bind(dto.lat)
            .bind(dto.lng)
            .bind(generate_qr_token())
            .bind(user_id)
            .bind(group_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(InsertErr::Db)?;
            if let Some(r) = audit.result_row(&mut *tx, Model::Store, Lookup::Id(id)).await {
                audit.record_create(Model::Store, &r).await;
            }
            tx.commit().await?;
            Ok(id)
        }
        .await;
        match result {
            Ok(id) => return Ok(store_by_id(db, id).await?.to_mobile(None)),
            Err(InsertErr::Db(e)) if mz_core::db::is_unique_violation(&e) => {
                if attempt < CREATE_MAX_ATTEMPTS {
                    attempt += 1;
                    continue;
                }
                let code = if is_qr_token_conflict(&e) {
                    "STORE_QR_TOKEN_COLLISION"
                } else {
                    "STORE_CODE_COLLISION"
                };
                return Err(ApiError::coded(
                    StatusCode::CONFLICT,
                    code,
                    "Could not allocate a unique store code; please retry",
                ));
            }
            Err(InsertErr::Db(e)) => return Err(e.into()),
            Err(InsertErr::Api(e)) => return Err(e),
        }
    }
}

enum InsertErr {
    Db(sqlx::Error),
    Api(ApiError),
}

impl From<ApiError> for InsertErr {
    fn from(e: ApiError) -> Self {
        Self::Api(e)
    }
}

impl From<sqlx::Error> for InsertErr {
    fn from(e: sqlx::Error) -> Self {
        Self::Api(e.into())
    }
}

// ───────────────────────── handler ─────────────────────────

/// Node: src/store-visits/mobile-stores.controller.ts:131 `findAll`
pub async fn h_find_all(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedQuery(q): ValidatedQuery<Raw>,
) -> ApiResult<Json<Value>> {
    let q = parse_list_mobile_stores(&q.0).map_err(ApiError::Validation)?;
    let censored = censored_for(&state, &ctx).await?;
    let body = find_all(&state, ctx.require_user_id()?, &q, &censored).await?;
    Ok(censor(body, &censored))
}

/// Node: src/store-visits/mobile-stores.controller.ts:226 `create` (201)
pub async fn h_create(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    ValidatedJson(body): ValidatedJson<Raw>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let dto = parse_create_mobile_store(&body.0).map_err(ApiError::Validation)?;
    let data = create(&state, &audit, ctx.require_user_id()?, &dto).await?;
    let censored = censored_for(&state, &ctx).await?;
    Ok((
        StatusCode::CREATED,
        censor(json!({ "data": data }), &censored),
    ))
}

/// Node: src/store-visits/mobile-stores.controller.ts:372 `resolveQr` (200)
pub async fn h_resolve_qr(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedJson(body): ValidatedJson<Raw>,
) -> ApiResult<Json<Value>> {
    let dto = parse_resolve_qr(&body.0).map_err(ApiError::Validation)?;
    let data = resolve_qr(&state, ctx.require_user_id()?, &dto).await?;
    let censored = censored_for(&state, &ctx).await?;
    Ok(censor(json!({ "data": data }), &censored))
}

/// Node: src/store-visits/mobile-stores.controller.ts:458 `findOne`
pub async fn h_find_one(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let data = find_one(&state, ctx.require_user_id()?, &id).await?;
    let censored = censored_for(&state, &ctx).await?;
    Ok(censor(json!({ "data": data }), &censored))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stores::common::tests::row;
    use mz_table_fields::registry::FieldVisibilityMode;

    fn at(lat: f64, lng: f64, name: &str, last: Option<i64>) -> StoreRow {
        let mut r = row();
        r.lat = lat;
        r.lng = lng;
        r.name = name.into();
        r.code = name.into();
        r.last_visit_at = last.and_then(JsDate::from_millis);
        r
    }

    #[test]
    fn codes_use_unambiguous_alphabet() {
        for _ in 0..200 {
            let c = generate_store_code();
            assert_eq!(c.len(), 9);
            assert!(c.starts_with("SLS-"));
            assert!(c[4..].chars().all(|ch| CODE_ALPHABET.contains(ch)));
            assert!(!c[4..].chars().any(|ch| matches!(ch, '0' | 'O' | '1' | 'I')));
        }
    }

    #[test]
    fn sort_keys() {
        assert_eq!(
            sort_column_key(MobileStoreSortField::lastVisit),
            Some("lastVisitAt")
        );
        assert_eq!(sort_column_key(MobileStoreSortField::distance), None);
        let censored: CensoredColumns =
            [("lastVisitAt".to_owned(), FieldVisibilityMode::Hidden)].into();
        assert!(
            assert_sortable_column(
                TABLE_KEY,
                sort_column_key(MobileStoreSortField::lastVisit),
                &censored
            )
            .is_err()
        );
        assert!(
            assert_sortable_column(
                TABLE_KEY,
                sort_column_key(MobileStoreSortField::distance),
                &censored
            )
            .is_ok()
        );
        assert_eq!(
            order_sql(MobileStoreSortField::lastVisit),
            r#" ORDER BY s."lastVisitAt" DESC NULLS LAST"#
        );
    }

    #[test]
    fn proximity_drops_corners_sorts_and_paginates() {
        let rows = vec![
            at(0.0, 0.008, "B", None),
            at(0.0, 0.001, "A", Some(5)),
            // Sudut kotak 1 km: ~1.41 km → dibuang.
            at(0.00899, 0.00899, "Corner", None),
            at(0.0, 0.005, "C", Some(9)),
        ];
        let (total, page) = proximity_page(
            rows.clone(),
            0.0,
            0.0,
            1.0,
            MobileStoreSortField::distance,
            1,
            2,
        );
        assert_eq!(total, 3);
        let names: Vec<&str> = page.iter().map(|(r, _)| r.name.as_str()).collect();
        assert_eq!(names, vec!["A", "C"]);
        assert!(page[0].1 < page[1].1);
        let (_, p2) = proximity_page(
            rows.clone(),
            0.0,
            0.0,
            1.0,
            MobileStoreSortField::distance,
            2,
            2,
        );
        assert_eq!(p2[0].0.name, "B");
        let (_, by_last) = proximity_page(
            rows.clone(),
            0.0,
            0.0,
            1.0,
            MobileStoreSortField::lastVisit,
            1,
            10,
        );
        let names: Vec<&str> = by_last.iter().map(|(r, _)| r.name.as_str()).collect();
        assert_eq!(names, vec!["C", "A", "B"]);
        let (_, by_name) = proximity_page(rows, 0.0, 0.0, 1.0, MobileStoreSortField::name, 1, 10);
        assert!(by_name.iter().all(|(_, d)| *d >= 0));
        assert_eq!(by_name[0].0.name, "A");
    }

    #[test]
    fn duplicate_requires_true_distance() {
        let c = vec![
            ("far".to_owned(), 0.0004, 0.0004),
            ("near".to_owned(), 0.0001, 0.0),
        ];
        assert_eq!(
            first_duplicate(0.0, 0.0, &c).map(|x| x.0),
            Some("near".to_owned())
        );
        assert!(first_duplicate(0.0, 0.0, &c[..1]).is_none());
    }

    #[test]
    fn mobile_search_has_no_address_and_scope_joins_via_and() {
        let mut qb = QueryBuilder::<Postgres>::new("SELECT 1 FROM store s");
        let scope = Some(crate::scope::Scope {
            group_ids: vec![1],
            strict: false,
        });
        push_mobile_where(
            &mut qb,
            1,
            Some("x"),
            &scope,
            Some(7),
            &CensoredColumns::new(),
        );
        let s = qb.sql();
        assert!(!s.contains("address"));
        assert!(s.contains(r#"s."ownerName" ILIKE"#));
        assert!(s.contains(r#"AND (s."groupId" = ANY("#));
        assert!(s.ends_with(r#" AND s."groupId" = $6"#));
    }
}
