//! Port `src/store-visits/admin-store-visits.service.ts` (§7.4–§7.6): daftar + KPI, detail,
//! proxy/URL foto, export. Semua dibatasi company caller.

use std::collections::HashMap;

use axum::http::StatusCode;
use chrono::{NaiveDate, Utc};
use chrono_tz::Tz;
use mz_core::company::require_company_id;
use mz_core::jsfmt::{collate_id, num};
use mz_core::time::JsDate;
use mz_core::tz::zone_or_utc;
use mz_core::{ApiError, ApiResult, AppState};
use serde_json::{Map, Value, json};
use sqlx::{PgPool, Postgres, QueryBuilder};

use super::dto::VisitListQuery;
use super::range::{DateWindow, clamp_to_history_window, resolve_date_range};
use crate::enums::{GpsValidation, StoreVisitState, StoreVisitStatus};
use crate::photos::{photos_of_visits, serialize_admin_photos, summarize_photos};
use crate::plan_dates::from_plan_date;

// ───────────────────────── filter (buildBaseWhere + resolveWhere) ─────────────────────────

/// `FROM` bersama: kunjungan + toko + sales (relasi wajib).
pub const VISIT_FROM: &str = r#" FROM "storeVisit" v
  JOIN store s ON s.id = v."storeId"
  JOIN users u ON u.id = v."userId""#;

/// Klausa `where` kunjungan admin, milik sendiri (bisa dibawa ke stream export).
#[derive(Debug, Clone, Default)]
pub struct VisitFilter {
    pub company_id: i32,
    pub window: DateWindow,
    pub sales_id: Option<Vec<String>>,
    pub store_id: Option<Vec<String>>,
    pub gps_validation: Option<Vec<GpsValidation>>,
    pub search: Option<String>,
    pub group_id: Option<Vec<String>>,
    pub unassigned: bool,
    /// Hanya `listWhere` (summary mengabaikannya).
    pub state: Option<StoreVisitState>,
    pub status: Option<Vec<StoreVisitStatus>>,
    pub planned: Option<bool>,
}

