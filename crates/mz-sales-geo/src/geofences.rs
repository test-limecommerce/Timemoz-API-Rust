//! Port `src/mobile-geofences/*`: mobile (`/mobile/geofences*`, 6 route) + admin
//! (`/v1/admin/geofences`, 4 route). Model `geofence` ber-audit (`AuditHandle`).

use std::collections::BTreeSet;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use mz_audit::{AuditHandle, Lookup, Model};
use mz_core::company::require_company_id;
use mz_core::extract::{ValidatedJson, ValidatedQuery, check_int_range};
use mz_core::iso8601::is_iso8601;
use mz_core::jsfmt::num as js_num;
use mz_core::time::{JsDate, now_naive};
use mz_core::uuid_rules::is_uuid;
use mz_core::{ApiError, ApiResult, AppState, RequestCtx};
use serde_json::{Map, Value, json};
use sqlx::types::Json as SqlJson;
use sqlx::{Postgres, QueryBuilder};

use crate::dto::{
    Field, Raw, as_number, check_bool, check_number, check_string, gate, js_to_string, page_limit,
    parse_float, parse_int_value, present,
};
use crate::geo::{self, Geofence};
use crate::plan::{PlanLimit, assert_plan_limit};

/// 500 (galat Prisma/TypeError di Node yang tak ditangkap).
pub(crate) fn internal(msg: &str) -> ApiError {
    ApiError::internal(anyhow::anyhow!("{msg}"))
}

/// Kolom `@db.Uuid`: string ditolak Prisma (P2023) → 500 di Node.
pub(crate) fn parse_row_uuid(raw: &str) -> ApiResult<uuid::Uuid> {
    mz_core::prisma::parse_uuid(raw).map_err(|m| internal(&m))
}

// ───────────────────────── baris & serialisasi ─────────────────────────

/// Baris `geofence` (semua kolom skalar, urutan `schema.prisma`).
#[derive(Debug, sqlx::FromRow)]
pub struct GeofenceRow {
    pub id: i32,
    pub uuid: String,
    #[sqlx(rename = "companyId")]
    pub company_id: i32,
    pub name: String,
    pub address: Option<String>,
    pub lat: f64,
    pub lng: f64,
    #[sqlx(rename = "radiusMeters")]
    pub radius_meters: i32,
    pub enforced: bool,
    #[sqlx(rename = "applicableTo")]
    pub applicable_to: String,
    #[sqlx(rename = "applicableIds")]
    pub applicable_ids: Vec<i32>,
    #[sqlx(rename = "isActive")]
    pub is_active: bool,
    #[sqlx(rename = "createdById")]
    pub created_by_id: i32,
    pub timezone: Option<String>,
    #[sqlx(rename = "weeklySchedule")]
    pub weekly_schedule: Option<SqlJson<Value>>,
    #[sqlx(rename = "breakTimeHours")]
    pub break_time_hours: i32,
    #[sqlx(rename = "breakTimeMinutes")]
    pub break_time_minutes: i32,
    #[sqlx(rename = "createdAt")]
    pub created_at: JsDate,
    #[sqlx(rename = "updatedAt")]
    pub updated_at: Option<JsDate>,
}

const COLUMNS: &str = r#"id, uuid::text AS uuid, "companyId", name, address, lat, lng, "radiusMeters",
    enforced, "applicableTo"::text AS "applicableTo", COALESCE("applicableIds", '{}') AS "applicableIds",
    "isActive", "createdById", timezone, "weeklySchedule", "breakTimeHours", "breakTimeMinutes",
    "createdAt", "updatedAt""#;

impl GeofenceRow {
    /// Bentuk Prisma (semua kolom, urutan skema).
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "uuid": self.uuid,
            "companyId": self.company_id,
            "name": self.name,
            "address": self.address,
            "lat": js_num(self.lat),
            "lng": js_num(self.lng),
            "radiusMeters": self.radius_meters,
            "enforced": self.enforced,
            "applicableTo": self.applicable_to,
            "applicableIds": self.applicable_ids,
            "isActive": self.is_active,
            "createdById": self.created_by_id,
            "timezone": self.timezone,
            "weeklySchedule": self.weekly_schedule.as_ref().map(|j| j.0.clone()),
            "breakTimeHours": self.break_time_hours,
            "breakTimeMinutes": self.break_time_minutes,
            "createdAt": self.created_at,
            "updatedAt": self.updated_at,
        })
    }
}

