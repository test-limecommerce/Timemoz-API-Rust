//! Port `src/admin-global/national-holidays.{controller,service}.ts` + DTO: libur nasional
//! global (`admin/national-holidays`). `POST /admin/national-holidays/apply` TIDAK di-port:
//! ia memanggil `LeaveJobsService.enqueueHolidaySync` (antrean Bull `leave-schedule`, modul
//! cuti masih Node) — lihat `plan/deviations/mz-admin-jobs.md`.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use chrono::{Datelike, NaiveDate};
use mz_core::qs::QVal;
use mz_core::{ApiError, ApiResult, AppState};
use mz_operator::{AuditCtx, Operator, OperatorAuditEntry, OperatorRole, record};
use serde_json::{Value, json};

use crate::validate::{Errors, RawBody, RawPath, RawQuery, present, qval_json, reason, trimmed};

const READ: &[OperatorRole] = &[
    OperatorRole::Owner,
    OperatorRole::Finance,
    OperatorRole::Support,
];
const WRITE: &[OperatorRole] = &[OperatorRole::Owner, OperatorRole::Support];
const HOLIDAY_TYPES: &[&str] = &["NATIONAL_HOLIDAY", "JOINT_LEAVE"];

fn duplicate() -> ApiError {
    ApiError::coded(
        StatusCode::CONFLICT,
        "DUPLICATE_HOLIDAY",
        "a national holiday with this date and name already exists",
    )
}

fn not_found() -> ApiError {
    ApiError::coded(
        StatusCode::NOT_FOUND,
        "HOLIDAY_NOT_FOUND",
        "national holiday not found",
    )
}

/// `parseDateKey(value)`: `YYYY-MM-DD` → tanggal; tanggal yang tidak ada di kalender → 400.
pub fn parse_date_key(value: Option<&str>) -> Result<NaiveDate, ApiError> {
    value
        .and_then(|v| {
            NaiveDate::parse_from_str(v, "%Y-%m-%d")
                .ok()
                .filter(|d| d.format("%Y-%m-%d").to_string() == v)
        })
        .ok_or_else(|| {
            ApiError::coded(
                StatusCode::BAD_REQUEST,
                "INVALID_DATE",
                "date is not a valid calendar date",
            )
        })
}

/// Baris `nationalHoliday` untuk `snapshot(row)`.
#[derive(Debug, Clone, sqlx::FromRow)]
struct HolidayRow {
    uuid: uuid::Uuid,
    date: NaiveDate,
    name: String,
    #[sqlx(rename = "type")]
    ty: String,
    year: i32,
    #[sqlx(rename = "isEstimated")]
    is_estimated: bool,
}

const COLS: &str = r#"uuid, date, name, type::text AS type, year, "isEstimated""#;

/// `snapshot(r)`.
fn snapshot(r: &HolidayRow) -> Value {
    json!({
        "uuid": r.uuid.to_string(),
        "date": r.date.format("%Y-%m-%d").to_string(),
        "name": r.name,
        "type": r.ty,
        "year": r.year,
        "isEstimated": r.is_estimated,
    })
}

const DATE_MSG: &str = "date must be YYYY-MM-DD";

fn date_ok(v: Option<&Value>) -> bool {
    v.and_then(Value::as_str)
        .is_some_and(crate::validate::is_date_only)
}

/// `CreateNationalHolidayDto`.
#[derive(Debug)]
pub struct CreateDto {
    pub date: String,
    pub name: String,
    pub ty: String,
    pub is_estimated: bool,
    pub reason: Option<String>,
}

pub fn validate_create(body: &Value) -> Result<CreateDto, ApiError> {
    let mut e = Errors::default();
    e.unknown_body(body, &["date", "name", "type", "isEstimated", "reason"]);
    let date = trimmed(body.get("date"));
    if !date_ok(date.as_ref()) {
        e.push(DATE_MSG);
    }
    let name = trimmed(body.get("name"));
    e.length("name", name.as_ref(), 1, 200);
    e.string("name", name.as_ref());
    e.one_of("type", body.get("type"), HOLIDAY_TYPES);
    e.boolean("isEstimated", body.get("isEstimated"));
    let reason = optional_reason_field(&mut e, body);
    e.into_result()?;
    let s = |v: Option<Value>| {
        v.and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default()
    };
    Ok(CreateDto {
        date: s(date),
        name: s(name),
        ty: body
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        is_estimated: body
            .get("isEstimated")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        reason,
    })
}

