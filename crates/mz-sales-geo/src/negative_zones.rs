//! Port `src/negative-zones/*`: admin (`/v1/admin/negative-zones`, 4 route) + mobile
//! (`/mobile/negative-zones`, 1 route). Model `negativeZone` ber-audit (`AuditHandle`).

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use mz_audit::{AuditHandle, Lookup, Model};
use mz_core::company::require_company_id;
use mz_core::extract::{ValidatedJson, check_int_range, validator_js_length};
use mz_core::jsfmt::num as js_num;
use mz_core::time::{JsDate, now_naive};
use mz_core::{ApiError, ApiResult, AppState, RequestCtx};
use serde_json::{Map, Value, json};
use sqlx::{Postgres, QueryBuilder};

use crate::dto::{Field, Raw, as_number, check_bool, check_number, gate, present};
use crate::geofences::{internal, parse_row_uuid};

/// Baris `negativeZone` (semua kolom skalar, urutan `schema.prisma`).
#[derive(Debug, sqlx::FromRow)]
pub struct NegativeZoneRow {
    pub id: i32,
    pub uuid: String,
    #[sqlx(rename = "companyId")]
    pub company_id: i32,
    pub name: String,
    pub lat: f64,
    pub lng: f64,
    #[sqlx(rename = "radiusMeters")]
    pub radius_meters: i32,
    #[sqlx(rename = "isActive")]
    pub is_active: bool,
    #[sqlx(rename = "createdById")]
    pub created_by_id: i32,
    #[sqlx(rename = "createdAt")]
    pub created_at: JsDate,
    #[sqlx(rename = "updatedAt")]
    pub updated_at: Option<JsDate>,
}

const COLUMNS: &str = r#"id, uuid::text AS uuid, "companyId", name, lat, lng, "radiusMeters",
    "isActive", "createdById", "createdAt", "updatedAt""#;

impl NegativeZoneRow {
    /// Bentuk Prisma (semua kolom, urutan skema).
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "uuid": self.uuid,
            "companyId": self.company_id,
            "name": self.name,
            "lat": js_num(self.lat),
            "lng": js_num(self.lng),
            "radiusMeters": self.radius_meters,
            "isActive": self.is_active,
            "createdById": self.created_by_id,
            "createdAt": self.created_at,
            "updatedAt": self.updated_at,
        })
    }
}

// ───────────────────────── DTO ─────────────────────────

#[derive(Debug, Default)]
struct Dto {
    name: Field<String>,
    lat: Field<f64>,
    lng: Field<f64>,
    radius_meters: Field<i32>,
    is_active: Field<bool>,
}

/// `@IsString() @MinLength(1) @MaxLength(255)` (pesan: Max, Min, IsString).
fn check_name(errs: &mut Vec<String>, v: Option<&Value>) -> Option<String> {
    let s = match v {
        Some(Value::String(s)) => Some(s.as_str()),
        _ => None,
    };
    let len = s.map(validator_js_length);
    if len.is_none_or(|l| l > 255) {
        errs.push("name must be shorter than or equal to 255 characters".into());
    }
    if len.is_none_or(|l| l < 1) {
        errs.push("name must be longer than or equal to 1 characters".into());
    }
    if s.is_none() {
        errs.push("name must be a string".into());
    }
    s.filter(|_| len.is_some_and(|l| (1..=255).contains(&l)))
        .map(str::to_owned)
}

/// `CreateNegativeZoneDto` (`required = true`) / `UpdateNegativeZoneDto` (`PartialType`: semua
/// `@IsOptional()`, jadi `null` melewati validasi).
fn parse(m: &Map<String, Value>, required: bool) -> Result<Dto, Vec<String>> {
    let mut errs = Vec::new();
    let mut dto = Dto::default();
    let take = |key: &str| gate(m, key, required);
    // name
    if let Some(v) = take("name") {
        if let Some(s) = check_name(&mut errs, v) {
            dto.name = Field::Val(s);
        }
    } else if m.contains_key("name") {
        dto.name = Field::Null;
    }
    // lat, lng
    for (key, min, max) in [("lat", -90.0, 90.0), ("lng", -180.0, 180.0)] {
        let slot = if key == "lat" {
            &mut dto.lat
        } else {
            &mut dto.lng
        };
        if let Some(v) = take(key) {
            if let Some(n) = check_number(&mut errs, key, as_number(v), Some(min), Some(max)) {
                *slot = Field::Val(n);
            }
        } else if m.contains_key(key) {
            *slot = Field::Null;
        }
    }
    // radiusMeters
    if let Some(v) = take("radiusMeters") {
        let n = match v {
            Some(Value::Number(n)) => n.as_f64(),
            _ => None,
        };
        if let Some(i) = check_int_range(&mut errs, "radiusMeters", n, 10, 5000)
            && let Ok(i) = i32::try_from(i)
        {
            dto.radius_meters = Field::Val(i);
        }
    } else if m.contains_key("radiusMeters") {
        dto.radius_meters = Field::Null;
    }
    // isActive (selalu opsional)
    if let Some(v) = present(m, "isActive") {
        if let Some(b) = check_bool(&mut errs, "isActive", Some(v)) {
            dto.is_active = Field::Val(b);
        }
    } else if m.contains_key("isActive") {
        dto.is_active = Field::Null;
    }
    if errs.is_empty() { Ok(dto) } else { Err(errs) }
}