impl VisitFilter {
    /// `buildBaseWhere(...)` (+ `state`/`status`/`planned` bila `list`). Menulis ` WHERE ...`.
    pub fn push_where(&self, qb: &mut QueryBuilder<'_, Postgres>, list: bool) {
        qb.push(r#" WHERE v."companyId" = "#)
            .push_bind(self.company_id);
        if let Some(g) = self.window.gte {
            qb.push(r#" AND v."checkInAt" >= "#).push_bind(g);
        }
        if let Some(l) = self.window.lte {
            qb.push(r#" AND v."checkInAt" <= "#).push_bind(l);
        }
        if let Some(ids) = self.sales_id.as_ref().filter(|v| !v.is_empty()) {
            qb.push(" AND u.uuid = ANY(")
                .push_bind(ids.clone())
                .push("::uuid[])");
        }
        if let Some(ids) = self.store_id.as_ref().filter(|v| !v.is_empty()) {
            qb.push(" AND s.uuid = ANY(")
                .push_bind(ids.clone())
                .push("::uuid[])");
        }
        if let Some(g) = self.gps_validation.as_ref().filter(|v| !v.is_empty()) {
            let vals: Vec<String> = g.iter().map(|x| x.as_str().to_owned()).collect();
            qb.push(r#" AND v."checkInGpsValidation"::text = ANY("#)
                .push_bind(vals)
                .push(")");
        }
        if let Some(s) = self.search.as_deref().filter(|s| !s.is_empty()) {
            // Prisma `contains` + `insensitive` = ILIKE tanpa escape `%`/`_` (sama dengan Node).
            qb.push(" AND (v.note ILIKE '%' || ")
                .push_bind(s.to_owned())
                .push(" || '%' OR s.name ILIKE '%' || ")
                .push_bind(s.to_owned())
                .push(" || '%' OR s.code ILIKE '%' || ")
                .push_bind(s.to_owned())
                .push(" || '%' OR u.name ILIKE '%' || ")
                .push_bind(s.to_owned())
                .push(" || '%')");
        }
        let groups = self.group_id.as_ref().filter(|v| !v.is_empty());
        let group_in = |qb: &mut QueryBuilder<'_, Postgres>, ids: &Vec<String>| {
            qb.push(r#"v."storeGroupId" IN (SELECT id FROM "storeGroup" WHERE uuid = ANY("#)
                .push_bind(ids.clone())
                .push("::uuid[]))");
        };
        match (groups, self.unassigned) {
            (Some(ids), true) => {
                qb.push(" AND (");
                group_in(qb, ids);
                qb.push(r#" OR v."storeGroupId" IS NULL)"#);
            }
            (Some(ids), false) => {
                qb.push(" AND ");
                group_in(qb, ids);
            }
            (None, true) => {
                qb.push(r#" AND v."storeGroupId" IS NULL"#);
            }
            (None, false) => {}
        }
        if !list {
            return;
        }
        if let Some(st) = self.state {
            qb.push(" AND v.state::text = ").push_bind(st.as_str());
        }
        if let Some(s) = self.status.as_ref().filter(|v| !v.is_empty()) {
            let vals: Vec<String> = s.iter().map(|x| x.as_str().to_owned()).collect();
            qb.push(" AND v.status::text = ANY(")
                .push_bind(vals)
                .push(")");
        }
        match self.planned {
            Some(true) => {
                qb.push(r#" AND v."planId" IS NOT NULL"#);
            }
            Some(false) => {
                qb.push(r#" AND v."planId" IS NULL"#);
            }
            None => {}
        }
    }
}

/// `getTimeZone(companyId)`: 403 tanpa baris company; kosong → `UTC`.
async fn get_time_zone(db: &PgPool, company_id: i32) -> ApiResult<String> {
    let tz: Option<String> = sqlx::query_scalar(r#"SELECT "timeZone" FROM company WHERE id = $1"#)
        .bind(company_id)
        .fetch_optional(db)
        .await?;
    let tz = tz.ok_or_else(|| ApiError::forbidden("No company found for user"))?;
    Ok(if tz.is_empty() { "UTC".into() } else { tz })
}

/// Hasil `resolveWhere`.
pub struct Resolved {
    pub company_id: i32,
    pub time_zone: String,
    pub filter: VisitFilter,
}

/// `resolveWhere(userId, query)`: company → zona → rentang → klem riwayat paket.
pub async fn resolve_where(
    state: &AppState,
    user_id: i32,
    q: &VisitListQuery,
) -> ApiResult<Resolved> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let time_zone = get_time_zone(&state.db, company_id).await?;
    let tz: Tz = zone_or_utc(Some(&time_zone));
    let now = Utc::now();
    let window = resolve_date_range(
        q.start_date.as_deref(),
        q.end_date.as_deref(),
        q.range,
        tz,
        now,
    )?;
    let days = mz_sales_geo::plan::resolve_visit_history_days(&state.db, company_id).await?;
    let window = clamp_to_history_window(window, days, tz, now);
    Ok(Resolved {
        company_id,
        time_zone,
        filter: VisitFilter {
            company_id,
            window,
            sales_id: q.sales_id.clone(),
            store_id: q.store_id.clone(),
            gps_validation: q.gps_validation.clone(),
            search: q.search.clone(),
            group_id: q.group_id.clone(),
            unassigned: q.unassigned == Some(true),
            state: q.state,
            status: q.status.clone(),
            planned: q.planned,
        },
    })
}

// ───────────────────────── baris kunjungan ─────────────────────────

/// Kolom `SELECT` baris kunjungan admin (list/detail/export).
pub const VISIT_COLUMNS: &str = r#"SELECT v.id, v.uuid::text AS uuid, v."companyId", v."visitNumber",
       v.state::text AS state, v.status::text AS status, v.note, v.tags, v."durationSeconds",
       v."checkInAt", v."checkInClientAt", v."checkInLat", v."checkInLng", v."checkInAccuracy",
       v."checkInDistance", v."checkInMethod"::text AS "checkInMethod",
       v."checkInGpsValidation"::text AS "checkInGpsValidation", v."checkInIsMock",
       v."checkInIsOverride", v."checkInOverrideReason", v."hasGpsIssue", v."checkOutAt",
       v."checkOutClientAt", v."checkOutLat", v."checkOutLng", v."checkOutAccuracy",
       v."checkOutDistance", v."checkOutGpsValidation"::text AS "checkOutGpsValidation",
       v."checkOutIsMock", v."createdAt", v."planId",
       s.uuid::text AS store_uuid, s.code AS store_code, s.name AS store_name,
       s."ownerName" AS store_owner_name,
       u.uuid::text AS sales_uuid, u.name AS sales_name, u.username AS sales_username,
       vt.uuid::text AS vt_uuid, vt.code AS vt_code, vt.name AS vt_name,
       vt."requiresOrder" AS vt_requires_order, vt."requiresPhoto" AS vt_requires_photo,
       vt."requiresGps" AS vt_requires_gps, vt."isActive" AS vt_is_active,
       g.uuid::text AS group_uuid, g.code AS group_code, g.name AS group_name,
       p.uuid::text AS plan_uuid, p."visitDate" AS plan_visit_date, p.note AS plan_note"#;

/// Join opsional untuk [`VISIT_COLUMNS`] (sesudah [`VISIT_FROM`]).
pub const VISIT_OPTIONAL_JOINS: &str = r#"
  LEFT JOIN "visitType" vt ON vt.id = v."visitTypeId"
  LEFT JOIN "storeGroup" g ON g.id = v."storeGroupId"
  LEFT JOIN "storeVisitPlan" p ON p.id = v."planId""#;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct VisitRow {
    pub id: i32,
    pub uuid: String,
    #[sqlx(rename = "companyId")]
    pub company_id: i32,
    #[sqlx(rename = "visitNumber")]
    pub visit_number: String,
    pub state: String,
    pub status: Option<String>,
    pub note: Option<String>,
    pub tags: Vec<String>,
    #[sqlx(rename = "durationSeconds")]
    pub duration_seconds: Option<i32>,
    #[sqlx(rename = "checkInAt")]
    pub check_in_at: JsDate,
    #[sqlx(rename = "checkInClientAt")]
    pub check_in_client_at: Option<JsDate>,
    #[sqlx(rename = "checkInLat")]
    pub check_in_lat: f64,
    #[sqlx(rename = "checkInLng")]
    pub check_in_lng: f64,
    #[sqlx(rename = "checkInAccuracy")]
    pub check_in_accuracy: f64,
    #[sqlx(rename = "checkInDistance")]
    pub check_in_distance: i32,
    #[sqlx(rename = "checkInMethod")]
    pub check_in_method: String,
    #[sqlx(rename = "checkInGpsValidation")]
    pub check_in_gps_validation: String,
    #[sqlx(rename = "checkInIsMock")]
    pub check_in_is_mock: bool,
    #[sqlx(rename = "checkInIsOverride")]
    pub check_in_is_override: bool,
    #[sqlx(rename = "checkInOverrideReason")]
    pub check_in_override_reason: Option<String>,
    #[sqlx(rename = "hasGpsIssue")]
    pub has_gps_issue: bool,
    #[sqlx(rename = "checkOutAt")]
    pub check_out_at: Option<JsDate>,
    #[sqlx(rename = "checkOutClientAt")]
    pub check_out_client_at: Option<JsDate>,
    #[sqlx(rename = "checkOutLat")]
    pub check_out_lat: Option<f64>,
    #[sqlx(rename = "checkOutLng")]
    pub check_out_lng: Option<f64>,
    #[sqlx(rename = "checkOutAccuracy")]
    pub check_out_accuracy: Option<f64>,
    #[sqlx(rename = "checkOutDistance")]
    pub check_out_distance: Option<i32>,
    #[sqlx(rename = "checkOutGpsValidation")]
    pub check_out_gps_validation: Option<String>,
    #[sqlx(rename = "checkOutIsMock")]
    pub check_out_is_mock: Option<bool>,
    #[sqlx(rename = "createdAt")]
    pub created_at: JsDate,
    #[sqlx(rename = "planId")]
    pub plan_id: Option<i32>,
    pub store_uuid: String,
    pub store_code: String,
    pub store_name: String,
    pub store_owner_name: String,
    pub sales_uuid: String,
    pub sales_name: String,
    pub sales_username: String,
    pub vt_uuid: Option<String>,
    pub vt_code: Option<String>,
    pub vt_name: Option<String>,
    pub vt_requires_order: Option<bool>,
    pub vt_requires_photo: Option<bool>,
    pub vt_requires_gps: Option<bool>,
    pub vt_is_active: Option<bool>,
    pub group_uuid: Option<String>,
    pub group_code: Option<String>,
    pub group_name: Option<String>,
    pub plan_uuid: Option<String>,
    pub plan_visit_date: Option<NaiveDate>,
    pub plan_note: Option<String>,
}

fn ms(d: JsDate) -> Value {
    Value::from(d.timestamp_millis())
}

fn opt_ms(d: Option<JsDate>) -> Value {
    d.map_or(Value::Null, ms)
}

fn opt_num(x: Option<f64>) -> Value {
    x.map_or(Value::Null, num)
}

impl VisitRow {
    /// `toVisitTypeRef(visit.visitType)` atau `null`.
    pub fn visit_type_ref(&self) -> Value {
        match (&self.vt_uuid, &self.vt_code, &self.vt_name) {
            (Some(id), Some(code), Some(name)) => json!({
                "id": id,
                "code": code,
                "name": name,
                "requiresOrder": self.vt_requires_order.unwrap_or(false),
                "requiresPhoto": self.vt_requires_photo.unwrap_or(false),
                "requiresGps": self.vt_requires_gps.unwrap_or(true),
                "isActive": self.vt_is_active.unwrap_or(true),
            }),
            _ => Value::Null,
        }
    }

    fn sales_ref(&self) -> Value {
        json!({ "id": self.sales_uuid, "name": self.sales_name, "username": self.sales_username })
    }

    fn store_ref(&self) -> Value {
        json!({
            "id": self.store_uuid,
            "code": self.store_code,
            "name": self.store_name,
            "ownerName": self.store_owner_name,
        })
    }

    fn check_in(&self) -> Value {
        json!({
            "at": ms(self.check_in_at),
            "clientAt": opt_ms(self.check_in_client_at),
            "lat": num(self.check_in_lat),
            "lng": num(self.check_in_lng),
            "accuracyMeters": num(self.check_in_accuracy),
            "distanceMeters": self.check_in_distance,
            "method": self.check_in_method,
            "gpsValidation": self.check_in_gps_validation,
            "isMock": self.check_in_is_mock,
            "isOverride": self.check_in_is_override,
            "overrideReason": self.check_in_override_reason,
        })
    }

    /// `toAdminVisitListItem(visit)` — `photos` = `(kind, isConfirmed)` milik kunjungan ini.
    pub fn to_list_item<'a>(&self, photos: impl IntoIterator<Item = (&'a str, bool)>) -> Value {
        let (photos_count, photo_kinds) = summarize_photos(photos);
        json!({
            "id": self.uuid,
            "visitNumber": self.visit_number,
            "sales": self.sales_ref(),
            "store": self.store_ref(),
            "state": self.state,
            "status": self.status,
            "visitType": self.visit_type_ref(),
            "checkIn": self.check_in(),
            "checkOut": self.check_out_at.map_or(Value::Null, |at| json!({
                "at": ms(at),
                "distanceMeters": self.check_out_distance,
                "gpsValidation": self.check_out_gps_validation,
                "isMock": self.check_out_is_mock.unwrap_or(false),
            })),
            "durationSeconds": self.duration_seconds,
            "note": self.note,
            "tags": self.tags,
            "photosCount": photos_count,
            "photoKinds": photo_kinds,
            "hasGpsIssue": self.has_gps_issue,
            "isPlanned": self.plan_id.is_some(),
            "createdAt": ms(self.created_at),
        })
    }

    /// Bentuk detail `findOne` (tanpa `photos`, disisipkan pemanggil).
    pub fn to_detail(&self, photos: Vec<Value>) -> Value {
        let group = match (&self.group_uuid, &self.group_code, &self.group_name) {
            (Some(id), Some(code), Some(name)) => json!({ "id": id, "code": code, "name": name }),
            _ => Value::Null,
        };
        let plan = match (&self.plan_uuid, self.plan_visit_date) {
            (Some(id), Some(d)) => json!({
                "id": id,
                "visitDate": from_plan_date(d),
                "note": self.plan_note,
            }),
            _ => Value::Null,
        };
        json!({
            "id": self.uuid,
            "visitNumber": self.visit_number,
            "sales": self.sales_ref(),
            "store": self.store_ref(),
            "group": group,
            "state": self.state,
            "status": self.status,
            "visitType": self.visit_type_ref(),
            "checkIn": self.check_in(),
            "checkOut": self.check_out_at.map_or(Value::Null, |at| json!({
                "at": ms(at),
                "clientAt": opt_ms(self.check_out_client_at),
                "lat": opt_num(self.check_out_lat),
                "lng": opt_num(self.check_out_lng),
                "accuracyMeters": opt_num(self.check_out_accuracy),
                "distanceMeters": self.check_out_distance,
                "gpsValidation": self.check_out_gps_validation,
                "isMock": self.check_out_is_mock.unwrap_or(false),
            })),
            "durationSeconds": self.duration_seconds,
            "note": self.note,
            "tags": self.tags,
            "hasGpsIssue": self.has_gps_issue,
            "plan": plan,
            "photos": photos,
            "createdAt": ms(self.created_at),
        })
    }
}

// ───────────────────────── KPI (computeSummary + computeGroupBreakdown) ─────────────────────────

/// Satu baris `groupBy([storeGroupId, state, status, hasGpsIssue])` + agregat durasi (completed).
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct KpiRow {
    #[sqlx(rename = "storeGroupId")]
    pub store_group_id: Option<i32>,
    pub state: String,
    pub status: Option<String>,
    #[sqlx(rename = "hasGpsIssue")]
    pub has_gps_issue: bool,
    pub count: i64,
    /// `SUM(durationSeconds)` baris completed.
    pub duration_sum: Option<i64>,
    /// `COUNT(durationSeconds)` baris completed.
    pub duration_count: i64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Bucket {
    visits_in_range: i64,
    completed: i64,
    success: i64,
    gps_issues: i64,
}

impl Bucket {
    fn add(&mut self, r: &KpiRow) {
        if r.state == "active" || r.state == "completed" {
            self.visits_in_range += r.count;
        }
        if r.state == "completed" {
            self.completed += r.count;
        }
        if r.status.as_deref() == Some("success") {
            self.success += r.count;
        }
        if r.has_gps_issue {
            self.gps_issues += r.count;
        }
    }
}

/// `Math.round(x)` JS (setengah ke atas).
pub fn js_round(x: f64) -> i64 {
    #[allow(clippy::cast_possible_truncation)]
    {
        (x + 0.5).floor() as i64
    }
}

/// `completed === 0 ? 0 : Math.round(success / completed * 100)`.
pub fn success_rate(success: i64, completed: i64) -> i64 {
    if completed == 0 {
        return 0;
    }
    #[allow(clippy::cast_precision_loss)]
    js_round(success as f64 / completed as f64 * 100.0)
}

fn bucket_row(group: Value, b: &Bucket) -> Value {
    json!({
        "group": group,
        "visitsInRange": b.visits_in_range,
        "completedVisits": b.completed,
        "successVisits": b.success,
        "successRate": success_rate(b.success, b.completed),
        "gpsIssues": b.gps_issues,
    })
}

/// Region untuk `byGroup`: id → (uuid, code, name).
pub type GroupRefs = HashMap<i32, (String, String, String)>;

/// Bagian murni `computeSummary` + `computeGroupBreakdown`. `byGroup`: region bernama urut
/// `name` (`localeCompare`), lalu bucket `group: null` paling akhir bila ada; region yang tidak
/// ditemukan dibuang.
pub fn compute_summary(rows: &[KpiRow], groups: &GroupRefs) -> Value {
    let (mut active, mut completed, mut cancelled, mut success, mut gps) =
        (0i64, 0i64, 0i64, 0i64, 0i64);
    let (mut dur_sum, mut dur_count) = (0i64, 0i64);
    let mut order: Vec<Option<i32>> = Vec::new();
    let mut buckets: HashMap<Option<i32>, Bucket> = HashMap::new();
    for r in rows {
        match r.state.as_str() {
            "active" => active += r.count,
            "completed" => completed += r.count,
            "cancelled" => cancelled += r.count,
            _ => {}
        }
        if r.status.as_deref() == Some("success") {
            success += r.count;
        }
        if r.has_gps_issue {
            gps += r.count;
        }
        if r.state == "completed" {
            dur_sum += r.duration_sum.unwrap_or(0);
            dur_count += r.duration_count;
        }
        if !buckets.contains_key(&r.store_group_id) {
            order.push(r.store_group_id);
        }
        buckets.entry(r.store_group_id).or_default().add(r);
    }
    let mut named: Vec<(&(String, String, String), Bucket)> = order
        .iter()
        .filter_map(|id| {
            let id = (*id)?;
            Some((
                groups.get(&id)?,
                buckets.get(&Some(id)).copied().unwrap_or_default(),
            ))
        })
        .collect();
    named.sort_by(|a, b| collate_id(&a.0.2, &b.0.2));
    let mut by_group: Vec<Value> = named
        .into_iter()
        .map(|((uuid, code, name), b)| {
            bucket_row(json!({ "id": uuid, "code": code, "name": name }), &b)
        })
        .collect();
    if let Some(b) = buckets.get(&None) {
        by_group.push(bucket_row(Value::Null, b));
    }
    #[allow(clippy::cast_precision_loss)]
    let avg = if dur_count == 0 {
        0
    } else {
        js_round(dur_sum as f64 / dur_count as f64)
    };
    json!({
        "visitsInRange": active + completed,
        "completedVisits": completed,
        "activeVisits": active,
        "cancelledVisits": cancelled,
        "successVisits": success,
        "successRate": success_rate(success, completed),
        "gpsIssues": gps,
        "avgDurationSeconds": avg,
        "byGroup": by_group,
    })
}

async fn summary(db: &PgPool, filter: &VisitFilter) -> ApiResult<Value> {
    let mut qb = QueryBuilder::<Postgres>::new(
        r#"SELECT v."storeGroupId", v.state::text AS state, v.status::text AS status, v."hasGpsIssue",
                  COUNT(*) AS count,
                  (SUM(v."durationSeconds") FILTER (WHERE v.state = 'completed'))::bigint AS duration_sum,
                  COUNT(v."durationSeconds") FILTER (WHERE v.state = 'completed') AS duration_count"#,
    );
    qb.push(VISIT_FROM);
    filter.push_where(&mut qb, false);
    qb.push(r#" GROUP BY v."storeGroupId", v.state, v.status, v."hasGpsIssue""#);
    let rows: Vec<KpiRow> = qb.build_query_as().fetch_all(db).await?;
    let ids: Vec<i32> = rows.iter().filter_map(|r| r.store_group_id).collect();
    let groups: GroupRefs = if ids.is_empty() {
        HashMap::new()
    } else {
        let g: Vec<(i32, String, String, String)> = sqlx::query_as(
            r#"SELECT id, uuid::text, code, name FROM "storeGroup" WHERE id = ANY($1)"#,
        )
        .bind(&ids)
        .fetch_all(db)
        .await?;
        g.into_iter().map(|(id, u, c, n)| (id, (u, c, n))).collect()
    };
    Ok(compute_summary(&rows, &groups))
}

/// `COUNT(*)` atas `listWhere`.
pub async fn count_visits(db: &PgPool, filter: &VisitFilter) -> ApiResult<i64> {
    let mut qb = QueryBuilder::<Postgres>::new("SELECT COUNT(*)");
    qb.push(VISIT_FROM);
    filter.push_where(&mut qb, true);
    Ok(qb.build_query_scalar().fetch_one(db).await?)
}

/// Satu halaman kunjungan `listWhere` (urut `checkInAt DESC`).
async fn page_rows(
    db: &PgPool,
    filter: &VisitFilter,
    page: i64,
    limit: i64,
) -> ApiResult<Vec<VisitRow>> {
    let mut qb = QueryBuilder::<Postgres>::new(VISIT_COLUMNS);
    qb.push(VISIT_FROM).push(VISIT_OPTIONAL_JOINS);
    filter.push_where(&mut qb, true);
    qb.push(r#" ORDER BY v."checkInAt" DESC, v.id DESC LIMIT "#)
        .push_bind(limit)
        .push(" OFFSET ")
        .push_bind((page - 1).saturating_mul(limit));
    Ok(qb.build_query_as().fetch_all(db).await?)
}

/// `(visitId → [(kind, isConfirmed)])` untuk sekumpulan kunjungan (urut `id` foto).
pub async fn photo_flags(
    db: &PgPool,
    visit_ids: &[i32],
) -> ApiResult<HashMap<i32, Vec<(String, bool)>>> {
    let mut out: HashMap<i32, Vec<(String, bool)>> = HashMap::new();
    for p in photos_of_visits(db, visit_ids).await? {
        out.entry(p.visit_id)
            .or_default()
            .push((p.kind, p.is_confirmed));
    }
    Ok(out)
}

/// `findAll(userId, query)` → `{ data, summary, meta }`.
pub async fn find_all(state: &AppState, user_id: i32, q: &VisitListQuery) -> ApiResult<Value> {
    let r = resolve_where(state, user_id, q).await?;
    let (rows, total, summary) = tokio::try_join!(
        page_rows(&state.db, &r.filter, q.page, q.limit),
        count_visits(&state.db, &r.filter),
        summary(&state.db, &r.filter),
    )?;
    let ids: Vec<i32> = rows.iter().map(|v| v.id).collect();
    let flags = photo_flags(&state.db, &ids).await?;
    let empty = Vec::new();
    let data: Vec<Value> = rows
        .iter()
        .map(|v| {
            let photos = flags.get(&v.id).unwrap_or(&empty);
            v.to_list_item(photos.iter().map(|(k, c)| (k.as_str(), *c)))
        })
        .collect();
    let total_pages = if q.limit > 0 {
        (total + q.limit - 1) / q.limit
    } else {
        0
    };
    let mut body = Map::new();
    body.insert("data".into(), Value::Array(data));
    body.insert("summary".into(), summary);
    body.insert(
        "meta".into(),
        json!({ "total": total, "page": q.page, "limit": q.limit, "totalPages": total_pages }),
    );
    Ok(Value::Object(body))
}

fn visit_not_found() -> ApiError {
    ApiError::coded(StatusCode::NOT_FOUND, "VISIT_NOT_FOUND", "Visit not found")
}

/// `findOne(userId, visitId)` — kunjungan company lain = 404.
pub async fn find_one(state: &AppState, user_id: i32, visit_uuid: &str) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let mut qb = QueryBuilder::<Postgres>::new(VISIT_COLUMNS);
    qb.push(VISIT_FROM).push(VISIT_OPTIONAL_JOINS);
    qb.push(" WHERE v.uuid = ")
        .push_bind(visit_uuid.to_owned())
        .push(r#"::uuid AND v."companyId" = "#)
        .push_bind(company_id)
        .push(" LIMIT 1");
    let visit: Option<VisitRow> = qb.build_query_as().fetch_optional(&state.db).await?;
    let visit = visit.ok_or_else(visit_not_found)?;
    let photos = photos_of_visits(&state.db, &[visit.id]).await?;
    let photos = serialize_admin_photos(state, &visit.uuid, visit.company_id, &photos).await;
    Ok(visit.to_detail(photos))
}

/// `streamPhotoFile(userId, visitUuid, photoUuid, headers, res)`.
pub async fn stream_photo_file(
    state: &AppState,
    user_id: i32,
    visit_uuid: &str,
    photo_uuid: &str,
    headers: &axum::http::HeaderMap,
) -> ApiResult<axum::response::Response> {
    let company_id = require_company_id(&state.db, user_id).await?;
    crate::photos::stream_company_photo(state, company_id, visit_uuid, photo_uuid, headers).await
}

/// `photoFileUrl(userId, visitUuid, photoUuid)` → `{ url, expiresAt }`.
pub async fn photo_file_url(
    state: &AppState,
    user_id: i32,
    visit_uuid: &str,
    photo_uuid: &str,
) -> ApiResult<mz_files::SignedReadUrl> {
    let company_id = require_company_id(&state.db, user_id).await?;
    crate::photos::company_photo_url(state, company_id, visit_uuid, photo_uuid).await
}

#[cfg(test)]
pub(crate) mod tests_support {
    use super::*;

    pub(crate) fn visit() -> VisitRow {
        VisitRow {
            id: 1,
            uuid: "v".into(),
            company_id: 2,
            visit_number: "SV-1".into(),
            state: "active".into(),
            status: None,
            note: None,
            tags: vec!["a".into()],
            duration_seconds: None,
            check_in_at: JsDate::from_millis(1000).unwrap(),
            check_in_client_at: None,
            check_in_lat: -7.5,
            check_in_lng: 110.0,
            check_in_accuracy: 5.0,
            check_in_distance: 12,
            check_in_method: "gps".into(),
            check_in_gps_validation: "valid".into(),
            check_in_is_mock: false,
            check_in_is_override: false,
            check_in_override_reason: None,
            has_gps_issue: false,
            check_out_at: None,
            check_out_client_at: None,
            check_out_lat: None,
            check_out_lng: None,
            check_out_accuracy: None,
            check_out_distance: None,
            check_out_gps_validation: None,
            check_out_is_mock: None,
            created_at: JsDate::from_millis(2000).unwrap(),
            plan_id: Some(4),
            store_uuid: "s".into(),
            store_code: "C".into(),
            store_name: "N".into(),
            store_owner_name: "O".into(),
            sales_uuid: "u".into(),
            sales_name: "Sales".into(),
            sales_username: "sales".into(),
            vt_uuid: None,
            vt_code: None,
            vt_name: None,
            vt_requires_order: None,
            vt_requires_photo: None,
            vt_requires_gps: None,
            vt_is_active: None,
            group_uuid: None,
            group_code: None,
            group_name: None,
            plan_uuid: Some("p".into()),
            plan_visit_date: NaiveDate::from_ymd_opt(2026, 8, 20),
            plan_note: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::tests_support::visit;
    use super::*;

    fn row(group: Option<i32>, state: &str, status: Option<&str>, gps: bool, count: i64) -> KpiRow {
        KpiRow {
            store_group_id: group,
            state: state.into(),
            status: status.map(Into::into),
            has_gps_issue: gps,
            count,
            duration_sum: None,
            duration_count: 0,
        }
    }

    #[test]
    fn summary_metrics() {
        let mut done = row(None, "completed", Some("success"), false, 2);
        done.duration_sum = Some(1801);
        done.duration_count = 2;
        let rows = vec![
            row(None, "active", None, true, 3),
            done,
            row(None, "completed", Some("closed"), false, 2),
            row(None, "cancelled", None, true, 5),
        ];
        let s = compute_summary(&rows, &HashMap::new());
        // active + completed, cancelled terpisah.
        assert_eq!(s["visitsInRange"], 7);
        assert_eq!(s["activeVisits"], 3);
        assert_eq!(s["completedVisits"], 4);
        assert_eq!(s["cancelledVisits"], 5);
        // successRate dibagi completed, bukan visitsInRange.
        assert_eq!(s["successVisits"], 2);
        assert_eq!(s["successRate"], 50);
        assert_eq!(s["gpsIssues"], 8);
        // rata-rata durasi hanya completed (1801 / 2 = 900.5 → 901).
        assert_eq!(s["avgDurationSeconds"], 901);
    }

    #[test]
    fn no_completed_means_zero_not_nan() {
        let s = compute_summary(&[row(None, "active", None, false, 1)], &HashMap::new());
        assert_eq!(s["successRate"], 0);
        assert_eq!(s["avgDurationSeconds"], 0);
        let s = compute_summary(&[], &HashMap::new());
        assert_eq!(s["byGroup"], json!([]));
    }

    #[test]
    fn region_breakdown() {
        let groups: GroupRefs = HashMap::from([
            (1, ("u1".into(), "JOG".into(), "Jogja Barat".into())),
            (2, ("u2".into(), "BDG".into(), "Bandung".into())),
        ]);
        let rows = vec![
            row(Some(1), "completed", Some("success"), true, 3),
            row(Some(1), "completed", Some("closed"), false, 1),
            row(None, "completed", Some("success"), false, 1),
            row(Some(2), "active", None, false, 2),
            row(Some(9), "active", None, false, 2),
        ];
        let s = compute_summary(&rows, &groups);
        let by = s["byGroup"].as_array().unwrap();
        assert_eq!(by.len(), 3);
        assert_eq!(by[0]["group"]["name"], "Bandung");
        assert_eq!(
            by[1]["group"],
            json!({"id":"u1","code":"JOG","name":"Jogja Barat"})
        );
        assert_eq!(by[1]["successRate"], 75);
        assert_eq!(by[1]["gpsIssues"], 3);
        assert_eq!(by[2]["group"], Value::Null);
        // tanpa bucket null bila semua kunjungan punya region.
        let s = compute_summary(&rows[..2], &groups);
        assert_eq!(s["byGroup"].as_array().unwrap().len(), 1);
    }

    fn sql_of(f: &VisitFilter, list: bool) -> String {
        let mut qb = QueryBuilder::<Postgres>::new("SELECT 1");
        qb.push(VISIT_FROM);
        f.push_where(&mut qb, list);
        qb.sql().to_owned()
    }

    #[test]
    fn where_clause_shapes() {
        let base = VisitFilter {
            company_id: 2,
            ..Default::default()
        };
        let sql = sql_of(&base, true);
        assert!(sql.ends_with(r#"WHERE v."companyId" = $1"#), "{sql}");
        // KPI tidak terpengaruh state/status/planned.
        let f = VisitFilter {
            state: Some(StoreVisitState::active),
            status: Some(vec![StoreVisitStatus::success]),
            planned: Some(true),
            ..base.clone()
        };
        assert!(!sql_of(&f, false).contains("state"));
        let l = sql_of(&f, true);
        assert!(
            l.contains("v.state::text")
                && l.contains("v.status::text")
                && l.contains(r#""planId" IS NOT NULL"#)
        );
        let f = VisitFilter {
            planned: Some(false),
            ..base.clone()
        };
        assert!(sql_of(&f, true).contains(r#"v."planId" IS NULL"#));
        // groupId + unassigned = union; search OR tetap utuh.
        let f = VisitFilter {
            group_id: Some(vec!["g".into()]),
            unassigned: true,
            search: Some("x".into()),
            ..base.clone()
        };
        let s = sql_of(&f, false);
        assert!(s.contains("u.name ILIKE"));
        assert!(s.contains(r#"OR v."storeGroupId" IS NULL)"#));
        let f = VisitFilter {
            unassigned: true,
            ..base
        };
        assert!(sql_of(&f, false).ends_with(r#"AND v."storeGroupId" IS NULL"#));
    }

    #[test]
    fn rounding() {
        assert_eq!(js_round(64.5), 65);
        assert_eq!(js_round(-0.5), 0);
        assert_eq!(success_rate(22, 34), 65);
        assert_eq!(success_rate(1, 0), 0);
    }

    #[test]
    fn list_item_shape() {
        let v = visit();
        let item = v.to_list_item([("store", true), ("selfie", false)]);
        let keys: Vec<&str> = item
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            vec![
                "id",
                "visitNumber",
                "sales",
                "store",
                "state",
                "status",
                "visitType",
                "checkIn",
                "checkOut",
                "durationSeconds",
                "note",
                "tags",
                "photosCount",
                "photoKinds",
                "hasGpsIssue",
                "isPlanned",
                "createdAt"
            ]
        );
        assert_eq!(item["checkOut"], Value::Null);
        assert_eq!(item["isPlanned"], true);
        assert_eq!(item["photosCount"], 1);
        assert_eq!(item["checkIn"]["lng"], json!(110));
        assert_eq!(item["visitType"], Value::Null);
        assert!(item.get("qrToken").is_none());
        let mut typed = visit();
        typed.vt_uuid = Some("t".into());
        typed.vt_code = Some("REGULAR".into());
        typed.vt_name = Some("Regular".into());
        typed.vt_requires_gps = Some(false);
        typed.vt_is_active = Some(true);
        typed.check_out_at = JsDate::from_millis(5000);
        typed.check_out_distance = Some(3);
        let item = typed.to_list_item([]);
        assert_eq!(item["visitType"]["requiresGps"], false);
        assert_eq!(
            item["checkOut"],
            json!({"at": 5000, "distanceMeters": 3, "gpsValidation": null, "isMock": false})
        );
    }

    #[test]
    fn detail_plan_date_is_plain() {
        let d = visit().to_detail(vec![]);
        assert_eq!(
            d["plan"],
            json!({"id": "p", "visitDate": "2026-08-20", "note": null})
        );
        assert_eq!(d["group"], Value::Null);
    }
}
