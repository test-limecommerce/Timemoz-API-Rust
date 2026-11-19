//! `POST /admin/national-holidays/apply` (operator) — port
//! `src/admin-global/national-holidays.{controller,service}.ts` (`apply`, `applyToCompany`,
//! handler job `APPLY_HOLIDAYS`) + `ApplyNationalHolidaysDto`. CRUD `national-holidays` lain
//! ada di `mz-admin-jobs` (tidak diubah).
//!
//! `@PermissionExempt('separate-auth')` + `@AllowWhenSubscriptionLocked` + `JwtAdminAuthGuard`
//! (extractor [`Operator`]) + `@OperatorRoles('OWNER', 'SUPPORT')` + `ADMIN_VALIDATION_PIPE`.

use std::collections::HashSet;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use chrono::NaiveDate;
use mz_admin_jobs::holidays::{validate_delete, validate_update};
use mz_admin_jobs::jobs::service::{CreateJob, JobType, create_job};
use mz_admin_jobs::validate::{Errors, RawBody, present, reason};
use mz_core::{ApiError, ApiResult, AppState};
use mz_leave::jobs::{HolidaySyncPayload, enqueue_holiday_sync};
use mz_operator::{AuditCtx, Operator, OperatorAuditEntry, OperatorRole, record};
use serde_json::{Map, Value, json};

const WRITE: &[OperatorRole] = &[OperatorRole::Owner, OperatorRole::Support];

fn coded(status: StatusCode, code: &str, message: &str) -> ApiError {
    ApiError::coded(status, code, message)
}

/// `ApplyNationalHolidaysDto` sesudah validasi.
#[derive(Debug, Clone)]
pub struct ApplyDto {
    pub year: i32,
    pub company_uuids: Option<Vec<String>>,
    pub all: Option<bool>,
    pub dry_run: bool,
    pub reason: String,
}

/// `@Type(() => Number)` pada nilai JSON (`Number(value)`; `null`/`undefined` tetap).
fn year_number(v: Option<&Value>) -> Option<f64> {
    match v {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => Some(mz_core::jsfmt::number(s)),
        Some(Value::Bool(b)) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

/// Validasi DTO (properti kelas sendiri dulu: year, companyUuids, all, dryRun; lalu `reason`
/// warisan `ReasonDto`). `ADMIN_VALIDATION_PIPE`: properti asing ditolak.
pub fn validate_apply(body: &Value) -> Result<ApplyDto, ApiError> {
    let mut e = Errors::default();
    e.unknown_body(body, &["year", "companyUuids", "all", "dryRun", "reason"]);

    let year = mz_core::extract::check_int_range(
        &mut e.0,
        "year",
        year_number(body.get("year")),
        2000,
        2100,
    );

    // companyUuids: IsOptional, ArrayMinSize(1), ArrayMaxSize(200), IsUUID('all', each),
    // IsArray (urutan pesan dari bawah ke atas; `each` pada nilai non-array memeriksa nilai itu).
    let mut uuids: Option<Vec<String>> = None;
    if let Some(v) = body.get("companyUuids").filter(|v| present(Some(v))) {
        let uuid_ok = |x: &Value| x.as_str().is_some_and(mz_core::uuid_rules::is_uuid);
        let each_ok = match v {
            Value::Array(items) => items.iter().all(uuid_ok),
            other => uuid_ok(other),
        };
        if !each_ok {
            e.push("each value in companyUuids must be a UUID");
        }
        let items = v.as_array();
        if items.is_none_or(|i| i.len() > 200) {
            e.push("companyUuids must contain no more than 200 elements");
        }
        if items.is_none_or(Vec::is_empty) {
            e.push("companyUuids must contain at least 1 elements");
        }
        if items.is_none() {
            e.push("companyUuids must be an array");
        }
        uuids = items.map(|i| {
            i.iter()
                .filter_map(|x| x.as_str().map(str::to_owned))
                .collect()
        });
    }
    let all = body.get("all").filter(|v| present(Some(v)));
    if all.is_some() {
        e.boolean("all", all);
    }
    e.boolean("dryRun", body.get("dryRun"));
    let reason = reason(&mut e, body);
    e.into_result()?;
    Ok(ApplyDto {
        year: year.and_then(|y| i32::try_from(y).ok()).unwrap_or_default(),
        company_uuids: uuids,
        all: all.and_then(Value::as_bool),
        dry_run: body
            .get("dryRun")
            .and_then(Value::as_bool)
            .unwrap_or_default(),
        reason: reason.unwrap_or_default(),
    })
}

/// Satu libur yang ditambahkan (`id` kosong pada dry run).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Created {
    pub id: Option<i32>,
    pub date: String,
    pub name: String,
    pub ty: String,
}

/// `ApplyCompanyResult`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApplyCompanyResult {
    pub created: Vec<Created>,
    pub skipped: usize,
}