/// `reason?` (`@IsOptional @Transform(trim) @IsString @Length(5, 500)`).
fn optional_reason_field(e: &mut Errors, body: &Value) -> Option<String> {
    let v = trimmed(body.get("reason"));
    if !present(v.as_ref()) {
        return None;
    }
    e.length("reason", v.as_ref(), 5, 500);
    e.string("reason", v.as_ref());
    v.and_then(|v| v.as_str().map(str::to_owned))
}

/// `UpdateNationalHolidayDto`: `None` = properti tidak dikirim (`undefined`); `Some(Null)` = null.
#[derive(Debug, Default)]
pub struct UpdateDto {
    pub date: Option<Value>,
    pub name: Option<Value>,
    pub ty: Option<Value>,
    pub is_estimated: Option<Value>,
    pub reason: Option<String>,
}

pub fn validate_update(body: &Value) -> Result<UpdateDto, ApiError> {
    let mut e = Errors::default();
    e.unknown_body(body, &["date", "name", "type", "isEstimated", "reason"]);
    let date = trimmed(body.get("date"));
    if present(date.as_ref()) && !date_ok(date.as_ref()) {
        e.push(DATE_MSG);
    }
    let name = trimmed(body.get("name"));
    if present(name.as_ref()) {
        e.length("name", name.as_ref(), 1, 200);
        e.string("name", name.as_ref());
    }
    if present(body.get("type")) {
        e.one_of("type", body.get("type"), HOLIDAY_TYPES);
    }
    if present(body.get("isEstimated")) {
        e.boolean("isEstimated", body.get("isEstimated"));
    }
    let reason = optional_reason_field(&mut e, body);
    e.into_result()?;
    Ok(UpdateDto {
        date,
        name,
        ty: body.get("type").cloned(),
        is_estimated: body.get("isEstimated").cloned(),
        reason,
    })
}

pub fn validate_delete(body: &Value) -> Result<Option<String>, ApiError> {
    let mut e = Errors::default();
    e.unknown_body(body, &["reason"]);
    let r = reason(&mut e, body);
    e.into_result()?;
    Ok(r)
}

/// `NationalHolidayListQueryDto` (`@Type(() => Number) @IsInt @Min(2000) @Max(2100)`).
pub fn validate_list(q: &[(String, QVal)]) -> Result<i32, ApiError> {
    let mut e = Errors::default();
    e.unknown_query(q, &["year"]);
    let year = e.query_int_range("year", mz_core::qs::get(q, "year"), 2000, 2100);
    e.into_result()?;
    let _ = qval_json;
    Ok(year.and_then(|y| i32::try_from(y).ok()).unwrap_or_default())
}

/// Node: src/admin-global/national-holidays.controller.ts:55 (`list`).
pub async fn h_list(
    State(state): State<AppState>,
    op: Operator,
    RawQuery(q): RawQuery,
) -> ApiResult<Json<Value>> {
    op.require(READ)?;
    let year = validate_list(&q)?;
    let rows: Vec<HolidayRow> = sqlx::query_as(&format!(
        r#"SELECT {COLS} FROM "nationalHoliday" WHERE year = $1 ORDER BY date ASC, name ASC"#
    ))
    .bind(year)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(
        json!({ "data": rows.iter().map(snapshot).collect::<Vec<_>>() }),
    ))
}