/// Nilai yang dikirim: `null` pada kolom non-null = Prisma melempar → 500.
fn defined<T: Clone>(f: &Field<T>) -> ApiResult<Option<T>> {
    match f {
        Field::Absent => Ok(None),
        Field::Null => Err(internal("Argument must not be null")),
        Field::Val(v) => Ok(Some(v.clone())),
    }
}

// ───────────────────────── service ─────────────────────────

async fn find_in_company(
    state: &AppState,
    company_id: i32,
    raw_uuid: &str,
) -> ApiResult<NegativeZoneRow> {
    let uuid = parse_row_uuid(raw_uuid)?;
    let row: Option<NegativeZoneRow> = sqlx::query_as(&format!(
        r#"SELECT {COLUMNS} FROM "negativeZone" WHERE uuid = $1 AND "companyId" = $2 ORDER BY id LIMIT 1"#
    ))
    .bind(uuid)
    .bind(company_id)
    .fetch_optional(&state.db)
    .await?;
    row.ok_or_else(|| ApiError::not_found("Negative zone not found"))
}

/// `listNegativeZonesAdmin`.
pub async fn list_admin(state: &AppState, user_id: i32) -> ApiResult<Vec<NegativeZoneRow>> {
    let company_id = require_company_id(&state.db, user_id).await?;
    Ok(sqlx::query_as(&format!(
        r#"SELECT {COLUMNS} FROM "negativeZone" WHERE "companyId" = $1 ORDER BY id ASC"#
    ))
    .bind(company_id)
    .fetch_all(&state.db)
    .await?)
}