async fn fetch_row(state: &AppState, id: i32) -> ApiResult<GeofenceRow> {
    Ok(
        sqlx::query_as(&format!(r#"SELECT {COLUMNS} FROM geofence WHERE id = $1"#))
            .bind(id)
            .fetch_one(&state.db)
            .await?,
    )
}

// ───────────────────────── DTO ─────────────────────────

const DAYS: [&str; 7] = [
    "monday",
    "tuesday",
    "wednesday",
    "thursday",
    "friday",
    "saturday",
    "sunday",
];

/// `TIME_REGEX = /^(0?[1-9]|1[0-2]):[0-5][0-9] (AM|PM)$/`.
pub fn is_schedule_time(s: &str) -> bool {
    let Some((t, ap)) = s.rsplit_once(' ') else {
        return false;
    };
    if ap != "AM" && ap != "PM" {
        return false;
    }
    let Some((h, m)) = t.split_once(':') else {
        return false;
    };
    let hour_ok = match h.as_bytes() {
        [a] => (b'1'..=b'9').contains(a),
        [b'0', b] => (b'1'..=b'9').contains(b),
        [b'1', b] => (b'0'..=b'2').contains(b),
        _ => false,
    };
    let min_ok = matches!(m.as_bytes(), [a, b] if (b'0'..=b'5').contains(a) && b.is_ascii_digit());
    hour_ok && min_ok
}

/// `@Matches(TIME_REGEX, { message }) @IsString()` pada `startTime`/`endTime` (pesan Matches dulu).
fn time_errors(prefix: &str, field: &str, v: Option<&Value>, out: &mut Vec<String>) {
    let ok = matches!(v, Some(Value::String(s)) if is_schedule_time(s));
    if !ok {
        out.push(format!("{prefix}{field} must be in format \"HH:MM AM/PM\""));
    }
    if !matches!(v, Some(Value::String(_))) {
        out.push(format!("{prefix}{field} must be a string"));
    }
}

/// `@ValidateNested() @Type(() => WeeklyScheduleDto)` pada nilai non-null. Pesan nested diberi
/// awalan jalur induk (`flattenValidationErrors` Nest).
fn schedule_errors(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Object(m) => {
            for day in DAYS {
                match m.get(day) {
                    None | Some(Value::Array(_)) => {}
                    Some(Value::Object(d)) => {
                        let prefix = format!("weeklySchedule.{day}.");
                        if !matches!(d.get("enabled"), Some(Value::Bool(_))) {
                            out.push(format!("{prefix}enabled must be a boolean value"));
                        }
                        time_errors(&prefix, "startTime", d.get("startTime"), out);
                        time_errors(&prefix, "endTime", d.get("endTime"), out);
                    }
                    Some(_) => out.push(format!(
                        "weeklySchedule.nested property {day} must be either object or array"
                    )),
                }
            }
        }
        Value::Array(_) => {}
        _ => out.push("nested property weeklySchedule must be either object or array".into()),
    }
}

/// `BreakTimeDto`: `Some((hours, minutes))`; `Err` = pesan.
fn break_time_check(v: &Value, errs: &mut Vec<String>) -> Option<Option<(i32, i32)>> {
    match v {
        Value::Object(m) => {
            let before = errs.len();
            let n = |k: &str| match m.get(k) {
                Some(Value::Number(n)) => n.as_f64(),
                _ => None,
            };
            let h = check_int_range(errs, "breakTime.hours", n("hours"), 0, 23);
            let mi = check_int_range(errs, "breakTime.minutes", n("minutes"), 0, 59);
            if errs.len() != before {
                return None;
            }
            Some(Some((i32::try_from(h?).ok()?, i32::try_from(mi?).ok()?)))
        }
        // `ValidateNested` atas array memeriksa tiap elemen (tidak ada) → lolos, tanpa jam/menit.
        Value::Array(_) => Some(None),
        _ => {
            errs.push("nested property breakTime must be either object or array".into());
            None
        }
    }
}

#[derive(Debug, Default)]
struct Dto {
    name: Field<String>,
    address: Field<String>,
    lat: Field<f64>,
    lng: Field<f64>,
    radius_meters: Field<i32>,
    enforced: Field<bool>,
    applicable_to: Field<String>,
    applicable_ids: Field<Vec<i64>>,
    timezone: Field<String>,
    weekly_schedule: Field<Value>,
    /// `Val(None)` = array (tidak ada jam/menit untuk ditulis).
    break_time: Field<Option<(i32, i32)>>,
    is_active: Field<bool>,
}

/// Kunci ada tapi `null` → `Null`; tidak ada → `Absent` (`@IsOptional()` melewati validasinya).
fn null_or_absent<T>(m: &Map<String, Value>, key: &str) -> Field<T> {
    if m.contains_key(key) {
        Field::Null
    } else {
        Field::Absent
    }
}

/// `CreateGeofenceDto` (`required`) / `UpdateGeofenceDto` (`PartialType`: semua `@IsOptional()`).
fn parse(m: &Map<String, Value>, required: bool) -> Result<Dto, Vec<String>> {
    let mut errs = Vec::new();
    let mut d = Dto::default();
    match gate(m, "name", required) {
        Some(v) => {
            if let Some(s) = check_string(&mut errs, "name", v) {
                d.name = Field::Val(s);
            }
        }
        None => d.name = null_or_absent(m, "name"),
    }
    match present(m, "address") {
        Some(v) => {
            if let Some(s) = check_string(&mut errs, "address", Some(v)) {
                d.address = Field::Val(s);
            }
        }
        None => d.address = null_or_absent(m, "address"),
    }
    match gate(m, "lat", required) {
        Some(v) => {
            if let Some(n) = check_number(&mut errs, "lat", as_number(v), Some(-90.0), Some(90.0)) {
                d.lat = Field::Val(n);
            }
        }
        None => d.lat = null_or_absent(m, "lat"),
    }
    match gate(m, "lng", required) {
        Some(v) => {
            if let Some(n) = check_number(&mut errs, "lng", as_number(v), Some(-180.0), Some(180.0))
            {
                d.lng = Field::Val(n);
            }
        }
        None => d.lng = null_or_absent(m, "lng"),
    }
    match gate(m, "radiusMeters", required) {
        Some(v) => {
            let n = match v {
                Some(Value::Number(n)) => n.as_f64(),
                _ => None,
            };
            if let Some(i) = check_int_range(&mut errs, "radiusMeters", n, 50, 1000)
                && let Ok(i) = i32::try_from(i)
            {
                d.radius_meters = Field::Val(i);
            }
        }
        None => d.radius_meters = null_or_absent(m, "radiusMeters"),
    }
    match present(m, "enforced") {
        Some(v) => {
            if let Some(b) = check_bool(&mut errs, "enforced", Some(v)) {
                d.enforced = Field::Val(b);
            }
        }
        None => d.enforced = null_or_absent(m, "enforced"),
    }
    match present(m, "applicableTo") {
        Some(Value::String(s)) if s == "all" || s == "user_ids" => {
            d.applicable_to = Field::Val(s.clone());
        }
        Some(_) => errs.push("applicableTo must be one of the following values: ".to_owned()),
        None => d.applicable_to = null_or_absent(m, "applicableTo"),
    }
    match present(m, "applicableIds") {
        Some(v) => {
            // `@IsInt({ each: true })` (array: tiap elemen; bukan array: nilainya sendiri), lalu `@IsArray()`.
            let is_int = |x: &Value| {
                x.as_f64()
                    .is_some_and(|f| f.is_finite() && f.fract() == 0.0)
            };
            let each_ok = match v {
                Value::Array(a) => a.iter().all(is_int),
                other => is_int(other),
            };
            if !each_ok {
                errs.push("each value in applicableIds must be an integer number".into());
            }
            if let Value::Array(a) = v {
                if each_ok {
                    // Bilangan bulat di luar `i64` mustahil lolos `Int`; dibulatkan agar ditolak di tulis.
                    d.applicable_ids = Field::Val(
                        a.iter()
                            .map(|x| {
                                x.as_i64().unwrap_or_else(|| {
                                    #[allow(clippy::cast_possible_truncation)]
                                    {
                                        x.as_f64().unwrap_or(0.0) as i64
                                    }
                                })
                            })
                            .collect(),
                    );
                }
            } else {
                errs.push("applicableIds must be an array".into());
            }
        }
        None => d.applicable_ids = null_or_absent(m, "applicableIds"),
    }
    match present(m, "timezone") {
        Some(v) => {
            if let Some(s) = check_string(&mut errs, "timezone", Some(v)) {
                d.timezone = Field::Val(s);
            }
        }
        None => d.timezone = null_or_absent(m, "timezone"),
    }
    match present(m, "weeklySchedule") {
        Some(v) => {
            schedule_errors(v, &mut errs);
            d.weekly_schedule = Field::Val(v.clone());
        }
        None => d.weekly_schedule = null_or_absent(m, "weeklySchedule"),
    }
    match present(m, "breakTime") {
        Some(v) => {
            if let Some(bt) = break_time_check(v, &mut errs) {
                d.break_time = Field::Val(bt);
            }
        }
        None => d.break_time = null_or_absent(m, "breakTime"),
    }
    match present(m, "isActive") {
        Some(v) => {
            if let Some(b) = check_bool(&mut errs, "isActive", Some(v)) {
                d.is_active = Field::Val(b);
            }
        }
        None => d.is_active = null_or_absent(m, "isActive"),
    }
    if errs.is_empty() { Ok(d) } else { Err(errs) }
}

/// `null` pada kolom non-null: Prisma melempar → 500. `Absent` → `None`.
fn defined<T: Clone>(f: &Field<T>) -> ApiResult<Option<T>> {
    match f {
        Field::Absent => Ok(None),
        Field::Null => Err(internal("Argument must not be null")),
        Field::Val(v) => Ok(Some(v.clone())),
    }
}

/// Kolom `Json?`: Node meneruskan `null` apa adanya ke Prisma, yang menyimpannya sebagai JSON `null`
/// (jsonb `'null'`, bukan SQL NULL) dan sukses — beda dengan kolom skalar non-null (lihat `defined`).
fn json_or_null(f: &Field<Value>) -> Option<Value> {
    match f {
        Field::Absent => None,
        Field::Null => Some(Value::Null),
        Field::Val(v) => Some(v.clone()),
    }
}

fn ids_i32(ids: &[i64]) -> ApiResult<Vec<i32>> {
    ids.iter()
        .map(|i| i32::try_from(*i).map_err(|_| internal("Value out of range for Int")))
        .collect()
}

// ───────────────────────── admin ─────────────────────────

/// `listGeofencesAdmin`.
pub async fn list_admin(state: &AppState, user_id: i32) -> ApiResult<Vec<GeofenceRow>> {
    let company_id = require_company_id(&state.db, user_id).await?;
    Ok(sqlx::query_as(&format!(
        r#"SELECT {COLUMNS} FROM geofence WHERE "companyId" = $1 ORDER BY id ASC"#
    ))
    .bind(company_id)
    .fetch_all(&state.db)
    .await?)
}

async fn create(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    dto: &Dto,
) -> ApiResult<GeofenceRow> {
    let company_id = require_company_id(&state.db, user_id).await?;
    // Batas `totalGeofence` + INSERT dalam satu transaksi (advisory lock `xact`).
    let discard = audit.discard_guard();
    let mut tx = state.db.begin().await?;
    assert_plan_limit(&mut tx, company_id, PlanLimit::Geofences).await?;

    let (Field::Val(name), Field::Val(lat), Field::Val(lng), Field::Val(radius)) =
        (&dto.name, &dto.lat, &dto.lng, &dto.radius_meters)
    else {
        return Err(internal("geofence DTO incomplete"));
    };
    let enforced = match &dto.enforced {
        Field::Val(b) => *b,
        _ => false,
    };
    let applicable_to = match &dto.applicable_to {
        Field::Val(s) => s.clone(),
        _ => "all".to_owned(),
    };
    let applicable_ids = match &dto.applicable_ids {
        Field::Val(v) => ids_i32(v)?,
        _ => Vec::new(),
    };
    let address = &dto.address;
    let timezone = &dto.timezone;
    let schedule = json_or_null(&dto.weekly_schedule);
    let break_time = defined(&dto.break_time)?.flatten();
    if matches!(dto.break_time, Field::Null) {
        return Err(internal("Cannot read properties of null (reading 'hours')"));
    }
    let is_active = defined(&dto.is_active)?;

    let mut qb = QueryBuilder::<Postgres>::new(
        r#"INSERT INTO geofence ("companyId", name, lat, lng, "radiusMeters", enforced,
              "applicableTo", "applicableIds", "createdById""#,
    );
    if address.is_defined() {
        qb.push(", address");
    }
    if timezone.is_defined() {
        qb.push(", timezone");
    }
    if schedule.is_some() {
        qb.push(r#", "weeklySchedule""#);
    }
    if break_time.is_some() {
        qb.push(r#", "breakTimeHours", "breakTimeMinutes""#);
    }
    if is_active.is_some() {
        qb.push(r#", "isActive""#);
    }
    qb.push(") VALUES (")
        .push_bind(company_id)
        .push(", ")
        .push_bind(name.clone())
        .push(", ")
        .push_bind(*lat)
        .push(", ")
        .push_bind(*lng)
        .push(", ")
        .push_bind(*radius)
        .push(", ")
        .push_bind(enforced)
        .push(", ")
        .push_bind(applicable_to)
        .push(r#"::"GeofenceApplicableTo", "#)
        .push_bind(applicable_ids)
        .push(", ")
        .push_bind(user_id);
    match address {
        Field::Val(s) => {
            qb.push(", ").push_bind(s.clone());
        }
        Field::Null => {
            qb.push(", NULL");
        }
        Field::Absent => {}
    }
    match timezone {
        Field::Val(s) => {
            qb.push(", ").push_bind(s.clone());
        }
        Field::Null => {
            qb.push(", NULL");
        }
        Field::Absent => {}
    }
    if let Some(s) = schedule {
        qb.push(", ").push_bind(SqlJson(s));
    }
    if let Some((h, mi)) = break_time {
        qb.push(", ").push_bind(h).push(", ").push_bind(mi);
    }
    if let Some(a) = is_active {
        qb.push(", ").push_bind(a);
    }
    qb.push(") RETURNING id");
    let id: i32 = qb.build_query_scalar().fetch_one(&mut *tx).await?;
    if let Some(r) = audit
        .result_row(&mut *tx, Model::Geofence, Lookup::Id(id))
        .await
    {
        audit.record_create(Model::Geofence, &r).await;
    }
    tx.commit().await?;
    discard.disarm();
    fetch_row(state, id).await
}

async fn find_in_company(
    state: &AppState,
    company_id: i32,
    raw_uuid: &str,
) -> ApiResult<GeofenceRow> {
    let uuid = parse_row_uuid(raw_uuid)?;
    let row: Option<GeofenceRow> = sqlx::query_as(&format!(
        r#"SELECT {COLUMNS} FROM geofence WHERE uuid = $1 AND "companyId" = $2 ORDER BY id LIMIT 1"#
    ))
    .bind(uuid)
    .bind(company_id)
    .fetch_optional(&state.db)
    .await?;
    row.ok_or_else(|| ApiError::not_found("Geofence not found"))
}

async fn update(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    raw_uuid: &str,
    dto: &Dto,
) -> ApiResult<GeofenceRow> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let gf = find_in_company(state, company_id, raw_uuid).await?;

    let name = defined(&dto.name)?;
    let lat = defined(&dto.lat)?;
    let lng = defined(&dto.lng)?;
    let radius = defined(&dto.radius_meters)?;
    let enforced = defined(&dto.enforced)?;
    let applicable_to = defined(&dto.applicable_to)?;
    let applicable_ids = defined(&dto.applicable_ids)?
        .map(|v| ids_i32(&v))
        .transpose()?;
    let schedule = json_or_null(&dto.weekly_schedule);
    if matches!(dto.break_time, Field::Null) {
        return Err(internal("Cannot read properties of null (reading 'hours')"));
    }
    let break_time = defined(&dto.break_time)?.flatten();
    let is_active = defined(&dto.is_active)?;

    let pre = audit.pre_read(Model::Geofence, Lookup::Id(gf.id)).await;
    let mut qb = QueryBuilder::<Postgres>::new("UPDATE geofence SET ");
    if let Some(v) = name {
        qb.push("name = ").push_bind(v).push(", ");
    }
    if let Some(v) = lat {
        qb.push("lat = ").push_bind(v).push(", ");
    }
    if let Some(v) = lng {
        qb.push("lng = ").push_bind(v).push(", ");
    }
    if let Some(v) = radius {
        qb.push(r#""radiusMeters" = "#).push_bind(v).push(", ");
    }
    if let Some(v) = enforced {
        qb.push("enforced = ").push_bind(v).push(", ");
    }
    if let Some(v) = applicable_to {
        qb.push(r#""applicableTo" = "#)
            .push_bind(v)
            .push(r#"::"GeofenceApplicableTo", "#);
    }
    if let Some(v) = applicable_ids {
        qb.push(r#""applicableIds" = "#).push_bind(v).push(", ");
    }
    match &dto.address {
        Field::Val(s) => {
            qb.push("address = ").push_bind(s.clone()).push(", ");
        }
        Field::Null => {
            qb.push("address = NULL, ");
        }
        Field::Absent => {}
    }
    match &dto.timezone {
        Field::Val(s) => {
            qb.push("timezone = ").push_bind(s.clone()).push(", ");
        }
        Field::Null => {
            qb.push("timezone = NULL, ");
        }
        Field::Absent => {}
    }
    if let Some(v) = schedule {
        qb.push(r#""weeklySchedule" = "#)
            .push_bind(SqlJson(v))
            .push(", ");
    }
    if let Some((h, mi)) = break_time {
        qb.push(r#""breakTimeHours" = "#)
            .push_bind(h)
            .push(r#", "breakTimeMinutes" = "#)
            .push_bind(mi)
            .push(", ");
    }
    if let Some(v) = is_active {
        qb.push(r#""isActive" = "#).push_bind(v).push(", ");
    }
    qb.push(r#""updatedAt" = "#)
        .push_bind(now_naive())
        .push(" WHERE id = ")
        .push_bind(gf.id);
    qb.build().execute(&state.db).await?;
    if let Some(r) = audit
        .result_row(&state.db, Model::Geofence, Lookup::Id(gf.id))
        .await
    {
        audit.record_update(Model::Geofence, pre, &r).await;
    }
    fetch_row(state, gf.id).await
}

async fn remove(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    raw_uuid: &str,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let gf = find_in_company(state, company_id, raw_uuid).await?;
    let pre = audit.pre_read(Model::Geofence, Lookup::Id(gf.id)).await;
    sqlx::query(r#"UPDATE geofence SET "isActive" = false, "updatedAt" = $2 WHERE id = $1"#)
        .bind(gf.id)
        .bind(now_naive())
        .execute(&state.db)
        .await?;
    if let Some(r) = audit
        .result_row(&state.db, Model::Geofence, Lookup::Id(gf.id))
        .await
    {
        audit.record_update(Model::Geofence, pre, &r).await;
    }
    Ok(json!({ "deleted": true }))
}

/// Node: src/mobile-geofences/admin-geofences.controller.ts:73 `list`
pub async fn h_admin_list(
    State(state): State<AppState>,
    ctx: RequestCtx,
) -> ApiResult<Json<Value>> {
    let rows = list_admin(&state, ctx.require_user_id()?).await?;
    Ok(Json(
        json!({ "data": rows.iter().map(GeofenceRow::to_json).collect::<Vec<_>>() }),
    ))
}

/// Node: src/mobile-geofences/admin-geofences.controller.ts:107 `create`
pub async fn h_admin_create(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    ValidatedJson(body): ValidatedJson<Raw>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let dto = parse(&body.0, true).map_err(ApiError::Validation)?;
    let row = create(&state, &audit, ctx.require_user_id()?, &dto).await?;
    Ok((StatusCode::CREATED, Json(json!({ "data": row.to_json() }))))
}

/// Node: src/mobile-geofences/admin-geofences.controller.ts:184 `update`
pub async fn h_admin_update(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    Path(id): Path<String>,
    ValidatedJson(body): ValidatedJson<Raw>,
) -> ApiResult<Json<Value>> {
    let dto = parse(&body.0, false).map_err(ApiError::Validation)?;
    let row = update(&state, &audit, ctx.require_user_id()?, &id, &dto).await?;
    Ok(Json(json!({ "data": row.to_json() })))
}

/// Node: src/mobile-geofences/admin-geofences.controller.ts:242 `remove`
pub async fn h_admin_remove(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let data = remove(&state, &audit, ctx.require_user_id()?, &id).await?;
    Ok(Json(json!({ "data": data })))
}

// ───────────────────────── mobile ─────────────────────────

/// `listGeofences(userId)`: geofence aktif company yang berlaku untuk user.
pub async fn list_for_user(state: &AppState, user_id: i32) -> ApiResult<Vec<Geofence>> {
    let company_id = require_company_id(&state.db, user_id).await?;
    Ok(geo::active_geofences_for(&state.db, company_id, user_id).await?)
}

/// Node: src/mobile-geofences/mobile-geofences.controller.ts:48 `list`
pub async fn h_mobile_list(
    State(state): State<AppState>,
    ctx: RequestCtx,
) -> ApiResult<Json<Value>> {
    let rows = list_for_user(&state, ctx.require_user_id()?).await?;
    let data: Vec<Value> = rows
        .iter()
        .map(|g| {
            json!({
                "id": g.uuid,
                "name": g.name,
                "lat": js_num(g.lat),
                "lng": js_num(g.lng),
                "radiusMeters": g.radius_meters,
                "enforced": g.enforced,
            })
        })
        .collect();
    Ok(Json(json!({ "data": data })))
}

/// `getGeofenceDashboard`.
async fn dashboard(state: &AppState, user_id: i32) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let geofences: Vec<GeofenceRow> = sqlx::query_as(&format!(
        r#"SELECT {COLUMNS} FROM geofence WHERE "companyId" = $1 ORDER BY id ASC"#
    ))
    .bind(company_id)
    .fetch_all(&state.db)
    .await?;
    let total_employees: i64 =
        sqlx::query_scalar(r#"SELECT COUNT(*) FROM "userCompany" WHERE "companyId" = $1"#)
            .bind(company_id)
            .fetch_one(&state.db)
            .await?;

    let covered = if geofences.iter().any(|g| g.applicable_to == "all") {
        total_employees
    } else {
        let unique: BTreeSet<i32> = geofences
            .iter()
            .flat_map(|g| g.applicable_ids.iter().copied())
            .collect();
        i64::try_from(unique.len()).unwrap_or(i64::MAX)
    };
    let active = geofences.iter().filter(|g| g.is_active).count();
    let zones: Vec<Value> = geofences
        .iter()
        .map(|g| {
            let assigned = if g.applicable_to == "all" {
                total_employees
            } else {
                i64::try_from(g.applicable_ids.len()).unwrap_or(i64::MAX)
            };
            json!({
                "id": g.uuid,
                "name": g.name,
                "address": g.address,
                "lat": js_num(g.lat),
                "lng": js_num(g.lng),
                "radiusMeters": g.radius_meters,
                "totalAssigned": assigned,
                "isActive": g.is_active,
            })
        })
        .collect();
    Ok(json!({
        "summary": {
            "total_zones": geofences.len(),
            "active": active,
            "inactive": geofences.len() - active,
            "employees_covered": covered,
        },
        "zones": zones,
    }))
}

/// Node: src/mobile-geofences/mobile-geofences.controller.ts:90 `dashboard`
pub async fn h_dashboard(State(state): State<AppState>, ctx: RequestCtx) -> ApiResult<Json<Value>> {
    let data = dashboard(&state, ctx.require_user_id()?).await?;
    Ok(Json(json!({ "data": data })))
}

/// `NearbyQueryDto`: `(lat, lng, limit)`.
fn parse_nearby(m: &Map<String, Value>) -> Result<(f64, f64, Option<f64>), Vec<String>> {
    let mut errs = Vec::new();
    let float = |k: &str| m.get(k).map(|v| parse_float(&js_to_string(v)));
    let lat = float("lat");
    let lng = float("lng");
    let msg = |f: &str| format!("{f} must be a number conforming to the specified constraints");
    let ok = |x: Option<f64>| x.filter(|v| v.is_finite());
    let lat_ok = ok(lat);
    if lat_ok.is_none() {
        errs.push(msg("lat"));
    }
    let lng_ok = ok(lng);
    if lng_ok.is_none() {
        errs.push(msg("lng"));
    }
    // `@Transform(value ? parseInt(value, 10) : 20) @IsNumber() @Max(50) @IsOptional()`.
    let limit = match present(m, "limit") {
        None => None,
        Some(v) => {
            let truthy = !matches!(v, Value::String(s) if s.is_empty());
            if truthy {
                let n = parse_int_value(v);
                #[allow(clippy::cast_precision_loss)]
                let f = n.map(|n| n as f64);
                if f.is_none_or(|x| x > 50.0) {
                    errs.push("limit must not be greater than 50".into());
                }
                if f.is_none() {
                    errs.push(msg("limit"));
                }
                f
            } else {
                Some(20.0)
            }
        }
    };
    if !errs.is_empty() {
        return Err(errs);
    }
    Ok((lat_ok.unwrap_or(0.0), lng_ok.unwrap_or(0.0), limit))
}

/// `Array.prototype.slice(0, limit)`.
fn slice_end(len: usize, limit: i64) -> usize {
    let len_i = i64::try_from(len).unwrap_or(i64::MAX);
    let end = if limit < 0 {
        (len_i + limit).max(0)
    } else {
        limit.min(len_i)
    };
    usize::try_from(end).unwrap_or(0)
}

/// `listNearby`.
async fn nearby(
    state: &AppState,
    user_id: i32,
    lat: f64,
    lng: f64,
    limit: Option<f64>,
) -> ApiResult<Value> {
    let geofences = list_for_user(state, user_id).await?;
    let limit = limit.unwrap_or(20.0);
    #[allow(clippy::cast_possible_truncation)]
    let end = slice_end(geofences.len(), limit as i64);
    let data: Vec<Value> = geo::by_distance(&geofences, lat, lng)
        .into_iter()
        .take(end)
        .map(|(g, d)| {
            json!({
                "id": g.uuid,
                "name": g.name,
                "lat": js_num(g.lat),
                "lng": js_num(g.lng),
                "radiusMeters": g.radius_meters,
                "enforced": g.enforced,
                "distanceMeters": d,
            })
        })
        .collect();
    Ok(Value::Array(data))
}

/// Node: src/mobile-geofences/mobile-geofences.controller.ts:139 `nearby`
pub async fn h_nearby(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedQuery(q): ValidatedQuery<Raw>,
) -> ApiResult<Json<Value>> {
    let (lat, lng, limit) = parse_nearby(&q.0).map_err(ApiError::Validation)?;
    let data = nearby(&state, ctx.require_user_id()?, lat, lng, limit).await?;
    Ok(Json(json!({ "data": data })))
}

/// `getGeofenceSchedule`.
async fn schedule(state: &AppState, user_id: i32, raw_uuid: &str) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let uuid = parse_row_uuid(raw_uuid)?;
    let row: Option<GeofenceRow> = sqlx::query_as(&format!(
        r#"SELECT {COLUMNS} FROM geofence WHERE uuid = $1 AND "companyId" = $2 AND "isActive" = true
           ORDER BY id LIMIT 1"#
    ))
    .bind(uuid)
    .bind(company_id)
    .fetch_optional(&state.db)
    .await?;
    let gf = row.ok_or_else(|| ApiError::not_found("Geofence not found or not accessible"))?;
    Ok(json!({
        "id": gf.uuid,
        "name": gf.name,
        "timezone": gf.timezone,
        "weeklySchedule": gf.weekly_schedule.map(|j| j.0),
        "breakTime": { "hours": gf.break_time_hours, "minutes": gf.break_time_minutes },
    }))
}

/// Node: src/mobile-geofences/mobile-geofences.controller.ts:174 `getSchedule`
pub async fn h_schedule(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let data = schedule(&state, ctx.require_user_id()?, &id).await?;
    Ok(Json(json!({ "data": data })))
}

// ── riwayat event ──

#[derive(Debug, Default)]
struct HistoryQuery {
    geofence_id: Option<String>,
    event: Option<String>,
    start_date: Option<String>,
    end_date: Option<String>,
    page: i64,
    limit: i64,
}

/// `GeofenceEventHistoryQueryDto`.
fn parse_history(m: &Map<String, Value>) -> Result<HistoryQuery, Vec<String>> {
    let mut errs = Vec::new();
    let mut q = HistoryQuery {
        page: 1,
        limit: 20,
        ..HistoryQuery::default()
    };
    if let Some(v) = present(m, "geofenceId") {
        match v {
            Value::String(s) if is_uuid(s) => q.geofence_id = Some(s.clone()),
            _ => errs.push("geofenceId must be a UUID".into()),
        }
    }
    if let Some(v) = present(m, "event") {
        match v {
            Value::String(s) if s == "enter" || s == "exit" => q.event = Some(s.clone()),
            _ => errs.push("event must be one of the following values: enter, exit".into()),
        }
    }
    for (key, slot) in [
        ("startDate", &mut q.start_date),
        ("endDate", &mut q.end_date),
    ] {
        if let Some(v) = present(m, key) {
            match v {
                Value::String(s) if is_iso8601(s) => *slot = Some(s.clone()),
                _ => errs.push(format!("{key} must be a valid ISO 8601 date string")),
            }
        }
    }
    (q.page, q.limit) = page_limit(m, &mut errs);
    if errs.is_empty() { Ok(q) } else { Err(errs) }
}

/// `new Date(`${d}T${time}`)`; `Err` = Invalid Date (Prisma menolak → 500).
fn date_at(d: &str, time: &str) -> Result<chrono::NaiveDateTime, ()> {
    chrono::DateTime::parse_from_rfc3339(&format!("{d}T{time}"))
        .map(|dt| dt.naive_utc())
        .map_err(|_| ())
}

/// (id, geofence uuid, geofence name, event, lat, lng, accuracy, timestamp).
type HistoryRow = (i32, String, String, String, f64, f64, f64, JsDate);

/// `listEventHistory`.
async fn event_history(state: &AppState, user_id: i32, q: &HistoryQuery) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;

    let geofence_filter: Option<i32> = match &q.geofence_id {
        Some(g) => {
            let uuid = parse_row_uuid(g)?;
            let id: Option<i32> = sqlx::query_scalar(
                r#"SELECT id FROM geofence WHERE uuid = $1 AND "companyId" = $2 ORDER BY id LIMIT 1"#,
            )
            .bind(uuid)
            .bind(company_id)
            .fetch_optional(&state.db)
            .await?;
            Some(id.ok_or_else(|| ApiError::not_found("Geofence not found or not accessible"))?)
        }
        None => None,
    };

    let gte = q.start_date.as_deref().map(|d| date_at(d, "00:00:00.000Z"));
    let lte = q.end_date.as_deref().map(|d| date_at(d, "23:59:59.999Z"));
    if let (Some(Ok(a)), Some(Ok(b))) = (&gte, &lte)
        && b < a
    {
        return Err(ApiError::bad_request(
            "endDate must be on or after startDate",
        ));
    }
    let gte = gte
        .transpose()
        .map_err(|()| internal("Invalid Date (startDate)"))?;
    let lte = lte
        .transpose()
        .map_err(|()| internal("Invalid Date (endDate)"))?;

    let push_where = |qb: &mut QueryBuilder<'_, Postgres>| {
        qb.push(r#" FROM "geofenceAuditLog" l JOIN geofence g ON g.id = l."geofenceId" WHERE l."userId" = "#)
            .push_bind(user_id);
        match geofence_filter {
            Some(id) => {
                qb.push(r#" AND l."geofenceId" = "#).push_bind(id);
            }
            None => {
                qb.push(r#" AND g."companyId" = "#).push_bind(company_id);
            }
        }
        if let Some(e) = &q.event {
            qb.push(" AND l.event = ").push_bind(e.clone());
        }
        if let Some(t) = gte {
            qb.push(r#" AND l."timestamp" >= "#).push_bind(t);
        }
        if let Some(t) = lte {
            qb.push(r#" AND l."timestamp" <= "#).push_bind(t);
        }
    };

    let skip = (q.page - 1).saturating_mul(q.limit);
    let mut list = QueryBuilder::<Postgres>::new(
        r#"SELECT l.id, g.uuid::text AS "geofenceUuid", g.name AS "geofenceName", l.event, l.lat, l.lng,
                  l.accuracy, l."timestamp""#,
    );
    push_where(&mut list);
    list.push(r#" ORDER BY l."timestamp" DESC, l.id DESC OFFSET "#)
        .push_bind(skip)
        .push(" LIMIT ")
        .push_bind(q.limit);
    let rows: Vec<HistoryRow> = list.build_query_as().fetch_all(&state.db).await?;

    let mut count = QueryBuilder::<Postgres>::new("SELECT COUNT(*)");
    push_where(&mut count);
    let total: i64 = count.build_query_scalar().fetch_one(&state.db).await?;

    let data: Vec<Value> = rows
        .into_iter()
        .map(|(id, gid, gname, event, lat, lng, acc, ts)| {
            json!({
                "id": id,
                "geofenceId": gid,
                "geofenceName": gname,
                "event": event,
                "lat": js_num(lat),
                "lng": js_num(lng),
                "accuracy": js_num(acc),
                "timestamp": ts,
            })
        })
        .collect();
    Ok(json!({
        "data": data,
        "meta": {
            "total": total,
            "page": q.page,
            "limit": q.limit,
            "totalPages": (total + q.limit - 1) / q.limit,
        },
    }))
}

/// Node: src/mobile-geofences/mobile-geofences.controller.ts:208 `listEventHistory`
pub async fn h_event_history(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedQuery(q): ValidatedQuery<Raw>,
) -> ApiResult<Json<Value>> {
    let q = parse_history(&q.0).map_err(ApiError::Validation)?;
    Ok(Json(
        event_history(&state, ctx.require_user_id()?, &q).await?,
    ))
}

// ── catat event ──

#[derive(Debug)]
struct EventDto {
    geofence_id: Option<String>,
    lat: f64,
    lng: f64,
    accuracy: f64,
    timestamp: f64,
}

/// `GeofenceEventDto`.
fn parse_event(m: &Map<String, Value>) -> Result<EventDto, Vec<String>> {
    let mut errs = Vec::new();
    let mut geofence_id = None;
    if let Some(v) = present(m, "geofenceId") {
        match v {
            Value::String(s) if is_uuid(s) => geofence_id = Some(s.clone()),
            _ => errs.push("geofenceId must be a UUID".into()),
        }
    }
    let mut num = |k: &str| {
        let n = as_number(m.get(k));
        if n.is_none() {
            errs.push(format!(
                "{k} must be a number conforming to the specified constraints"
            ));
        }
        n.unwrap_or(0.0)
    };
    let lat = num("lat");
    let lng = num("lng");
    let accuracy = num("accuracy");
    let timestamp = num("timestamp");
    if errs.is_empty() {
        Ok(EventDto {
            geofence_id,
            lat,
            lng,
            accuracy,
            timestamp,
        })
    } else {
        Err(errs)
    }
}

/// `new Date(dto.timestamp)`: di luar ±8.64e15 = Invalid Date → Prisma menolak → 500.
fn event_timestamp(dto: &EventDto) -> ApiResult<JsDate> {
    #[allow(clippy::cast_possible_truncation)]
    if dto.timestamp.abs() <= 8.64e15 {
        JsDate::from_millis(dto.timestamp.trunc() as i64)
    } else {
        None
    }
    .ok_or_else(|| internal("Invalid Date (timestamp)"))
}

/// `logEvent`.
async fn log_event(state: &AppState, user_id: i32, dto: &EventDto) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;

    let (geofence_id, geofence_uuid, radius, distance): (i32, String, i32, i64) = if let Some(gid) =
        &dto.geofence_id
    {
        let uuid = parse_row_uuid(gid)?;
        let found: Option<Geofence> = sqlx::query_as(
                r#"SELECT id, uuid::text AS uuid, name, lat, lng, "radiusMeters", enforced,
                          "applicableTo"::text AS "applicableTo", COALESCE("applicableIds", '{}') AS "applicableIds"
                     FROM geofence WHERE uuid = $1 AND "companyId" = $2 AND "isActive" = true
                    ORDER BY id LIMIT 1"#,
            )
            .bind(uuid)
            .bind(company_id)
            .fetch_optional(&state.db)
            .await?;
        let found = found
            .filter(|g| g.applies_to(user_id))
            .ok_or_else(|| ApiError::not_found("Geofence not found or not accessible"))?;
        let d = geo::get_distance(dto.lat, dto.lng, found.lat, found.lng);
        (found.id, found.uuid, found.radius_meters, d)
    } else {
        let candidates = geo::active_geofences_for(&state.db, company_id, user_id).await?;
        let Some((chosen, d)) = geo::pick_geofence(&candidates, dto.lat, dto.lng) else {
            // User tanpa geofence sama sekali (mis. sales lapangan): tak ada enter/exit yang
            // bisa diputuskan, tapi posisinya tetap disimpan di `userLocationPing` supaya
            // perjalanannya tercatat (tampil di detail attendance sebagai `source: 'location'`).
            let ts = event_timestamp(dto)?;
            sqlx::query(
                r#"INSERT INTO "userLocationPing" ("companyId", "userId", lat, lng, accuracy, "timestamp")
                   VALUES ($1, $2, $3, $4, $5, $6)"#,
            )
            .bind(company_id)
            .bind(user_id)
            .bind(dto.lat)
            .bind(dto.lng)
            .bind(dto.accuracy)
            .bind(ts.0)
            .execute(&state.db)
            .await?;
            return Ok(json!({
                "logged": true,
                "geofenceId": null,
                "event": null,
                "distanceMeters": null,
            }));
        };
        (chosen.id, chosen.uuid.clone(), chosen.radius_meters, d)
    };

    let event = if distance <= i64::from(radius) {
        "enter"
    } else {
        "exit"
    };
    let ts = event_timestamp(dto)?;

    sqlx::query(
        r#"INSERT INTO "geofenceAuditLog" ("geofenceId", "userId", event, lat, lng, accuracy, "timestamp")
           VALUES ($1, $2, $3, $4, $5, $6, $7)"#,
    )
    .bind(geofence_id)
    .bind(user_id)
    .bind(event)
    .bind(dto.lat)
    .bind(dto.lng)
    .bind(dto.accuracy)
    .bind(ts.0)
    .execute(&state.db)
    .await?;
    Ok(json!({
        "logged": true,
        "geofenceId": geofence_uuid,
        "event": event,
        "distanceMeters": distance,
    }))
}

/// Node: src/mobile-geofences/mobile-geofences.controller.ts:243 `logEvent`
pub async fn h_log_event(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedJson(body): ValidatedJson<Raw>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let dto = parse_event(&body.0).map_err(ApiError::Validation)?;
    let data = log_event(&state, ctx.require_user_id()?, &dto).await?;
    Ok((StatusCode::CREATED, Json(json!({ "data": data }))))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn schedule_time_regex() {
        for ok in ["09:00 AM", "9:00 AM", "12:59 PM", "01:05 PM", "10:30 AM"] {
            assert!(is_schedule_time(ok), "{ok}");
        }
        for bad in [
            "13:00 AM",
            "00:30 AM",
            "9:60 AM",
            "9:00AM",
            "9:00 am",
            "9:0 AM",
            "09:00 AM ",
            "",
            "009:00 AM",
        ] {
            assert!(!is_schedule_time(bad), "{bad}");
        }
    }

    #[test]
    fn create_minimal_ok_and_missing_errors() {
        let d = parse(
            &obj(json!({"name": "K", "lat": -6.2, "lng": 106.8, "radiusMeters": 100})),
            true,
        )
        .unwrap();
        assert_eq!(d.radius_meters, Field::Val(100));
        let e = parse(&obj(json!({})), true).unwrap_err();
        assert_eq!(e[0], "name must be a string");
        assert!(e.contains(&"radiusMeters must not be less than 50".to_owned()));
    }

    #[test]
    fn nested_messages_have_parent_prefix() {
        let e = parse(
            &obj(json!({
                "weeklySchedule": {"monday": {"enabled": "x", "startTime": "25:00", "endTime": "9:00 AM"},
                                   "tuesday": 5},
                "breakTime": {"hours": 24, "minutes": 1.5},
                "applicableTo": "x",
                "applicableIds": [1, "a"],
            })),
            false,
        )
        .unwrap_err();
        assert_eq!(
            e,
            vec![
                "applicableTo must be one of the following values: ",
                "each value in applicableIds must be an integer number",
                "weeklySchedule.monday.enabled must be a boolean value",
                "weeklySchedule.monday.startTime must be in format \"HH:MM AM/PM\"",
                "weeklySchedule.nested property tuesday must be either object or array",
                "breakTime.hours must not be greater than 23",
                "breakTime.minutes must be an integer number",
            ]
        );
        let e = parse(
            &obj(json!({"weeklySchedule": "x", "applicableIds": 5})),
            false,
        )
        .unwrap_err();
        assert_eq!(
            e,
            vec![
                "applicableIds must be an array",
                "nested property weeklySchedule must be either object or array"
            ]
        );
    }

    #[test]
    fn update_null_defers_to_write() {
        let d = parse(
            &obj(json!({"name": null, "address": null, "isActive": null})),
            false,
        )
        .unwrap();
        assert_eq!(d.name, Field::Null);
        assert_eq!(d.address, Field::Null);
        assert!(defined(&d.name).is_err());
        assert!(defined(&d.is_active).is_err());
    }

    #[test]
    fn nearby_query_rules() {
        let (lat, lng, limit) =
            parse_nearby(&obj(json!({"lat": "-6.2abc", "lng": "106.8"}))).unwrap();
        assert_eq!((lat, lng, limit), (-6.2, 106.8, None));
        let e = parse_nearby(&obj(json!({"lat": "x", "limit": "99"}))).unwrap_err();
        assert_eq!(
            e,
            vec![
                "lat must be a number conforming to the specified constraints",
                "lng must be a number conforming to the specified constraints",
                "limit must not be greater than 50",
            ]
        );
        let (_, _, l) = parse_nearby(&obj(json!({"lat": "1", "lng": "2", "limit": ""}))).unwrap();
        assert_eq!(l, Some(20.0));
        assert_eq!(slice_end(5, -1), 4);
        assert_eq!(slice_end(5, 0), 0);
        assert_eq!(slice_end(5, 50), 5);
        assert_eq!(slice_end(2, -5), 0);
    }

    #[test]
    fn history_query_rules() {
        let q = parse_history(&obj(json!({}))).unwrap();
        assert_eq!((q.page, q.limit), (1, 20));
        let e = parse_history(&obj(json!({
            "geofenceId": "x", "event": "in", "startDate": "nope", "page": "0", "limit": "101"
        })))
        .unwrap_err();
        assert_eq!(
            e,
            vec![
                "geofenceId must be a UUID",
                "event must be one of the following values: enter, exit",
                "startDate must be a valid ISO 8601 date string",
                "page must not be less than 1",
                "limit must not be greater than 100",
            ]
        );
    }

    #[test]
    fn event_dto_requires_numbers() {
        let e = parse_event(&obj(json!({"lat": "1"}))).unwrap_err();
        assert_eq!(e.len(), 4);
        let d = parse_event(&obj(
            json!({"lat": 1, "lng": 2, "accuracy": 3, "timestamp": 4}),
        ))
        .unwrap();
        assert_eq!(d.geofence_id, None);
        assert!(d.timestamp > 0.0 && d.accuracy > 0.0 && d.lat > 0.0 && d.lng > 0.0);
    }

    #[test]
    fn dates_parse_strictly() {
        assert!(date_at("2026-06-01", "00:00:00.000Z").is_ok());
        assert!(date_at("2026-06-01T10:00:00Z", "00:00:00.000Z").is_err());
    }
}