/// Node: src/admin-global/national-holidays.controller.ts:79 (`create`).
pub async fn h_create(
    State(state): State<AppState>,
    op: Operator,
    ctx: AuditCtx,
    RawBody(body): RawBody,
) -> ApiResult<(StatusCode, Json<Value>)> {
    op.require(WRITE)?;
    let dto = validate_create(&body)?;
    let date = parse_date_key(Some(&dto.date))?;
    let mut tx = state.db.begin().await?;
    let row: Result<HolidayRow, sqlx::Error> = sqlx::query_as(&format!(
        r#"INSERT INTO "nationalHoliday" (date, name, type, "isEstimated", year)
           VALUES ($1, $2, $3::"HolidayType", $4, $5) RETURNING {COLS}"#
    ))
    .bind(date)
    .bind(&dto.name)
    .bind(&dto.ty)
    .bind(dto.is_estimated)
    .bind(date.year())
    .fetch_one(&mut *tx)
    .await;
    let row = match row {
        Ok(r) => r,
        Err(e) if mz_core::db::is_unique_violation(&e) => return Err(duplicate()),
        Err(e) => return Err(e.into()),
    };
    let snap = snapshot(&row);
    record(
        &mut *tx,
        &ctx,
        &OperatorAuditEntry {
            operator_id: op.id,
            action: "national_holiday.create",
            target_type: "nationalHoliday",
            target_uuid: Some(row.uuid),
            company_id: None,
            before: None,
            after: Some(&snap),
            reason: dto.reason.as_deref(),
        },
    )
    .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(json!({ "data": snap }))))
}

fn opt_text(v: &Option<Value>) -> Option<String> {
    v.as_ref().and_then(|v| v.as_str().map(str::to_owned))
}

/// Node: src/admin-global/national-holidays.controller.ts:87 (`update`).
pub async fn h_update(
    State(state): State<AppState>,
    op: Operator,
    ctx: AuditCtx,
    RawPath(raw): RawPath,
    RawBody(body): RawBody,
) -> ApiResult<Json<Value>> {
    op.require(WRITE)?;
    // Node: galat `ValidationPipe` body (pipe global) muncul sebelum `ParseUUIDPipe` param.
    let dto = validate_update(&body)?;
    let uuid = mz_core::uuid_rules::parse_uuid_pipe(&raw)?;
    // `dto.date !== undefined ? parseDateKey(dto.date)` — null pun diparse (→ 400).
    let date = match &dto.date {
        None => None,
        Some(v) => Some(parse_date_key(v.as_str())?),
    };
    let mut tx = state.db.begin().await?;
    let before: Option<HolidayRow> = sqlx::query_as(&format!(
        r#"SELECT {COLS} FROM "nationalHoliday" WHERE uuid = $1"#
    ))
    .bind(uuid)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(before) = before else {
        return Err(not_found());
    };
    let updated: Result<HolidayRow, sqlx::Error> = sqlx::query_as(&format!(
        r#"UPDATE "nationalHoliday" SET
              date = CASE WHEN $2 THEN $3 ELSE date END,
              year = CASE WHEN $2 THEN $4 ELSE year END,
              name = CASE WHEN $5 THEN $6 ELSE name END,
              type = CASE WHEN $7 THEN $8::"HolidayType" ELSE type END,
              "isEstimated" = CASE WHEN $9 THEN $10 ELSE "isEstimated" END,
              "updatedAt" = $11
            WHERE uuid = $1 RETURNING {COLS}"#
    ))
    .bind(uuid)
    .bind(date.is_some())
    .bind(date)
    .bind(date.map(|d| d.year()))
    .bind(dto.name.is_some())
    .bind(opt_text(&dto.name))
    .bind(dto.ty.is_some())
    .bind(opt_text(&dto.ty))
    .bind(dto.is_estimated.is_some())
    .bind(dto.is_estimated.as_ref().and_then(Value::as_bool))
    .bind(mz_core::time::now_naive())
    .fetch_one(&mut *tx)
    .await;
    let updated = match updated {
        Ok(r) => r,
        Err(e) if mz_core::db::is_unique_violation(&e) => return Err(duplicate()),
        Err(e) => return Err(e.into()),
    };
    let (b, a) = (snapshot(&before), snapshot(&updated));
    record(
        &mut *tx,
        &ctx,
        &OperatorAuditEntry {
            operator_id: op.id,
            action: "national_holiday.update",
            target_type: "nationalHoliday",
            target_uuid: Some(uuid),
            company_id: None,
            before: Some(&b),
            after: Some(&a),
            reason: dto.reason.as_deref(),
        },
    )
    .await?;
    tx.commit().await?;
    Ok(Json(json!({ "data": a })))
}

