//! Port `src/store-visits/store-visit-plans.service.ts` (§4.1, §4.3) — CRUD jadwal kunjungan admin.
//!
//! Tanpa konversi zona waktu: seluruh tanggal lewat [`crate::plan_dates`] (UTC polos).
//! `storeVisitPlan` ber-audit (`AuditHandle`): `createMany` → ringkasan `bulk`, soft delete lewat
//! `updateMany`, `update` per baris. Setiap pembacaan menyaring `deletedAt IS NULL`.

use std::collections::{HashMap, HashSet};

use axum::http::StatusCode;
use chrono::NaiveDate;
use mz_audit::{AuditHandle, Lookup, Model, Row};
use mz_core::company::require_company_id;
use mz_core::time::{JsDate, now_naive};
use mz_core::{ApiError, ApiResult, AppState};
use mz_sales_geo::dto::Field;
use mz_sales_geo::visit_masters::{VisitMasterRow, find_default, resolve_visit_types};
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool, Postgres, QueryBuilder};

use super::dto::{CreatePlans, ListPlans, PlanFilters, PlanStoreItem, ReplaceDay, UpdatePlan};
use super::logic::{
    Bucket, MAX_PLANS_PER_DAY, PlanRangeOk, already_fulfilled, assert_batch_size, assert_bulk_fits,
    assert_plan_date, assert_writable_dates, classify, compliance_row, daily_limit,
    duplicate_plans, is_fulfilled, plan_not_found, plan_status, resolve_range,
    resolve_requested_dates, store_ref, store_visit_plan_not_found, visit_type_locked,
};
use crate::enums::{StoreVisitPlanComplianceGroupBy, StoreVisitPlanStatus};
use crate::groups::{row_uuid, total_pages};
use crate::plan_dates::{add_plan_days, from_plan_date, to_plan_date, today_plan_date};
use crate::scope::{StoreScope, is_in_scope, resolve_store_scope};

// ───────────────────────── baris & serializer ─────────────────────────

/// Kolom `storeVisitPlan` + relasi yang dibaca serializer §4.1 (`planSelect`).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PlanRow {
    pub id: i32,
    pub uuid: String,
    #[sqlx(rename = "userId")]
    pub user_id: i32,
    #[sqlx(rename = "visitDate")]
    pub visit_date: NaiveDate,
    #[sqlx(rename = "sortOrder")]
    pub sort_order: Option<i32>,
    pub note: Option<String>,
    #[sqlx(rename = "createdAt")]
    pub created_at: JsDate,
    pub vt_uuid: Option<String>,
    pub vt_code: Option<String>,
    pub vt_name: Option<String>,
    pub vt_requires_order: Option<bool>,
    pub vt_requires_photo: Option<bool>,
    pub vt_requires_gps: Option<bool>,
    pub vt_is_active: Option<bool>,
    pub store_uuid: String,
    pub store_code: String,
    pub store_name: String,
    pub store_is_active: bool,
    pub store_deleted: bool,
    pub group_uuid: Option<String>,
    pub group_code: Option<String>,
    pub group_name: Option<String>,
    pub created_by_uuid: Option<String>,
    pub created_by_name: Option<String>,
    pub visit_uuid: Option<String>,
    pub visit_number: Option<String>,
    pub visit_check_in_at: Option<JsDate>,
    pub visit_status: Option<String>,
    pub visit_state: Option<String>,
}

/// `SELECT … FROM "storeVisitPlan" p` + join (tanpa `WHERE`).
pub const PLAN_SELECT: &str = r#"SELECT p.id, p.uuid::text AS uuid, p."userId", p."visitDate", p."sortOrder", p.note,
       p."createdAt",
       vt.uuid::text AS vt_uuid, vt.code AS vt_code, vt.name AS vt_name,
       vt."requiresOrder" AS vt_requires_order, vt."requiresPhoto" AS vt_requires_photo,
       vt."requiresGps" AS vt_requires_gps, vt."isActive" AS vt_is_active,
       s.uuid::text AS store_uuid, s.code AS store_code, s.name AS store_name,
       s."isActive" AS store_is_active, (s."deletedAt" IS NOT NULL) AS store_deleted,
       g.uuid::text AS group_uuid, g.code AS group_code, g.name AS group_name,
       cb.uuid::text AS created_by_uuid, cb.name AS created_by_name,
       v.uuid::text AS visit_uuid, v."visitNumber" AS visit_number, v."checkInAt" AS visit_check_in_at,
       v.status::text AS visit_status, v.state::text AS visit_state
  FROM "storeVisitPlan" p
  JOIN store s ON s.id = p."storeId"
  LEFT JOIN "storeGroup" g ON g.id = s."groupId"
  LEFT JOIN users cb ON cb.id = p."createdById"
  LEFT JOIN "visitType" vt ON vt.id = p."visitTypeId"
  LEFT JOIN "storeVisit" v ON v."planId" = p.id"#;

/// `toVisitTypeRef` atas kolom gabungan (bentuk `mz_sales_geo::visit_masters::to_visit_type_ref`).
pub(crate) fn visit_type_ref(
    uuid: &Option<String>,
    code: &Option<String>,
    name: &Option<String>,
    flags: (Option<bool>, Option<bool>, Option<bool>, Option<bool>),
) -> Value {
    match (uuid, code, name) {
        (Some(u), Some(c), Some(n)) => json!({
            "id": u,
            "code": c,
            "name": n,
            "requiresOrder": flags.0.unwrap_or(false),
            "requiresPhoto": flags.1.unwrap_or(false),
            "requiresGps": flags.2.unwrap_or(true),
            "isActive": flags.3.unwrap_or(true),
        }),
        _ => Value::Null,
    }
}

/// Sales yang dijadwalkan (query kedua, §0.12).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SalesInfo {
    pub uuid: String,
    pub name: String,
    pub is_active_member: bool,
}