fn key(d: NaiveDate) -> String {
    d.format("%Y-%m-%d").to_string()
}

/// Node: src/admin-global/national-holidays.service.ts:233 (`applyToCompany`). Inti apply per
/// company (endpoint sinkron dan handler job `APPLY_HOLIDAYS`). Kunci advisory SAMA dengan
/// impor CSV (`holiday-import:<companyId>`). Tidak pernah menghapus; yang sudah ada
/// `(date, lower(name))` dilewati. Sesudah commit memasang `leave.holiday-sync` per baris baru.
pub async fn apply_to_company(
    state: &AppState,
    company_id: i32,
    year: i32,
    dry_run: bool,
) -> Result<ApplyCompanyResult, sqlx::Error> {
    let mut tx = state.db.begin().await?;
    mz_core::db::advisory_xact_lock(&mut tx, &format!("holiday-import:{company_id}")).await?;

    let source: Vec<(NaiveDate, String, String)> = sqlx::query_as(
        r#"SELECT date, name, type::text FROM "nationalHoliday"
            WHERE year = $1 ORDER BY date ASC, name ASC"#,
    )
    .bind(year)
    .fetch_all(&mut *tx)
    .await?;
    let (from, to) = (
        NaiveDate::from_ymd_opt(year, 1, 1),
        NaiveDate::from_ymd_opt(year, 12, 31),
    );
    let existing: Vec<(NaiveDate, String)> = match (from, to) {
        (Some(from), Some(to)) => {
            sqlx::query_as(
                r#"SELECT date, name FROM "companyHoliday"
                    WHERE "companyId" = $1 AND date >= $2 AND date <= $3"#,
            )
            .bind(company_id)
            .bind(from)
            .bind(to)
            .fetch_all(&mut *tx)
            .await?
        }
        _ => Vec::new(),
    };
    let seen: HashSet<String> = existing
        .iter()
        .map(|(d, n)| format!("{}|{}", key(*d), n.to_lowercase()))
        .collect();
    let to_add: Vec<&(NaiveDate, String, String)> = source
        .iter()
        .filter(|(d, n, _)| !seen.contains(&format!("{}|{}", key(*d), n.to_lowercase())))
        .collect();
    let skipped = source.len() - to_add.len();

    if dry_run || to_add.is_empty() {
        tx.commit().await?;
        return Ok(ApplyCompanyResult {
            created: to_add
                .iter()
                .map(|(d, n, t)| Created {
                    id: None,
                    date: key(*d),
                    name: n.clone(),
                    ty: t.clone(),
                })
                .collect(),
            skipped,
        });
    }

    let names: Vec<&str> = to_add.iter().map(|(_, n, _)| n.as_str()).collect();
    let dates: Vec<NaiveDate> = to_add.iter().map(|(d, _, _)| *d).collect();
    let types: Vec<&str> = to_add.iter().map(|(_, _, t)| t.as_str()).collect();
    let deducts: Vec<bool> = to_add.iter().map(|(_, _, t)| t == "JOINT_LEAVE").collect();
    let rows: Vec<(i32, NaiveDate, String, String)> = sqlx::query_as(
        r#"INSERT INTO "companyHoliday" ("companyId", name, date, type, "deductsLeave")
           SELECT $1, u.name, u.date, u.type::"HolidayType", u.deducts
             FROM UNNEST($2::text[], $3::date[], $4::text[], $5::bool[])
               AS u(name, date, type, deducts)
           RETURNING id, date, name, type::text"#,
    )
    .bind(company_id)
    .bind(&names)
    .bind(&dates)
    .bind(&types)
    .bind(&deducts)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;

    // Setelah commit; gagal antre tidak menggagalkan apply (tick per jam menutup celahnya).
    let now = mz_core::time::now_naive().and_utc().timestamp_millis();
    for (id, date, _, _) in &rows {
        enqueue_holiday_sync(
            state,
            &HolidaySyncPayload {
                company_id,
                holiday_id: *id,
                previous_date: None,
                current_date: Some(key(*date)),
            },
            now,
        )
        .await;
    }
    Ok(ApplyCompanyResult {
        created: rows
            .into_iter()
            .map(|(id, d, n, t)| Created {
                id: Some(id),
                date: key(d),
                name: n,
                ty: t,
            })
            .collect(),
        skipped,
    })
}