/// Node: src/admin-global/national-holidays.controller.ts:100 (`remove`). Hanya baris global;
/// salinan di `companyHoliday` tidak disentuh.
pub async fn h_remove(
    State(state): State<AppState>,
    op: Operator,
    ctx: AuditCtx,
    RawPath(raw): RawPath,
    RawBody(body): RawBody,
) -> ApiResult<Json<Value>> {
    op.require(WRITE)?;
    // Node: galat body muncul sebelum `ParseUUIDPipe` (lihat `h_update`).
    let reason = validate_delete(&body)?;
    let uuid = mz_core::uuid_rules::parse_uuid_pipe(&raw)?;
    let mut tx = state.db.begin().await?;
    let before: Option<HolidayRow> = sqlx::query_as(&format!(
        r#"SELECT {COLS} FROM "nationalHoliday" WHERE uuid = $1"#
    ))
    .bind(uuid)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(before) = before else {
        return Err(not_found());
    };
    sqlx::query(r#"DELETE FROM "nationalHoliday" WHERE uuid = $1"#)
        .bind(uuid)
        .execute(&mut *tx)
        .await?;
    let b = snapshot(&before);
    record(
        &mut *tx,
        &ctx,
        &OperatorAuditEntry {
            operator_id: op.id,
            action: "national_holiday.delete",
            target_type: "nationalHoliday",
            target_uuid: Some(uuid),
            company_id: None,
            before: Some(&b),
            after: None,
            reason: reason.as_deref(),
        },
    )
    .await?;
    tx.commit().await?;
    Ok(Json(json!({ "data": { "deleted": true } })))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn golden() -> Value {
        serde_json::from_str(include_str!("../tests/golden/validation.json")).unwrap()
    }

    fn errs<T>(r: Result<T, ApiError>) -> Option<Vec<String>> {
        match r {
            Ok(_) => None,
            Err(ApiError::Validation(v)) => Some(v),
            Err(o) => Some(vec![format!("OTHER {o}")]),
        }
    }

    fn want(case: &Value) -> Option<Vec<String>> {
        case.get("errors").map(|e| {
            e.as_array()
                .unwrap()
                .iter()
                .map(|s| s.as_str().unwrap().to_owned())
                .collect()
        })
    }

    #[test]
    fn validasi_identik_dengan_node() {
        let g = golden();
        for c in g["holidayCreate"].as_array().unwrap() {
            assert_eq!(
                errs(validate_create(&c["input"])),
                want(c),
                "{}",
                c["input"]
            );
        }
        for c in g["holidayUpdate"].as_array().unwrap() {
            assert_eq!(
                errs(validate_update(&c["input"])),
                want(c),
                "{}",
                c["input"]
            );
        }
        for c in g["holidayDelete"].as_array().unwrap() {
            assert_eq!(
                errs(validate_delete(&c["input"])),
                want(c),
                "{}",
                c["input"]
            );
        }
        for c in g["holidayList"].as_array().unwrap() {
            let q = mz_core::qs::parse(c["input"].as_str().unwrap());
            assert_eq!(errs(validate_list(&q)), want(c), "{}", c["input"]);
        }
    }

    #[test]
    fn tanggal_kalender() {
        assert!(parse_date_key(Some("2027-02-28")).is_ok());
        assert!(parse_date_key(Some("2027-02-30")).is_err());
        assert!(parse_date_key(None).is_err());
        let dto = validate_create(&json!({
            "date": "2027-03-10", "name": " Nyepi ", "type": "JOINT_LEAVE", "isEstimated": true, "reason": "  Cuti bersama  "
        }))
        .unwrap();
        assert_eq!(dto.name, "Nyepi");
        assert_eq!(dto.reason.as_deref(), Some("Cuti bersama"));
    }
}