/// `toPlan(row, today, sales)`.
pub fn to_plan(row: &PlanRow, today: &str, sales: &HashMap<i32, SalesInfo>) -> Value {
    let status = plan_status(row.visit_state.as_deref(), row.visit_date, today);
    let member = sales.get(&row.user_id);
    let group = match (&row.group_uuid, &row.group_code, &row.group_name) {
        (Some(u), Some(c), Some(n)) => json!({ "id": u, "code": c, "name": n }),
        _ => Value::Null,
    };
    let visit = match (&row.visit_uuid, status) {
        (Some(u), StoreVisitPlanStatus::fulfilled) => json!({
            "id": u,
            "visitNumber": row.visit_number,
            "checkInAt": row.visit_check_in_at.map(|d| d.timestamp_millis()),
            "status": row.visit_status,
        }),
        _ => Value::Null,
    };
    let created_by = match (&row.created_by_uuid, &row.created_by_name) {
        (Some(u), Some(n)) => json!({ "id": u, "name": n }),
        _ => Value::Null,
    };
    json!({
        "id": row.uuid,
        "visitDate": from_plan_date(row.visit_date),
        "sortOrder": row.sort_order,
        "note": row.note,
        "visitType": visit_type_ref(&row.vt_uuid, &row.vt_code, &row.vt_name, (
            row.vt_requires_order, row.vt_requires_photo, row.vt_requires_gps, row.vt_is_active,
        )),
        "sales": {
            "id": member.map(|m| m.uuid.clone()),
            "name": member.map(|m| m.name.clone()),
            "isActiveMember": member.is_some_and(|m| m.is_active_member),
        },
        "store": {
            "id": row.store_uuid,
            "code": row.store_code,
            "name": row.store_name,
            "isActive": row.store_is_active,
            "isDeleted": row.store_deleted,
            "group": group,
        },
        "status": status,
        "visit": visit,
        "createdBy": created_by,
        "createdAt": row.created_at.timestamp_millis(),
    })
}