/// `listNegativeZones` (hanya aktif).
pub async fn list_active(state: &AppState, user_id: i32) -> ApiResult<Vec<NegativeZoneRow>> {
    let company_id = require_company_id(&state.db, user_id).await?;
    Ok(sqlx::query_as(&format!(
        r#"SELECT {COLUMNS} FROM "negativeZone" WHERE "companyId" = $1 AND "isActive" = true ORDER BY id ASC"#
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
) -> ApiResult<NegativeZoneRow> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let (Field::Val(name), Field::Val(lat), Field::Val(lng), Field::Val(radius)) =
        (&dto.name, &dto.lat, &dto.lng, &dto.radius_meters)
    else {
        return Err(internal("negative zone DTO incomplete"));
    };
    let is_active = defined(&dto.is_active)?;
    let mut qb = QueryBuilder::<Postgres>::new(
        r#"INSERT INTO "negativeZone" ("companyId", name, lat, lng, "radiusMeters", "createdById""#,
    );
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
        .push_bind(user_id);
    if let Some(a) = is_active {
        qb.push(", ").push_bind(a);
    }
    qb.push(") RETURNING id");
    let id: i32 = qb.build_query_scalar().fetch_one(&state.db).await?;
    if let Some(r) = audit
        .result_row(&state.db, Model::NegativeZone, Lookup::Id(id))
        .await
    {
        audit.record_create(Model::NegativeZone, &r).await;
    }
    Ok(sqlx::query_as(&format!(
        r#"SELECT {COLUMNS} FROM "negativeZone" WHERE id = $1"#
    ))
    .bind(id)
    .fetch_one(&state.db)
    .await?)
}

async fn update(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    raw_uuid: &str,
    dto: &Dto,
) -> ApiResult<NegativeZoneRow> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let zone = find_in_company(state, company_id, raw_uuid).await?;
    let (name, lat, lng) = (defined(&dto.name)?, defined(&dto.lat)?, defined(&dto.lng)?);
    let (radius, is_active) = (defined(&dto.radius_meters)?, defined(&dto.is_active)?);

    let pre = audit
        .pre_read(Model::NegativeZone, Lookup::Id(zone.id))
        .await;
    let mut qb = QueryBuilder::<Postgres>::new(r#"UPDATE "negativeZone" SET "#);
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
    if let Some(v) = is_active {
        qb.push(r#""isActive" = "#).push_bind(v).push(", ");
    }
    qb.push(r#""updatedAt" = "#)
        .push_bind(now_naive())
        .push(" WHERE id = ")
        .push_bind(zone.id);
    qb.build().execute(&state.db).await?;
    if let Some(r) = audit
        .result_row(&state.db, Model::NegativeZone, Lookup::Id(zone.id))
        .await
    {
        audit.record_update(Model::NegativeZone, pre, &r).await;
    }
    Ok(sqlx::query_as(&format!(
        r#"SELECT {COLUMNS} FROM "negativeZone" WHERE id = $1"#
    ))
    .bind(zone.id)
    .fetch_one(&state.db)
    .await?)
}

async fn remove(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    raw_uuid: &str,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let zone = find_in_company(state, company_id, raw_uuid).await?;
    let pre = audit
        .pre_read(Model::NegativeZone, Lookup::Id(zone.id))
        .await;
    sqlx::query(r#"UPDATE "negativeZone" SET "isActive" = false, "updatedAt" = $2 WHERE id = $1"#)
        .bind(zone.id)
        .bind(now_naive())
        .execute(&state.db)
        .await?;
    if let Some(r) = audit
        .result_row(&state.db, Model::NegativeZone, Lookup::Id(zone.id))
        .await
    {
        audit.record_update(Model::NegativeZone, pre, &r).await;
    }
    Ok(json!({ "deleted": true }))
}

// ───────────────────────── handler ─────────────────────────

/// Node: src/negative-zones/admin-negative-zones.controller.ts:57 `list`
pub async fn h_admin_list(
    State(state): State<AppState>,
    ctx: RequestCtx,
) -> ApiResult<Json<Value>> {
    let rows = list_admin(&state, ctx.require_user_id()?).await?;
    Ok(Json(
        json!({ "data": rows.iter().map(NegativeZoneRow::to_json).collect::<Vec<_>>() }),
    ))
}

/// Node: src/negative-zones/admin-negative-zones.controller.ts:89 `create`
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

/// Node: src/negative-zones/admin-negative-zones.controller.ts:137 `update`
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

/// Node: src/negative-zones/admin-negative-zones.controller.ts:187 `remove`
pub async fn h_admin_remove(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let data = remove(&state, &audit, ctx.require_user_id()?, &id).await?;
    Ok(Json(json!({ "data": data })))
}

/// Node: src/negative-zones/mobile-negative-zones.controller.ts:32 `list`
pub async fn h_mobile_list(
    State(state): State<AppState>,
    ctx: RequestCtx,
) -> ApiResult<Json<Value>> {
    let rows = list_active(&state, ctx.require_user_id()?).await?;
    let data: Vec<Value> = rows
        .iter()
        .map(|z| {
            json!({
                "id": z.uuid,
                "name": z.name,
                "lat": js_num(z.lat),
                "lng": js_num(z.lng),
                "radiusMeters": z.radius_meters,
            })
        })
        .collect();
    Ok(Json(json!({ "data": data })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn create_requires_fields() {
        let e = parse(&obj(json!({})), true).unwrap_err();
        assert_eq!(
            e,
            vec![
                "name must be shorter than or equal to 255 characters",
                "name must be longer than or equal to 1 characters",
                "name must be a string",
                "lat must not be greater than 90",
                "lat must not be less than -90",
                "lat must be a number conforming to the specified constraints",
                "lng must not be greater than 180",
                "lng must not be less than -180",
                "lng must be a number conforming to the specified constraints",
                "radiusMeters must not be greater than 5000",
                "radiusMeters must not be less than 10",
                "radiusMeters must be an integer number",
            ]
        );
    }

    #[test]
    fn create_range_errors() {
        let e = parse(
            &obj(json!({"name": "", "lat": 1, "lng": 2, "radiusMeters": 5, "isActive": "x"})),
            true,
        )
        .unwrap_err();
        assert_eq!(
            e,
            vec![
                "name must be longer than or equal to 1 characters",
                "radiusMeters must not be less than 10",
                "isActive must be a boolean value"
            ]
        );
    }

    #[test]
    fn update_is_partial_and_null_passes() {
        let d = parse(&obj(json!({"name": null, "radiusMeters": 120})), false).unwrap();
        assert_eq!(d.name, Field::Null);
        assert_eq!(d.radius_meters, Field::Val(120));
        assert_eq!(d.lat, Field::Absent);
        assert!(parse(&obj(json!({"lat": 99})), false).is_err());
        assert!(defined(&d.name).is_err());
    }
}