/// Node: src/admin-global/national-holidays.service.ts:67 (handler job `APPLY_HOLIDAYS`):
/// semua company aktif (`status != 0`) berurutan id. `log` = baris log job. Dipasang di
/// worker `admin-jobs` (lihat laporan: `mz-admin-jobs` masih menolak tipe ini).
pub async fn run_apply_holidays_job(
    state: &AppState,
    params: &Map<String, Value>,
    log: &(dyn Fn(&str) + Send + Sync),
) -> anyhow::Result<Map<String, Value>> {
    // `Number(job.params.year)`.
    let year = params
        .get("year")
        .and_then(Value::as_f64)
        .filter(|y| y.fract() == 0.0 && y.abs() < f64::from(i32::MAX))
        .ok_or_else(|| anyhow::anyhow!("APPLY_HOLIDAYS: params.year tidak valid"))?;
    #[allow(clippy::cast_possible_truncation)]
    let year = year as i32;
    let companies: Vec<(i32, String)> =
        sqlx::query_as(r#"SELECT id, uuid::text FROM company WHERE status <> 0 ORDER BY id ASC"#)
            .fetch_all(&state.db)
            .await?;
    let (mut created, mut skipped) = (0usize, 0usize);
    for (id, uuid) in &companies {
        let res = apply_to_company(state, *id, year, false).await?;
        created += res.created.len();
        skipped += res.skipped;
        log(&format!(
            "company {uuid}: created {}, skipped {}",
            res.created.len(),
            res.skipped
        ));
    }
    let mut out = Map::new();
    out.insert("companies".into(), json!(companies.len()));
    out.insert("created".into(), json!(created));
    out.insert("skipped".into(), json!(skipped));
    Ok(out)
}

/// Node: src/admin-global/national-holidays.controller.ts:61 (`apply`) +
/// national-holidays.service.ts:113. Tanpa `@HttpCode`: 202 bila `all: true`, selain itu 200.
pub async fn h_apply(
    State(state): State<AppState>,
    op: Operator,
    ctx: AuditCtx,
    RawBody(body): RawBody,
) -> ApiResult<(StatusCode, Json<Value>)> {
    op.require(WRITE)?;
    let dto = validate_apply(&body)?;

    let has_uuids = dto.company_uuids.as_ref().is_some_and(|u| !u.is_empty());
    let has_all = dto.all == Some(true);
    if has_uuids == has_all {
        return Err(coded(
            StatusCode::BAD_REQUEST,
            "INVALID_TARGET",
            "provide exactly one of companyUuids or all: true",
        ));
    }
    if has_all && dto.dry_run {
        // Jalur semua-company berjalan sebagai admin job sungguhan; menolak lebih baik
        // daripada diam-diam menjalankan sungguhan saat operator meminta pratinjau.
        return Err(coded(
            StatusCode::BAD_REQUEST,
            "DRY_RUN_NOT_SUPPORTED_FOR_ALL",
            "dry run is only available with companyUuids",
        ));
    }

    if has_all {
        let mut params = Map::new();
        params.insert("year".into(), json!(dto.year));
        let uuid = create_job(
            &state,
            &ctx,
            CreateJob {
                ty: JobType::ApplyHolidays,
                company_id: None,
                params,
                dry_run: false,
                operator_id: op.id,
                reason: Some(dto.reason.clone()),
            },
        )
        .await?;
        return Ok((
            StatusCode::ACCEPTED,
            Json(json!({ "data": { "jobUuid": uuid.to_string() } })),
        ));
    }

    // `[...new Set(dto.companyUuids)]`.
    let mut seen = HashSet::new();
    let uuids: Vec<String> = dto
        .company_uuids
        .clone()
        .unwrap_or_default()
        .into_iter()
        .filter(|u| seen.insert(u.clone()))
        .collect();
    let companies: Vec<(i32, String)> = sqlx::query_as(
        r#"SELECT id, uuid::text FROM company WHERE uuid = ANY($1::uuid[]) ORDER BY id ASC"#,
    )
    .bind(&uuids)
    .fetch_all(&state.db)
    .await?;
    if companies.len() != uuids.len() {
        return Err(coded(
            StatusCode::NOT_FOUND,
            "COMPANY_NOT_FOUND",
            "one or more companies were not found",
        ));
    }

    let mut data: Vec<Value> = Vec::with_capacity(companies.len());
    let (mut created_total, mut skipped_total) = (0usize, 0usize);
    for (id, uuid) in &companies {
        let res = apply_to_company(&state, *id, dto.year, dto.dry_run).await?;
        created_total += res.created.len();
        skipped_total += res.skipped;
        data.push(json!({
            "companyUuid": uuid,
            "created": res.created.iter()
                .map(|c| json!({ "date": c.date, "name": c.name, "type": c.ty }))
                .collect::<Vec<_>>(),
            "skipped": res.skipped,
        }));
    }

    let after = json!({
        "year": dto.year,
        "dryRun": dto.dry_run,
        "companies": data.len(),
        "created": created_total,
        "skipped": skipped_total,
    });
    let mut tx = state.db.begin().await?;
    record(
        &mut *tx,
        &ctx,
        &OperatorAuditEntry {
            operator_id: op.id,
            action: "national_holiday.apply",
            target_type: "nationalHoliday",
            target_uuid: None,
            company_id: None,
            before: None,
            after: Some(&after),
            reason: Some(&dto.reason),
        },
    )
    .await?;
    tx.commit().await?;
    Ok((StatusCode::OK, Json(json!({ "data": data }))))
}

/// `PATCH /admin/national-holidays/apply`: di Nest jatuh ke `PATCH :uuid` (OWNER/SUPPORT,
/// galat body lebih dulu, lalu `ParseUUIDPipe` → 400).
pub async fn h_apply_as_uuid_patch(op: Operator, RawBody(body): RawBody) -> ApiResult<Json<Value>> {
    op.require(WRITE)?;
    validate_update(&body)?;
    mz_core::uuid_rules::parse_uuid_pipe("apply")?;
    Err(ApiError::internal(anyhow::anyhow!(
        "apply accepted as uuid"
    )))
}

/// `DELETE /admin/national-holidays/apply`: jatuh ke `DELETE :uuid`.
pub async fn h_apply_as_uuid_delete(
    op: Operator,
    RawBody(body): RawBody,
) -> ApiResult<Json<Value>> {
    op.require(WRITE)?;
    validate_delete(&body)?;
    mz_core::uuid_rules::parse_uuid_pipe("apply")?;
    Err(ApiError::internal(anyhow::anyhow!(
        "apply accepted as uuid"
    )))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn errs(body: &Value) -> Vec<String> {
        match validate_apply(body) {
            Err(ApiError::Validation(v)) => v,
            Err(o) => vec![format!("OTHER {o}")],
            Ok(_) => Vec::new(),
        }
    }

    #[test]
    fn dto_valid() {
        let d = validate_apply(&json!({
            "year": "2027", "all": true, "dryRun": false, "reason": "  Salin libur 2027  "
        }))
        .unwrap();
        assert_eq!(d.year, 2027);
        assert_eq!(d.all, Some(true));
        assert_eq!(d.reason, "Salin libur 2027");
        let d = validate_apply(&json!({
            "year": 2027, "companyUuids": ["3f2b6d3e-7c6e-4a8b-9d1c-0a1b2c3d4e5f"],
            "dryRun": true, "reason": "Pratinjau"
        }))
        .unwrap();
        assert_eq!(d.company_uuids.unwrap().len(), 1);
        assert!(d.dry_run);
    }

    #[test]
    fn dto_pesan_dan_urutan() {
        assert_eq!(
            errs(&json!({})),
            [
                "year must not be greater than 2100",
                "year must not be less than 2000",
                "year must be an integer number",
                "dryRun must be a boolean value",
                "reason must be longer than or equal to 5 characters",
                "reason must be a string",
            ]
        );
        assert_eq!(
            errs(&json!({
                "x": 1, "year": 2027, "dryRun": false, "reason": "abcde",
                "companyUuids": [], "all": "ya"
            })),
            [
                "property x should not exist",
                "companyUuids must contain at least 1 elements",
                "all must be a boolean value",
            ]
        );
        assert_eq!(
            errs(&json!({
                "year": 2027, "dryRun": false, "reason": "abcde", "companyUuids": ["x"]
            })),
            ["each value in companyUuids must be a UUID"]
        );
        assert_eq!(
            errs(&json!({
                "year": 2027, "dryRun": false, "reason": "abcde",
                "companyUuids": "3f2b6d3e-7c6e-4a8b-9d1c-0a1b2c3d4e5f"
            })),
            [
                "companyUuids must contain no more than 200 elements",
                "companyUuids must contain at least 1 elements",
                "companyUuids must be an array",
            ]
        );
    }
}