/// `loadSales(companyId, userIds)`.
pub async fn load_sales(
    db: &PgPool,
    company_id: i32,
    user_ids: &[i32],
) -> ApiResult<HashMap<i32, SalesInfo>> {
    let mut ids: Vec<i32> = user_ids.to_vec();
    ids.sort_unstable();
    ids.dedup();
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let users: Vec<(i32, String, String)> =
        sqlx::query_as(r#"SELECT id, uuid::text, name FROM users WHERE id = ANY($1)"#)
            .bind(&ids)
            .fetch_all(db)
            .await?;
    let active: HashSet<i32> = sqlx::query_scalar::<_, i32>(
        r#"SELECT "userId" FROM "userCompany" WHERE "companyId" = $1 AND status = 1 AND "userId" = ANY($2)"#,
    )
    .bind(company_id)
    .bind(&ids)
    .fetch_all(db)
    .await?
    .into_iter()
    .collect();
    Ok(users
        .into_iter()
        .map(|(id, uuid, name)| {
            (
                id,
                SalesInfo {
                    uuid,
                    name,
                    is_active_member: active.contains(&id),
                },
            )
        })
        .collect())
}

// ───────────────────────── filter ─────────────────────────

/// Hasil `buildWhere` yang sudah diterjemahkan (uuid → id).
#[derive(Debug, Clone)]
struct Where {
    company_id: i32,
    gte: NaiveDate,
    lte: NaiveDate,
    user_ids: Vec<i32>,
    visit_type_ids: Option<Vec<i32>>,
    store_uuids: Option<Vec<uuid::Uuid>>,
    group_uuids: Option<Vec<uuid::Uuid>>,
}

fn parse_uuids(v: &Option<Vec<String>>) -> ApiResult<Option<Vec<uuid::Uuid>>> {
    v.as_ref()
        .filter(|v| !v.is_empty())
        .map(|v| v.iter().map(|u| row_uuid(u)).collect::<ApiResult<Vec<_>>>())
        .transpose()
}

/// `buildWhere(companyId, range, query)`: hanya sales yang masih anggota aktif company.
async fn build_where(
    db: &PgPool,
    company_id: i32,
    range: &PlanRangeOk,
    f: &PlanFilters,
) -> ApiResult<Where> {
    let active: Vec<i32> = sqlx::query_scalar(
        r#"SELECT "userId" FROM "userCompany" WHERE "companyId" = $1 AND status = 1 ORDER BY id"#,
    )
    .bind(company_id)
    .fetch_all(db)
    .await?;
    let user_ids = match parse_uuids(&f.sales_id)? {
        Some(uuids) => {
            let requested: HashSet<i32> =
                sqlx::query_scalar::<_, i32>(r#"SELECT id FROM users WHERE uuid = ANY($1)"#)
                    .bind(&uuids)
                    .fetch_all(db)
                    .await?
                    .into_iter()
                    .collect();
            active
                .into_iter()
                .filter(|id| requested.contains(id))
                .collect()
        }
        None => active,
    };
    let visit_type_ids = match parse_uuids(&f.visit_type_id)? {
        Some(uuids) => Some(
            sqlx::query_scalar(
                r#"SELECT id FROM "visitType" WHERE "companyId" = $1 AND uuid = ANY($2)"#,
            )
            .bind(company_id)
            .bind(&uuids)
            .fetch_all(db)
            .await?,
        ),
        None => None,
    };
    Ok(Where {
        company_id,
        gte: range.gte,
        lte: range.lte,
        user_ids,
        visit_type_ids,
        store_uuids: parse_uuids(&f.store_id)?,
        group_uuids: parse_uuids(&f.group_id)?,
    })
}

fn push_where(qb: &mut QueryBuilder<'_, Postgres>, w: &Where) {
    qb.push(r#" WHERE p."companyId" = "#)
        .push_bind(w.company_id);
    qb.push(r#" AND p."deletedAt" IS NULL AND p."visitDate" >= "#)
        .push_bind(w.gte)
        .push(r#" AND p."visitDate" <= "#)
        .push_bind(w.lte);
    qb.push(r#" AND p."userId" = ANY("#)
        .push_bind(w.user_ids.clone())
        .push(")");
    if let Some(ids) = &w.visit_type_ids {
        qb.push(r#" AND p."visitTypeId" = ANY("#)
            .push_bind(ids.clone())
            .push(")");
    }
    if let Some(u) = &w.store_uuids {
        qb.push(" AND s.uuid = ANY(").push_bind(u.clone()).push(")");
    }
    if let Some(u) = &w.group_uuids {
        qb.push(" AND g.uuid = ANY(").push_bind(u.clone()).push(")");
    }
}

/// `FROM` tanpa kolom (untuk hitungan) dengan join yang dibutuhkan filter & status.
const PLAN_FROM: &str = r#" FROM "storeVisitPlan" p
  JOIN store s ON s.id = p."storeId"
  LEFT JOIN "storeGroup" g ON g.id = s."groupId"
  LEFT JOIN "storeVisit" v ON v."planId" = p.id"#;

/// §2.1 `statusWhere` (di database, bukan sesudah paginasi).
fn push_status(
    qb: &mut QueryBuilder<'_, Postgres>,
    status: StoreVisitPlanStatus,
    today: NaiveDate,
) {
    const FULFILLED: &str =
        " AND (v.id IS NOT NULL AND v.state <> 'cancelled'::\"StoreVisitState\")";
    const UNFULFILLED: &str = " AND (v.id IS NULL OR v.state = 'cancelled'::\"StoreVisitState\")";
    match status {
        StoreVisitPlanStatus::fulfilled => {
            qb.push(FULFILLED);
        }
        StoreVisitPlanStatus::missed => {
            qb.push(UNFULFILLED)
                .push(r#" AND p."visitDate" < "#)
                .push_bind(today);
        }
        StoreVisitPlanStatus::pending => {
            qb.push(UNFULFILLED)
                .push(r#" AND p."visitDate" >= "#)
                .push_bind(today);
        }
    }
}

fn today_date(today: &str) -> NaiveDate {
    to_plan_date(today).unwrap_or_default()
}

// ───────────────────────── baca ─────────────────────────

/// `findAll(userId, query)` → `{ data, meta }`.
pub async fn find_all(state: &AppState, user_id: i32, q: &ListPlans) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let range = resolve_range(Some(&q.filters.start_date), Some(&q.filters.end_date))?;
    let today = today_plan_date();
    let w = build_where(&state.db, company_id, &range, &q.filters).await?;

    let mut qb = QueryBuilder::<Postgres>::new(PLAN_SELECT);
    push_where(&mut qb, &w);
    if let Some(s) = q.status {
        push_status(&mut qb, s, today_date(&today));
    }
    qb.push(r#" ORDER BY p."visitDate" ASC, p."sortOrder" ASC NULLS LAST, p.id ASC OFFSET "#)
        .push_bind((q.page - 1) * q.limit)
        .push(" LIMIT ")
        .push_bind(q.limit);
    let rows: Vec<PlanRow> = qb.build_query_as().fetch_all(&state.db).await?;

    let mut cq = QueryBuilder::<Postgres>::new(format!("SELECT COUNT(*){PLAN_FROM}"));
    push_where(&mut cq, &w);
    if let Some(s) = q.status {
        push_status(&mut cq, s, today_date(&today));
    }
    let total: i64 = cq.build_query_scalar().fetch_one(&state.db).await?;

    let ids: Vec<i32> = rows.iter().map(|r| r.user_id).collect();
    let sales = load_sales(&state.db, company_id, &ids).await?;
    let data: Vec<Value> = rows.iter().map(|r| to_plan(r, &today, &sales)).collect();
    Ok(json!({
        "data": data,
        "meta": { "page": q.page, "limit": q.limit, "total": total, "totalPages": total_pages(total, q.limit) },
    }))
}

/// `calendar(userId, query)` → `{ startDate, endDate, data }` per (tanggal, sales).
pub async fn calendar(state: &AppState, user_id: i32, f: &PlanFilters) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let range = resolve_range(Some(&f.start_date), Some(&f.end_date))?;
    let today = today_plan_date();
    let w = build_where(&state.db, company_id, &range, f).await?;
    let mut qb = QueryBuilder::<Postgres>::new(format!(
        r#"SELECT p."visitDate", p."userId", COUNT(*),
                  COUNT(*) FILTER (WHERE v.id IS NOT NULL AND v.state <> 'cancelled'::"StoreVisitState"){PLAN_FROM}"#
    ));
    push_where(&mut qb, &w);
    qb.push(r#" GROUP BY p."visitDate", p."userId""#);
    let cells: Vec<(NaiveDate, i32, i64, i64)> = qb.build_query_as().fetch_all(&state.db).await?;
    let ids: Vec<i32> = cells.iter().map(|c| c.1).collect();
    let sales = load_sales(&state.db, company_id, &ids).await?;
    let mut data: Vec<(String, Option<String>, Value)> = cells
        .iter()
        .map(|(d, uid, total, done)| {
            let date = from_plan_date(*d);
            let remaining = total - done;
            let is_past = date < today;
            let sales_id = sales.get(uid).map(|s| s.uuid.clone());
            let v = json!({
                "date": date,
                "salesId": sales_id,
                "planned": total,
                "fulfilled": done,
                "missed": if is_past { remaining } else { 0 },
                "pending": if is_past { 0 } else { remaining },
            });
            (date, sales_id, v)
        })
        .collect();
    data.sort_by(|a, b| {
        a.0.cmp(&b.0).then_with(|| {
            a.1.as_deref()
                .unwrap_or("")
                .cmp(b.1.as_deref().unwrap_or(""))
        })
    });
    Ok(json!({
        "startDate": range.start_date,
        "endDate": range.end_date,
        "data": data.into_iter().map(|x| x.2).collect::<Vec<_>>(),
    }))
}

/// Kunci bucket kepatuhan.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Key {
    User(i32),
    Region(Option<String>),
}

/// `compliance(userId, query)` (§4.3) → `{ startDate, endDate, totals, data }`.
pub async fn compliance(
    state: &AppState,
    user_id: i32,
    f: &PlanFilters,
    group_by: StoreVisitPlanComplianceGroupBy,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let range = resolve_range(Some(&f.start_date), Some(&f.end_date))?;
    let today = today_plan_date();
    let filters = PlanFilters {
        sales_id: f.sales_id.clone(),
        group_id: f.group_id.clone(),
        ..Default::default()
    };
    let w = build_where(&state.db, company_id, &range, &filters).await?;

    type PlanC = (
        i32,
        NaiveDate,
        bool,
        bool,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    let mut qb = QueryBuilder::<Postgres>::new(format!(
        r#"SELECT p."userId", p."visitDate", (s."deletedAt" IS NOT NULL), s."isActive",
                  g.uuid::text, g.name, v.state::text{PLAN_FROM}"#
    ));
    push_where(&mut qb, &w);
    qb.push(" ORDER BY p.id");
    let plan_rows: Vec<PlanC> = qb.build_query_as().fetch_all(&state.db).await?;
    let active: HashSet<i32> = sqlx::query_scalar::<_, i32>(
        r#"SELECT "userId" FROM "userCompany" WHERE "companyId" = $1 AND status = 1"#,
    )
    .bind(company_id)
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .collect();

    // `unplanned`: kunjungan completed tanpa planId di jendela UTC polos yang sama.
    let start = to_plan_date(&range.start_date).unwrap_or_default();
    let end_next = add_plan_days(&range.end_date, 1)
        .and_then(|d| to_plan_date(&d))
        .unwrap_or_default();
    let mut uq = QueryBuilder::<Postgres>::new(
        r#"SELECT sv."userId", g.uuid::text, g.name FROM "storeVisit" sv
             JOIN store s ON s.id = sv."storeId"
             LEFT JOIN "storeGroup" g ON g.id = s."groupId"
             JOIN users u ON u.id = sv."userId"
            WHERE sv."companyId" = "#,
    );
    uq.push_bind(company_id)
        .push(r#" AND sv."planId" IS NULL AND sv.state = 'completed'::"StoreVisitState" AND sv."checkInAt" >= "#)
        .push_bind(start.and_time(chrono::NaiveTime::MIN))
        .push(r#" AND sv."checkInAt" < "#)
        .push_bind(end_next.and_time(chrono::NaiveTime::MIN));
    if let Some(u) = parse_uuids(&f.sales_id)? {
        uq.push(" AND u.uuid = ANY(").push_bind(u).push(")");
    }
    if let Some(u) = parse_uuids(&f.group_id)? {
        uq.push(" AND g.uuid = ANY(").push_bind(u).push(")");
    }
    uq.push(" ORDER BY sv.id");
    let unplanned: Vec<(i32, Option<String>, Option<String>)> =
        uq.build_query_as().fetch_all(&state.db).await?;

    let is_region = group_by == StoreVisitPlanComplianceGroupBy::region;
    let mut order: Vec<Key> = Vec::new();
    let mut buckets: HashMap<Key, Bucket> = HashMap::new();
    let mut names: HashMap<String, String> = HashMap::new();
    fn bucket<'a>(
        order: &mut Vec<Key>,
        buckets: &'a mut HashMap<Key, Bucket>,
        k: Key,
    ) -> &'a mut Bucket {
        if !buckets.contains_key(&k) {
            order.push(k.clone());
        }
        buckets.entry(k).or_default()
    }
    for (uid, date, deleted, store_active, g_uuid, g_name, vstate) in &plan_rows {
        if let (Some(u), Some(n)) = (g_uuid, g_name) {
            names.insert(u.clone(), n.clone());
        }
        let key = if is_region {
            Key::Region(g_uuid.clone())
        } else {
            Key::User(*uid)
        };
        let cls = classify(
            *deleted,
            *store_active,
            vstate.as_deref(),
            *date,
            &today,
            active.contains(uid),
        );
        bucket(&mut order, &mut buckets, key).apply(cls);
    }
    for (uid, g_uuid, g_name) in &unplanned {
        if let (Some(u), Some(n)) = (g_uuid, g_name) {
            names.insert(u.clone(), n.clone());
        }
        let key = if is_region {
            Key::Region(g_uuid.clone())
        } else {
            Key::User(*uid)
        };
        bucket(&mut order, &mut buckets, key).unplanned += 1;
    }
    let mut sum = Bucket::default();
    for k in &order {
        if let Some(b) = buckets.get(k) {
            sum.add(b);
        }
    }
    let totals = Value::Object(sum.counts());

    let data: Vec<Value> = if is_region {
        let mut named: Vec<(String, Value)> = order
            .iter()
            .filter_map(|k| match k {
                Key::Region(Some(u)) => {
                    let name = names.get(u).cloned().unwrap_or_default();
                    let b = buckets.get(k).copied().unwrap_or_default();
                    Some((
                        name.clone(),
                        compliance_row(json!({ "id": u, "name": name }), &b),
                    ))
                }
                _ => None,
            })
            .collect();
        named.sort_by(|a, b| mz_core::jsfmt::collate_id(&a.0, &b.0));
        let mut out: Vec<Value> = named.into_iter().map(|x| x.1).collect();
        if let Some(b) = buckets.get(&Key::Region(None)) {
            out.push(compliance_row(
                json!({ "id": null, "name": "Tanpa region" }),
                b,
            ));
        }
        out
    } else {
        let ids: Vec<i32> = order
            .iter()
            .filter_map(|k| match k {
                Key::User(u) => Some(*u),
                Key::Region(_) => None,
            })
            .collect();
        let sales = load_sales(&state.db, company_id, &ids).await?;
        let mut rows: Vec<(String, Value)> = ids
            .iter()
            .map(|uid| {
                let m = sales.get(uid);
                let b = buckets.get(&Key::User(*uid)).copied().unwrap_or_default();
                (
                    m.map(|m| m.name.clone()).unwrap_or_default(),
                    compliance_row(
                        json!({ "id": m.map(|m| m.uuid.clone()), "name": m.map(|m| m.name.clone()) }),
                        &b,
                    ),
                )
            })
            .collect();
        rows.sort_by(|a, b| mz_core::jsfmt::collate_id(&a.0, &b.0));
        rows.into_iter().map(|x| x.1).collect()
    };
    Ok(json!({
        "startDate": range.start_date,
        "endDate": range.end_date,
        "totals": totals,
        "data": data,
    }))
}

// ───────────────────────── tulis ─────────────────────────

/// `resolveSalesId(companyId, userUuid)`: hanya anggota AKTIF company.
async fn resolve_sales_id(db: &PgPool, company_id: i32, user_uuid: &str) -> ApiResult<i32> {
    let u = row_uuid(user_uuid)?;
    let id: Option<i32> = sqlx::query_scalar(
        r#"SELECT uc."userId" FROM "userCompany" uc JOIN users u ON u.id = uc."userId"
            WHERE uc."companyId" = $1 AND uc.status = 1 AND u.uuid = $2 ORDER BY uc.id LIMIT 1"#,
    )
    .bind(company_id)
    .bind(u)
    .fetch_optional(db)
    .await?;
    id.ok_or_else(|| {
        ApiError::body(
            StatusCode::BAD_REQUEST,
            json!({
                "code": "SALES_NOT_IN_COMPANY",
                "message": "The scheduled user is not an active employee of this company",
                "details": { "userId": user_uuid },
            }),
        )
    })
}

/// Toko yang boleh dijadwalkan (`PlanStoreRow`).
#[derive(Debug, Clone, sqlx::FromRow)]
struct PlanStore {
    id: i32,
    uuid: String,
    code: String,
    name: String,
    #[sqlx(rename = "groupId")]
    group_id: Option<i32>,
}

/// §3.2 `resolveStores`: 404 `STORE_NOT_FOUND` (kolektif), 409 `STORE_OUT_OF_SCOPE_FOR_SALES`.
async fn resolve_stores(
    conn: &mut PgConnection,
    company_id: i32,
    scope: &StoreScope,
    items: &[PlanStoreItem],
) -> ApiResult<HashMap<String, PlanStore>> {
    let mut uuids: Vec<String> = Vec::new();
    for it in items {
        if !uuids.contains(&it.store_id) {
            uuids.push(it.store_id.clone());
        }
    }
    if uuids.is_empty() {
        return Ok(HashMap::new());
    }
    let parsed: Vec<uuid::Uuid> = uuids
        .iter()
        .map(|u| row_uuid(u))
        .collect::<ApiResult<_>>()?;
    let stores: Vec<PlanStore> = sqlx::query_as(
        r#"SELECT id, uuid::text AS uuid, code, name, "groupId" FROM store
            WHERE uuid = ANY($1) AND "companyId" = $2 AND "deletedAt" IS NULL AND "isActive" = true
            ORDER BY id"#,
    )
    .bind(&parsed)
    .bind(company_id)
    .fetch_all(conn)
    .await?;
    let by_uuid: HashMap<String, PlanStore> =
        stores.iter().map(|s| (s.uuid.clone(), s.clone())).collect();
    let missing: Vec<&String> = uuids.iter().filter(|u| !by_uuid.contains_key(*u)).collect();
    if !missing.is_empty() {
        return Err(ApiError::body(
            StatusCode::NOT_FOUND,
            json!({
                "code": "STORE_NOT_FOUND",
                "message": "Every store must exist, be active, and belong to this company",
                "details": { "storeIds": missing },
            }),
        ));
    }
    let out: Vec<Value> = stores
        .iter()
        .filter(|s| !is_in_scope(scope, s.group_id))
        .map(|s| store_ref(&s.uuid, &s.code, &s.name))
        .collect();
    if !out.is_empty() {
        return Err(ApiError::body(
            StatusCode::CONFLICT,
            json!({
                "code": "STORE_OUT_OF_SCOPE_FOR_SALES",
                "message": "Some stores are outside the region assigned to this salesperson",
                "details": { "stores": out },
            }),
        ));
    }
    Ok(by_uuid)
}

/// Satu baris `data` `createMany`.
#[derive(Debug, Clone)]
struct NewPlan {
    store_id: i32,
    visit_date: NaiveDate,
    sort_order: Option<i32>,
    note: Option<String>,
    visit_type_id: Option<i32>,
}

/// `createMany({ data, skipDuplicates })` + audit. Mengembalikan `count`.
async fn insert_plans(
    conn: &mut PgConnection,
    audit: &AuditHandle,
    company_id: i32,
    sales_user_id: i32,
    created_by: i32,
    data: &[NewPlan],
    skip_duplicates: bool,
) -> Result<u64, sqlx::Error> {
    let conflict = if skip_duplicates {
        " ON CONFLICT DO NOTHING"
    } else {
        ""
    };
    let n = sqlx::query(&format!(
        r#"INSERT INTO "storeVisitPlan" ("companyId", "userId", "storeId", "visitDate", "sortOrder", note,
                                         "createdById", "visitTypeId")
           SELECT $1, $2, t.s, t.d, t.so, t.n, $3, t.vt
             FROM UNNEST($4::int[], $5::date[], $6::int[], $7::text[], $8::int[]) AS t(s, d, so, n, vt){conflict}"#
    ))
    .bind(company_id)
    .bind(sales_user_id)
    .bind(created_by)
    .bind(data.iter().map(|d| d.store_id).collect::<Vec<_>>())
    .bind(data.iter().map(|d| d.visit_date).collect::<Vec<_>>())
    .bind(data.iter().map(|d| d.sort_order).collect::<Vec<_>>())
    .bind(data.iter().map(|d| d.note.clone()).collect::<Vec<_>>())
    .bind(data.iter().map(|d| d.visit_type_id).collect::<Vec<_>>())
    .execute(conn)
    .await?
    .rows_affected();
    let rows: Vec<Row> = data
        .iter()
        .map(|d| {
            Row::new()
                .with("companyId", company_id)
                .with("userId", sales_user_id)
                .with("storeId", d.store_id)
                .with(
                    "visitDate",
                    json!(JsDate(d.visit_date.and_time(chrono::NaiveTime::MIN))),
                )
                .with("sortOrder", json!(d.sort_order))
                .with("note", json!(d.note))
                .with("createdById", created_by)
                .with("visitTypeId", json!(d.visit_type_id))
        })
        .collect();
    audit
        .record_create_many(Model::StoreVisitPlan, &rows, n)
        .await;
    Ok(n)
}

/// `createBulk(userId, dto)` (§4.1, §5.5) → `{ created, skipped, dates, stores }`.
pub async fn create_bulk(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    dto: &CreatePlans,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let today = today_plan_date();
    let dates = resolve_requested_dates(dto)?;
    assert_writable_dates(&dates, &today)?;
    assert_batch_size(dates.len(), dto.stores.len())?;

    let sales_user_id = resolve_sales_id(&state.db, company_id, &dto.user_id).await?;
    let scope = resolve_store_scope(&state.db, sales_user_id, company_id, None).await?;
    let mut type_uuids: Vec<String> = Vec::new();
    for u in dto
        .visit_type_id
        .iter()
        .chain(dto.stores.iter().filter_map(|s| s.visit_type_id.as_ref()))
    {
        if !type_uuids.contains(u) {
            type_uuids.push(u.clone());
        }
    }
    let date_values: Vec<NaiveDate> = dates.iter().filter_map(|d| to_plan_date(d)).collect();

    let res: ApiResult<(u64, usize)> = async {
        let mut tx = state.db.begin().await?;
        let stores = resolve_stores(&mut tx, company_id, &scope, &dto.stores).await?;
        let types = resolve_visit_types(&mut tx, company_id, &type_uuids).await?;
        let needs_default =
            dto.visit_type_id.is_none() && dto.stores.iter().any(|s| s.visit_type_id.is_none());
        let default_type: Option<VisitMasterRow> = if needs_default {
            find_default(&mut tx, company_id).await?
        } else {
            None
        };
        let existing: Vec<(NaiveDate, i32)> = sqlx::query_as(
            r#"SELECT "visitDate", "storeId" FROM "storeVisitPlan"
                WHERE "userId" = $1 AND "deletedAt" IS NULL AND "visitDate" = ANY($2)"#,
        )
        .bind(sales_user_id)
        .bind(&date_values)
        .fetch_all(&mut *tx)
        .await?;
        let mut existing_by_date: HashMap<String, HashSet<i32>> = HashMap::new();
        for (d, s) in existing {
            existing_by_date
                .entry(from_plan_date(d))
                .or_default()
                .insert(s);
        }
        let store_ids: HashMap<String, i32> =
            stores.iter().map(|(u, s)| (u.clone(), s.id)).collect();
        assert_bulk_fits(
            &dates,
            &dto.stores,
            &store_ids,
            &existing_by_date,
            dto.skip_existing,
        )?;

        let resolve_type = |item: &PlanStoreItem| -> Option<i32> {
            match item.visit_type_id.as_ref().or(dto.visit_type_id.as_ref()) {
                Some(u) => types.get(u).map(|t| t.id),
                None => default_type.as_ref().map(|t| t.id),
            }
        };
        let data: Vec<NewPlan> = date_values
            .iter()
            .flat_map(|d| {
                dto.stores.iter().map(|item| NewPlan {
                    store_id: store_ids.get(&item.store_id).copied().unwrap_or_default(),
                    visit_date: *d,
                    sort_order: item.sort_order,
                    note: item.note.clone(),
                    visit_type_id: resolve_type(item),
                })
            })
            .collect();
        let n = match insert_plans(
            &mut tx,
            audit,
            company_id,
            sales_user_id,
            user_id,
            &data,
            dto.skip_existing,
        )
        .await
        {
            Ok(n) => n,
            Err(e) if mz_core::db::is_unique_violation(&e) => return Err(duplicate_plans(&[])),
            Err(e) => return Err(e.into()),
        };
        tx.commit().await?;
        Ok((n, data.len()))
    }
    .await;
    let (created, attempted) = match res {
        Ok(v) => v,
        Err(e) => {
            audit.discard();
            return Err(e);
        }
    };
    Ok(json!({
        "created": created,
        "skipped": u64::try_from(attempted).unwrap_or(0).saturating_sub(created),
        "dates": dates.len(),
        "stores": dto.stores.len(),
    }))
}

/// Baris hari itu yang dibaca `replaceDay`.
#[derive(Debug, Clone, sqlx::FromRow)]
struct DayRow {
    id: i32,
    #[sqlx(rename = "storeId")]
    store_id: i32,
    #[sqlx(rename = "sortOrder")]
    sort_order: Option<i32>,
    note: Option<String>,
    #[sqlx(rename = "visitTypeId")]
    visit_type_id: Option<i32>,
    store_uuid: String,
    store_code: String,
    store_name: String,
    visit_state: Option<String>,
}

/// Snapshot `updateMany` soft delete untuk audit.
fn soft_delete_data(now: chrono::NaiveDateTime) -> Row {
    Row::new()
        .with("deletedAt", json!(JsDate(now)))
        .with("updatedAt", json!(JsDate(now)))
}

/// `replaceDay(userId, dto)` (§4.1, §5.1) → `{ date, data }`.
pub async fn replace_day(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    dto: &ReplaceDay,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let today = today_plan_date();
    let date = assert_plan_date(Some(&dto.date), "date")?;
    assert_writable_dates(std::slice::from_ref(&date), &today)?;
    if dto.stores.len() > MAX_PLANS_PER_DAY {
        return Err(daily_limit(&[(date.clone(), dto.stores.len())]));
    }
    let sales_user_id = resolve_sales_id(&state.db, company_id, &dto.user_id).await?;
    let scope = resolve_store_scope(&state.db, sales_user_id, company_id, None).await?;
    let visit_date = to_plan_date(&date).unwrap_or_default();

    let res: ApiResult<()> = async {
        let mut tx = state.db.begin().await?;
        // §6.3 butir 2 — kunci seluruh baris hari itu SEBELUM membaca.
        sqlx::query(
            r#"SELECT id FROM "storeVisitPlan"
                WHERE "companyId" = $1 AND "userId" = $2 AND "visitDate" = $3 AND "deletedAt" IS NULL
                FOR UPDATE"#,
        )
        .bind(company_id)
        .bind(sales_user_id)
        .bind(visit_date)
        .execute(&mut *tx)
        .await?;
        let stores = resolve_stores(&mut tx, company_id, &scope, &dto.stores).await?;
        let wanted: HashMap<i32, &PlanStoreItem> = dto
            .stores
            .iter()
            .filter_map(|it| stores.get(&it.store_id).map(|s| (s.id, it)))
            .collect();
        let mut type_uuids: Vec<String> = Vec::new();
        for u in dto.stores.iter().filter_map(|s| s.visit_type_id.as_ref()) {
            if !type_uuids.contains(u) {
                type_uuids.push(u.clone());
            }
        }
        let types = resolve_visit_types(&mut tx, company_id, &type_uuids).await?;
        let existing: Vec<DayRow> = sqlx::query_as(
            r#"SELECT p.id, p."storeId", p."sortOrder", p.note, p."visitTypeId",
                      s.uuid::text AS store_uuid, s.code AS store_code, s.name AS store_name,
                      v.state::text AS visit_state
                 FROM "storeVisitPlan" p JOIN store s ON s.id = p."storeId"
                 LEFT JOIN "storeVisit" v ON v."planId" = p.id
                WHERE p."companyId" = $1 AND p."userId" = $2 AND p."visitDate" = $3 AND p."deletedAt" IS NULL
                ORDER BY p.id"#,
        )
        .bind(company_id)
        .bind(sales_user_id)
        .bind(visit_date)
        .fetch_all(&mut *tx)
        .await?;
        let fulfilled = |r: &DayRow| is_fulfilled(r.visit_state.as_deref());
        let blocked: Vec<Value> = existing
            .iter()
            .filter(|r| fulfilled(r) && !wanted.contains_key(&r.store_id))
            .map(|r| store_ref(&r.store_uuid, &r.store_code, &r.store_name))
            .collect();
        if !blocked.is_empty() {
            return Err(already_fulfilled(blocked));
        }
        let keep: Vec<&DayRow> = existing.iter().filter(|r| fulfilled(r)).collect();
        let locked: Vec<Value> = keep
            .iter()
            .filter(|r| {
                wanted
                    .get(&r.store_id)
                    .and_then(|it| it.visit_type_id.as_ref())
                    .is_some_and(|u| types.get(u).map(|t| t.id) != r.visit_type_id)
            })
            .map(|r| store_ref(&r.store_uuid, &r.store_code, &r.store_name))
            .collect();
        if !locked.is_empty() {
            return Err(visit_type_locked(locked));
        }
        let removable: Vec<i32> = existing.iter().filter(|r| !fulfilled(r)).map(|r| r.id).collect();
        if !removable.is_empty() {
            let now = now_naive();
            let pre = audit
                .pre_read_many(Model::StoreVisitPlan, Lookup::Ids(removable.clone()))
                .await;
            let n = sqlx::query(
                r#"UPDATE "storeVisitPlan" SET "deletedAt" = $2, "updatedAt" = $2 WHERE id = ANY($1)"#,
            )
            .bind(&removable)
            .bind(now)
            .execute(&mut *tx)
            .await?
            .rows_affected();
            audit
                .record_update_many(
                    Model::StoreVisitPlan,
                    pre,
                    n,
                    &soft_delete_data(now),
                    json!({ "id": { "in": removable } }),
                )
                .await;
        }
        for r in &keep {
            let Some(item) = wanted.get(&r.store_id) else {
                continue;
            };
            if r.sort_order == item.sort_order && r.note == item.note {
                continue;
            }
            let pre = audit.pre_read(Model::StoreVisitPlan, Lookup::Id(r.id)).await;
            sqlx::query(
                r#"UPDATE "storeVisitPlan" SET "sortOrder" = $2, note = $3, "updatedAt" = $4 WHERE id = $1"#,
            )
            .bind(r.id)
            .bind(item.sort_order)
            .bind(&item.note)
            .bind(now_naive())
            .execute(&mut *tx)
            .await?;
            if let Some(row) = audit
                .result_row(&mut *tx, Model::StoreVisitPlan, Lookup::Id(r.id))
                .await
            {
                audit.record_update(Model::StoreVisitPlan, pre, &row).await;
            }
        }
        let kept: HashSet<i32> = keep.iter().map(|r| r.store_id).collect();
        let prev_type: HashMap<i32, Option<i32>> =
            existing.iter().map(|r| (r.store_id, r.visit_type_id)).collect();
        let not_kept: Vec<(&PlanStoreItem, i32)> = dto
            .stores
            .iter()
            .filter_map(|it| stores.get(&it.store_id).map(|s| (it, s.id)))
            .filter(|(_, sid)| !kept.contains(sid))
            .collect();
        let needs_default = not_kept
            .iter()
            .any(|(it, sid)| it.visit_type_id.is_none() && !prev_type.contains_key(sid));
        let default_type: Option<VisitMasterRow> = if needs_default {
            find_default(&mut tx, company_id).await?
        } else {
            None
        };
        let data: Vec<NewPlan> = not_kept
            .iter()
            .map(|(it, sid)| NewPlan {
                store_id: *sid,
                visit_date,
                sort_order: it.sort_order,
                note: it.note.clone(),
                visit_type_id: match &it.visit_type_id {
                    Some(u) => types.get(u).map(|t| t.id),
                    None => match prev_type.get(sid) {
                        Some(prev) => *prev,
                        None => default_type.as_ref().map(|t| t.id),
                    },
                },
            })
            .collect();
        if !data.is_empty() {
            insert_plans(&mut tx, audit, company_id, sales_user_id, user_id, &data, true).await?;
        }
        tx.commit().await?;
        Ok(())
    }
    .await;
    if let Err(e) = res {
        audit.discard();
        return Err(e);
    }

    let rows: Vec<PlanRow> = sqlx::query_as(&format!(
        r#"{PLAN_SELECT} WHERE p."companyId" = $1 AND p."userId" = $2 AND p."visitDate" = $3 AND p."deletedAt" IS NULL
           ORDER BY p."sortOrder" ASC NULLS LAST, p.id ASC"#
    ))
    .bind(company_id)
    .bind(sales_user_id)
    .bind(visit_date)
    .fetch_all(&state.db)
    .await?;
    let sales = load_sales(&state.db, company_id, &[sales_user_id]).await?;
    Ok(json!({
        "date": date,
        "data": rows.iter().map(|r| to_plan(r, &today, &sales)).collect::<Vec<_>>(),
    }))
}

/// Kolom skalar `planSelect` (bentuk `after` audit `update({ select })`).
const PLAN_SELECT_SCALARS: [&str; 6] = [
    "uuid",
    "userId",
    "visitDate",
    "sortOrder",
    "note",
    "createdAt",
];

async fn record_plan_update(
    audit: &AuditHandle,
    exec: &mut PgConnection,
    pre: mz_audit::PreRead,
    id: i32,
) {
    if let Some(full) = audit
        .result_row(&mut *exec, Model::StoreVisitPlan, Lookup::Id(id))
        .await
    {
        let selected: Row = full
            .iter()
            .filter(|(k, _)| PLAN_SELECT_SCALARS.contains(k))
            .map(|(k, v)| (k.to_owned(), v.clone()))
            .collect();
        audit
            .record_update(Model::StoreVisitPlan, pre, &selected)
            .await;
    }
}

fn push_patch(qb: &mut QueryBuilder<'_, Postgres>, dto: &UpdatePlan) {
    match &dto.sort_order {
        Field::Val(v) => {
            qb.push(r#", "sortOrder" = "#).push_bind(*v);
        }
        Field::Null => {
            qb.push(r#", "sortOrder" = NULL"#);
        }
        Field::Absent => {}
    }
    match &dto.note {
        Field::Val(v) => {
            qb.push(", note = ").push_bind(v.clone());
        }
        Field::Null => {
            qb.push(", note = NULL");
        }
        Field::Absent => {}
    }
}

async fn fetch_plan(db: &PgPool, id: i32) -> ApiResult<PlanRow> {
    Ok(sqlx::query_as(&format!(r#"{PLAN_SELECT} WHERE p.id = $1"#))
        .bind(id)
        .fetch_one(db)
        .await?)
}

/// `update(userId, uuid, dto)` (§4.1, §6.4).
pub async fn update(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    uuid: &str,
    dto: &UpdatePlan,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let today = today_plan_date();
    let u = row_uuid(uuid)?;

    let Some(type_uuid) = dto.visit_type_id.clone() else {
        let plan: Option<(i32, i32)> = sqlx::query_as(
            r#"SELECT id, "userId" FROM "storeVisitPlan" WHERE uuid = $1 AND "companyId" = $2 AND "deletedAt" IS NULL LIMIT 1"#,
        )
        .bind(u)
        .bind(company_id)
        .fetch_optional(&state.db)
        .await?;
        let (id, plan_user) = plan.ok_or_else(plan_not_found)?;
        let pre = audit.pre_read(Model::StoreVisitPlan, Lookup::Id(id)).await;
        let mut qb = QueryBuilder::<Postgres>::new(r#"UPDATE "storeVisitPlan" SET "updatedAt" = "#);
        qb.push_bind(now_naive());
        push_patch(&mut qb, dto);
        qb.push(" WHERE id = ").push_bind(id);
        qb.build().execute(&state.db).await?;
        let mut conn = state.db.acquire().await?;
        record_plan_update(audit, &mut conn, pre, id).await;
        drop(conn);
        let row = fetch_plan(&state.db, id).await?;
        let sales = load_sales(&state.db, company_id, &[plan_user]).await?;
        return Ok(to_plan(&row, &today, &sales));
    };

    let res: ApiResult<i32> = async {
        let mut tx = state.db.begin().await?;
        let locked: Option<(i32, i32, Option<i32>)> = sqlx::query_as(
            r#"SELECT id, "userId", "visitTypeId" FROM "storeVisitPlan"
                WHERE uuid = $1 AND "companyId" = $2 AND "deletedAt" IS NULL FOR UPDATE"#,
        )
        .bind(u)
        .bind(company_id)
        .fetch_optional(&mut *tx)
        .await?;
        let (id, _, current_type) = locked.ok_or_else(plan_not_found)?;
        let types = resolve_visit_types(&mut tx, company_id, std::slice::from_ref(&type_uuid)).await?;
        let new_type = types.get(&type_uuid).map(|t| t.id);
        if new_type != current_type {
            let claimed: Option<i32> = sqlx::query_scalar(
                r#"SELECT id FROM "storeVisit" WHERE "planId" = $1 AND state <> 'cancelled'::"StoreVisitState" LIMIT 1"#,
            )
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
            if claimed.is_some() {
                let (su, sc, sn): (String, String, String) = sqlx::query_as(
                    r#"SELECT s.uuid::text, s.code, s.name FROM "storeVisitPlan" p JOIN store s ON s.id = p."storeId" WHERE p.id = $1"#,
                )
                .bind(id)
                .fetch_one(&mut *tx)
                .await?;
                return Err(visit_type_locked(vec![store_ref(&su, &sc, &sn)]));
            }
        }
        let pre = audit.pre_read(Model::StoreVisitPlan, Lookup::Id(id)).await;
        let mut qb = QueryBuilder::<Postgres>::new(r#"UPDATE "storeVisitPlan" SET "updatedAt" = "#);
        qb.push_bind(now_naive());
        push_patch(&mut qb, dto);
        qb.push(r#", "visitTypeId" = "#).push_bind(new_type);
        qb.push(" WHERE id = ").push_bind(id);
        qb.build().execute(&mut *tx).await?;
        record_plan_update(audit, &mut tx, pre, id).await;
        tx.commit().await?;
        Ok(id)
    }
    .await;
    let id = match res {
        Ok(id) => id,
        Err(e) => {
            audit.discard();
            return Err(e);
        }
    };
    let row = fetch_plan(&state.db, id).await?;
    let sales = load_sales(&state.db, company_id, &[row.user_id]).await?;
    Ok(to_plan(&row, &today, &sales))
}

/// `remove(userId, uuid)` (§0.8, §5.2) — soft delete, hanya yang belum tercapai.
pub async fn remove(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    uuid: &str,
) -> ApiResult<()> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let u = row_uuid(uuid)?;
    let plan: Option<(i32, String, String, String, Option<String>)> = sqlx::query_as(
        r#"SELECT p.id, s.uuid::text, s.code, s.name, v.state::text
             FROM "storeVisitPlan" p JOIN store s ON s.id = p."storeId"
             LEFT JOIN "storeVisit" v ON v."planId" = p.id
            WHERE p.uuid = $1 AND p."companyId" = $2 AND p."deletedAt" IS NULL LIMIT 1"#,
    )
    .bind(u)
    .bind(company_id)
    .fetch_optional(&state.db)
    .await?;
    let (id, su, sc, sn, vstate) = plan.ok_or_else(plan_not_found)?;
    if is_fulfilled(vstate.as_deref()) {
        return Err(already_fulfilled(vec![store_ref(&su, &sc, &sn)]));
    }
    let lookup = Lookup::filter(move |qb| {
        qb.push("id = ").push_bind(id);
        qb.push(r#" AND "deletedAt" IS NULL"#);
    });
    let pre = audit.pre_read_many(Model::StoreVisitPlan, lookup).await;
    let now = now_naive();
    let n = sqlx::query(
        r#"UPDATE "storeVisitPlan" SET "deletedAt" = $2, "updatedAt" = $2 WHERE id = $1 AND "deletedAt" IS NULL"#,
    )
    .bind(id)
    .bind(now)
    .execute(&state.db)
    .await?
    .rows_affected();
    audit
        .record_update_many(
            Model::StoreVisitPlan,
            pre,
            n,
            &soft_delete_data(now),
            json!({ "id": id, "deletedAt": null }),
        )
        .await;
    if n == 0 {
        return Err(plan_not_found());
    }
    Ok(())
}

/// `getPlan(userId, uuid)`.
pub async fn get_plan(state: &AppState, user_id: i32, uuid: &str) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let today = today_plan_date();
    let u = row_uuid(uuid)?;
    let row: Option<PlanRow> = sqlx::query_as(&format!(
        r#"{PLAN_SELECT} WHERE p.uuid = $1 AND p."companyId" = $2 AND p."deletedAt" IS NULL LIMIT 1"#
    ))
    .bind(u)
    .bind(company_id)
    .fetch_optional(&state.db)
    .await?;
    let row = row.ok_or_else(store_visit_plan_not_found)?;
    let sales = load_sales(&state.db, company_id, &[row.user_id]).await?;
    Ok(to_plan(&row, &today, &sales))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row() -> PlanRow {
        PlanRow {
            id: 1,
            uuid: "p".into(),
            user_id: 7,
            visit_date: to_plan_date("2026-08-20").unwrap(),
            sort_order: None,
            note: Some("tagih".into()),
            created_at: JsDate::from_millis(1_786_000_000_000).unwrap(),
            vt_uuid: None,
            vt_code: None,
            vt_name: None,
            vt_requires_order: None,
            vt_requires_photo: None,
            vt_requires_gps: None,
            vt_is_active: None,
            store_uuid: "s".into(),
            store_code: "S1".into(),
            store_name: "Toko".into(),
            store_is_active: true,
            store_deleted: false,
            group_uuid: None,
            group_code: None,
            group_name: None,
            created_by_uuid: Some("a".into()),
            created_by_name: Some("Admin".into()),
            visit_uuid: Some("v".into()),
            visit_number: Some("SV-1".into()),
            visit_check_in_at: JsDate::from_millis(5),
            visit_status: None,
            visit_state: Some("cancelled".into()),
        }
    }

    /// "kunjungan yang dibatalkan tidak dihitung tercapai" + bentuk serializer §4.1.
    #[test]
    fn to_plan_shape() {
        let sales: HashMap<i32, SalesInfo> = [(
            7,
            SalesInfo {
                uuid: "u7".into(),
                name: "Budi".into(),
                is_active_member: true,
            },
        )]
        .into_iter()
        .collect();
        let v = to_plan(&row(), "2026-08-21", &sales);
        assert_eq!(
            v,
            json!({
                "id": "p", "visitDate": "2026-08-20", "sortOrder": null, "note": "tagih", "visitType": null,
                "sales": {"id": "u7", "name": "Budi", "isActiveMember": true},
                "store": {"id": "s", "code": "S1", "name": "Toko", "isActive": true, "isDeleted": false, "group": null},
                "status": "missed", "visit": null,
                "createdBy": {"id": "a", "name": "Admin"}, "createdAt": 1_786_000_000_000_i64
            })
        );
        let mut r = row();
        r.visit_state = Some("completed".into());
        r.vt_uuid = Some("t".into());
        r.vt_code = Some("REG".into());
        r.vt_name = Some("Reguler".into());
        r.vt_requires_order = Some(false);
        r.vt_requires_photo = Some(true);
        r.vt_requires_gps = Some(true);
        r.vt_is_active = Some(true);
        let v = to_plan(&r, "2026-08-21", &HashMap::new());
        assert_eq!(v["status"], "fulfilled");
        assert_eq!(
            v["visit"],
            json!({"id": "v", "visitNumber": "SV-1", "checkInAt": 5, "status": null})
        );
        assert_eq!(
            v["sales"],
            json!({"id": null, "name": null, "isActiveMember": false})
        );
        assert_eq!(v["visitType"]["requiresPhoto"], json!(true));
    }
}
