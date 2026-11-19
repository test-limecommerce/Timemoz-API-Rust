//! `UsersService`: pay rates (:2466-2587), project rates (:2589-2685), work time limit (:2687-2807).

use mz_account::target_user::{CompanyMemberMiss, find_company_member, require_company_member};
use mz_core::time::{JsDate, parse_js_date};
use mz_core::{ApiError, ApiResult, AppState};
use serde_json::{Value, json};
use sqlx::{Postgres, QueryBuilder};

use crate::model::{PayRateInput, js_num, opt_num};

fn internal(msg: &str) -> ApiError {
    ApiError::internal(anyhow::anyhow!("{msg}"))
}

fn failure(message: &str) -> Value {
    json!({ "success": false, "message": message, "data": null })
}

// ── pay rates ────────────────────────────────────────────────────────────────

/// Kolom `userPayRate` (`@db.Date` dibaca sebagai `timestamp` supaya serialisasinya `...T00:00:00.000Z`).
const PAY_RATE_COLUMNS: &str = r#"id, uuid::text AS "uuid", "userId", "companyId",
    "payPeriodType"::text AS "payPeriodType", "timesheetApproval", "payRate",
    "rateType"::text AS "rateType", "effectiveDate"::timestamp AS "effectiveDate", notes,
    "createdAt", "updatedAt", status"#;

#[derive(sqlx::FromRow)]
struct PayRateRow {
    id: i32,
    uuid: String,
    #[sqlx(rename = "userId")]
    user_id: i32,
    #[sqlx(rename = "companyId")]
    company_id: i32,
    #[sqlx(rename = "payPeriodType")]
    pay_period_type: String,
    #[sqlx(rename = "timesheetApproval")]
    timesheet_approval: bool,
    #[sqlx(rename = "payRate")]
    pay_rate: Option<f64>,
    #[sqlx(rename = "rateType")]
    rate_type: String,
    #[sqlx(rename = "effectiveDate")]
    effective_date: JsDate,
    notes: Option<String>,
    #[sqlx(rename = "createdAt")]
    created_at: JsDate,
    #[sqlx(rename = "updatedAt")]
    updated_at: Option<JsDate>,
    status: i32,
}

/// Baris `userPayRate` lengkap seperti objek Prisma.
fn pay_rate_json(r: &PayRateRow) -> Value {
    json!({
        "id": r.id,
        "uuid": r.uuid,
        "userId": r.user_id,
        "companyId": r.company_id,
        "payPeriodType": r.pay_period_type,
        "timesheetApproval": r.timesheet_approval,
        "payRate": opt_num(r.pay_rate),
        "rateType": r.rate_type,
        "effectiveDate": r.effective_date,
        "notes": r.notes,
        "createdAt": r.created_at,
        "updatedAt": r.updated_at,
        "status": r.status,
    })
}

#[derive(sqlx::FromRow)]
struct PayRateListRow {
    uuid: String,
    #[sqlx(rename = "payPeriodType")]
    pay_period_type: String,
    #[sqlx(rename = "timesheetApproval")]
    timesheet_approval: bool,
    #[sqlx(rename = "payRate")]
    pay_rate: Option<f64>,
    #[sqlx(rename = "rateType")]
    rate_type: String,
    #[sqlx(rename = "effectiveDate")]
    effective_date: JsDate,
    notes: Option<String>,
    #[sqlx(rename = "createdAt")]
    created_at: JsDate,
    currency: String,
}

/// `getPayRates(userUuid, requesterId)`.
pub async fn get_pay_rates(
    state: &AppState,
    user_uuid: &str,
    requester_id: i32,
) -> ApiResult<Value> {
    let member = require_company_member(&state.db, requester_id, user_uuid).await?;
    let rows = sqlx::query_as::<_, PayRateListRow>(
        r#"SELECT r.uuid::text AS "uuid", r."payPeriodType"::text AS "payPeriodType",
                  r."timesheetApproval", r."payRate", r."rateType"::text AS "rateType",
                  r."effectiveDate"::timestamp AS "effectiveDate", r.notes, r."createdAt",
                  COALESCE(c.currency, 'USD') AS currency
           FROM "userPayRate" r LEFT JOIN company c ON c.id = r."companyId"
           WHERE r."userId" = $1 AND r."companyId" = $2 AND r.status = 1
           ORDER BY r."effectiveDate" DESC, r.id DESC"#,
    )
    .bind(member.user.id)
    .bind(member.company_id)
    .fetch_all(&state.db)
    .await?;
    let data: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "uuid": r.uuid,
                "payPeriodType": r.pay_period_type,
                "timesheetApproval": r.timesheet_approval,
                "payRate": opt_num(r.pay_rate),
                "rateType": r.rate_type,
                "effectiveDate": r.effective_date,
                "notes": r.notes,
                "createdAt": r.created_at,
                "currency": r.currency,
            })
        })
        .collect();
    Ok(json!({ "success": true, "message": "success get pay rates", "data": data }))
}

/// `new Date(dto.effectiveDate)` untuk kolom `@db.Date`: tanggal UTC-nya. `Invalid Date` -> 500 (Prisma).
fn effective_date(raw: &str) -> ApiResult<chrono::NaiveDate> {
    parse_js_date(raw)
        .map(|d| d.date())
        .ok_or_else(|| internal("Invalid Date"))
}

/// `createPayRate(userUuid, dto, requesterId)` -> 201.
pub async fn create_pay_rate(
    state: &AppState,
    user_uuid: &str,
    dto: &PayRateInput,
    requester_id: i32,
) -> ApiResult<Value> {
    // Sebelumnya baris gaji tercipta untuk user company lain lalu distempel company pelaku.
    let member = require_company_member(&state.db, requester_id, user_uuid).await?;
    let date = effective_date(dto.effective_date.as_deref().unwrap_or_default())?;
    let row = sqlx::query_as::<_, PayRateRow>(&format!(
        r#"INSERT INTO "userPayRate"
             ("userId", "companyId", "payPeriodType", "timesheetApproval", "payRate", "rateType",
              "effectiveDate", notes, status)
           VALUES ($1, $2, $3::"PayPeriodType", $4, $5, $6::"RateType", $7, $8, 1)
           RETURNING {PAY_RATE_COLUMNS}"#
    ))
    .bind(member.user.id)
    .bind(member.company_id)
    .bind(dto.pay_period_type.as_deref().unwrap_or_default())
    .bind(dto.timesheet_approval.flatten().unwrap_or(false))
    .bind(dto.pay_rate.flatten())
    .bind(dto.rate_type.as_deref().unwrap_or_default())
    .bind(date)
    .bind(dto.notes.clone().flatten())
    .fetch_one(&state.db)
    .await?;
    Ok(json!({ "success": true, "message": "pay rate created", "data": pay_rate_json(&row) }))
}

async fn find_active_rate(
    state: &AppState,
    rate_uuid: &str,
    user_id: i32,
    company_id: i32,
) -> ApiResult<Option<i32>> {
    Ok(sqlx::query_scalar(
        r#"SELECT id FROM "userPayRate"
           WHERE uuid = $1::uuid AND "userId" = $2 AND "companyId" = $3 AND status = 1
           ORDER BY id LIMIT 1"#,
    )
    .bind(rate_uuid)
    .bind(user_id)
    .bind(company_id)
    .fetch_optional(&state.db)
    .await?)
}

/// Bagian `SET` dinamis PATCH pay rate. Murni terhadap `QueryBuilder` supaya bisa diuji.
fn push_pay_rate_update(
    qb: &mut QueryBuilder<'_, Postgres>,
    dto: &PayRateInput,
    now: JsDate,
) -> ApiResult<()> {
    qb.push(r#"UPDATE "userPayRate" SET "updatedAt" = "#)
        .push_bind(now);
    // `dto.payPeriodType && {...}`: string kosong/null tidak mengubah apa pun.
    if let Some(v) = dto.pay_period_type.as_deref().filter(|v| !v.is_empty()) {
        qb.push(r#", "payPeriodType" = "#)
            .push_bind(v.to_owned())
            .push(r#"::"PayPeriodType""#);
    }
    if let Some(v) = dto.timesheet_approval {
        // `null` pada kolom Boolean non-null ditolak Prisma -> 500.
        let v = v.ok_or_else(|| internal("timesheetApproval cannot be null"))?;
        qb.push(r#", "timesheetApproval" = "#).push_bind(v);
    }
    if let Some(v) = dto.pay_rate {
        qb.push(r#", "payRate" = "#).push_bind(v);
    }
    if let Some(v) = dto.rate_type.as_deref().filter(|v| !v.is_empty()) {
        qb.push(r#", "rateType" = "#)
            .push_bind(v.to_owned())
            .push(r#"::"RateType""#);
    }
    if let Some(v) = dto.effective_date.as_deref().filter(|v| !v.is_empty()) {
        qb.push(r#", "effectiveDate" = "#)
            .push_bind(effective_date(v)?);
    }
    if let Some(v) = &dto.notes {
        qb.push(", notes = ").push_bind(v.clone());
    }
    Ok(())
}

/// `updatePayRate(userUuid, rateUuid, dto, requesterId)`.
pub async fn update_pay_rate(
    state: &AppState,
    user_uuid: &str,
    rate_uuid: &str,
    dto: &PayRateInput,
    requester_id: i32,
) -> ApiResult<Value> {
    let member = require_company_member(&state.db, requester_id, user_uuid).await?;
    let Some(rate_id) =
        find_active_rate(state, rate_uuid, member.user.id, member.company_id).await?
    else {
        return Ok(failure("Pay rate not found"));
    };
    let mut qb = QueryBuilder::<Postgres>::new("");
    push_pay_rate_update(&mut qb, dto, JsDate::now())?;
    qb.push(" WHERE id = ")
        .push_bind(rate_id)
        .push(format!(" RETURNING {PAY_RATE_COLUMNS}"));
    let row: PayRateRow = qb.build_query_as().fetch_one(&state.db).await?;
    Ok(json!({ "success": true, "message": "pay rate updated", "data": pay_rate_json(&row) }))
}

/// `deletePayRate(userUuid, rateUuid, requesterId)` — soft delete (`status = 0`).
pub async fn delete_pay_rate(
    state: &AppState,
    user_uuid: &str,
    rate_uuid: &str,
    requester_id: i32,
) -> ApiResult<Value> {
    let member = require_company_member(&state.db, requester_id, user_uuid).await?;
    let Some(rate_id) =
        find_active_rate(state, rate_uuid, member.user.id, member.company_id).await?
    else {
        return Ok(failure("Pay rate not found"));
    };
    sqlx::query(r#"UPDATE "userPayRate" SET status = 0, "updatedAt" = $2 WHERE id = $1"#)
        .bind(rate_id)
        .bind(JsDate::now())
        .execute(&state.db)
        .await?;
    Ok(json!({ "success": true, "message": "pay rate deleted", "data": null }))
}

// ── project rates ────────────────────────────────────────────────────────────

#[derive(sqlx::FromRow)]
struct ProjectRateRow {
    #[sqlx(rename = "projectUuid")]
    project_uuid: String,
    #[sqlx(rename = "projectName")]
    project_name: String,
    #[sqlx(rename = "payRate")]
    pay_rate: Option<f64>,
    #[sqlx(rename = "billRate")]
    bill_rate: Option<f64>,
}

/// `getProjectRates(userUuid, requesterId)`.
pub async fn get_project_rates(
    state: &AppState,
    user_uuid: &str,
    requester_id: i32,
) -> ApiResult<Value> {
    let member = require_company_member(&state.db, requester_id, user_uuid).await?;
    // `projectUser` tidak punya `companyId`: pagarnya lewat `projectCompany` (tanpa ini tarif
    // proyek company lain ikut terbaca).
    let rows = sqlx::query_as::<_, ProjectRateRow>(
        r#"SELECT p.uuid::text AS "projectUuid", p.name AS "projectName", pu."payRate", pu."billRate"
           FROM "projectUser" pu JOIN project p ON p.id = pu."projectId"
           WHERE pu."userId" = $1 AND pu.status = 1
             AND EXISTS (SELECT 1 FROM "projectCompany" pc
                         WHERE pc."projectId" = p.id AND pc."companyId" = $2 AND pc.status = 1)
           ORDER BY pu.id"#,
    )
    .bind(member.user.id)
    .bind(member.company_id)
    .fetch_all(&state.db)
    .await?;
    let data: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "projectUuid": r.project_uuid,
                "projectName": r.project_name,
                "payRate": opt_num(r.pay_rate),
                "billRate": opt_num(r.bill_rate),
            })
        })
        .collect();
    Ok(json!({ "success": true, "message": "success get project rates", "data": data }))
}

/// `setProjectRate(userUuid, projectUuid, dto, requesterId)`.
pub async fn set_project_rate(
    state: &AppState,
    user_uuid: &str,
    project_uuid: &str,
    pay_rate: Option<f64>,
    bill_rate: Option<f64>,
    requester_id: i32,
) -> ApiResult<Value> {
    let member = require_company_member(&state.db, requester_id, user_uuid).await?;
    // Proyeknya ikut dipagari: `findUnique({ uuid })` menerima proyek company mana pun.
    let project_id: Option<i32> = sqlx::query_scalar(
        r#"SELECT p.id FROM project p
           WHERE p.uuid = $1::uuid
             AND EXISTS (SELECT 1 FROM "projectCompany" pc
                         WHERE pc."projectId" = p.id AND pc."companyId" = $2 AND pc.status = 1)
           ORDER BY p.id LIMIT 1"#,
    )
    .bind(project_uuid)
    .bind(member.company_id)
    .fetch_optional(&state.db)
    .await?;
    let Some(project_id) = project_id else {
        return Ok(failure("Project not found"));
    };
    let project_user_id: Option<i32> = sqlx::query_scalar(
        r#"SELECT id FROM "projectUser" WHERE "userId" = $1 AND "projectId" = $2 AND status = 1
           ORDER BY id LIMIT 1"#,
    )
    .bind(member.user.id)
    .bind(project_id)
    .fetch_optional(&state.db)
    .await?;
    let Some(project_user_id) = project_user_id else {
        return Ok(failure("User is not a member of this project"));
    };
    let (pay, bill): (Option<f64>, Option<f64>) = sqlx::query_as(
        r#"UPDATE "projectUser" SET "payRate" = $2, "billRate" = $3, "updatedAt" = $4
           WHERE id = $1 RETURNING "payRate", "billRate""#,
    )
    .bind(project_user_id)
    .bind(pay_rate)
    .bind(bill_rate)
    .bind(JsDate::now())
    .fetch_one(&state.db)
    .await?;
    Ok(json!({
        "success": true,
        "message": "project rate updated",
        "data": { "projectUuid": project_uuid, "payRate": opt_num(pay), "billRate": opt_num(bill) },
    }))
}

// ── work time limit ──────────────────────────────────────────────────────────

fn limit_json(uuid: &str, daily: Option<i32>, weekly: Option<i32>) -> Value {
    json!({
        "uuid": uuid,
        "dailyLimitSec": daily.map(|v| js_num(f64::from(v))),
        "weeklyLimitSec": weekly.map(|v| js_num(f64::from(v))),
    })
}

/// Pesan kegagalan `findCompanyMember` (bentuk yang TIDAK melempar).
fn miss_message(miss: CompanyMemberMiss) -> &'static str {
    match miss {
        CompanyMemberMiss::NoCompany => "Company not found",
        CompanyMemberMiss::NotMember => "User not found",
    }
}

/// `getWorkTimeLimit(userUuid, requesterId)`.
pub async fn get_work_time_limit(
    state: &AppState,
    user_uuid: &str,
    requester_id: i32,
) -> ApiResult<Value> {
    let member = match find_company_member(&state.db, requester_id, user_uuid).await? {
        Ok(member) => member,
        Err(miss) => return Ok(failure(miss_message(miss))),
    };
    let row: Option<(String, Option<i32>, Option<i32>)> = sqlx::query_as(
        r#"SELECT uuid::text, "dailyLimitSec", "weeklyLimitSec" FROM "userWorkTimeLimit"
           WHERE "userId" = $1 AND "companyId" = $2 AND status = 1 ORDER BY id LIMIT 1"#,
    )
    .bind(member.user.id)
    .bind(member.company_id)
    .fetch_optional(&state.db)
    .await?;
    Ok(json!({
        "success": true,
        "message": "success get work time limit",
        "data": row.map(|(uuid, daily, weekly)| limit_json(&uuid, daily, weekly)),
    }))
}

/// `weeklyLimitSec` tidak boleh lebih kecil dari `dailyLimitSec` (400, SEBELUM cek company).
pub fn check_limit_order(daily: Option<i32>, weekly: Option<i32>) -> ApiResult<()> {
    if let (Some(daily), Some(weekly)) = (daily, weekly)
        && weekly < daily
    {
        return Err(ApiError::bad_request(
            "weeklyLimitSec must not be less than dailyLimitSec",
        ));
    }
    Ok(())
}

/// `upsertWorkTimeLimit(userUuid, dto, requesterId)`.
pub async fn upsert_work_time_limit(
    state: &AppState,
    user_uuid: &str,
    daily: Option<i32>,
    weekly: Option<i32>,
    requester_id: i32,
) -> ApiResult<Value> {
    check_limit_order(daily, weekly)?;
    // Jalur `create`-nya mengarang baris untuk user company lain, jadi memakai bentuk yang melempar.
    let member = require_company_member(&state.db, requester_id, user_uuid).await?;
    let existing: Option<i32> = sqlx::query_scalar(
        r#"SELECT id FROM "userWorkTimeLimit" WHERE "userId" = $1 AND "companyId" = $2
           ORDER BY id LIMIT 1"#,
    )
    .bind(member.user.id)
    .bind(member.company_id)
    .fetch_optional(&state.db)
    .await?;

    let (uuid, daily, weekly): (String, Option<i32>, Option<i32>) = if let Some(id) = existing {
        sqlx::query_as(
            r#"UPDATE "userWorkTimeLimit"
               SET "dailyLimitSec" = $2, "weeklyLimitSec" = $3, status = 1, "updatedAt" = $4
               WHERE id = $1 RETURNING uuid::text, "dailyLimitSec", "weeklyLimitSec""#,
        )
        .bind(id)
        .bind(daily)
        .bind(weekly)
        .bind(JsDate::now())
        .fetch_one(&state.db)
        .await?
    } else {
        sqlx::query_as(
            r#"INSERT INTO "userWorkTimeLimit" ("userId", "companyId", "dailyLimitSec", "weeklyLimitSec", status)
               VALUES ($1, $2, $3, $4, 1) RETURNING uuid::text, "dailyLimitSec", "weeklyLimitSec""#,
        )
        .bind(member.user.id)
        .bind(member.company_id)
        .bind(daily)
        .bind(weekly)
        .fetch_one(&state.db)
        .await?
    };
    Ok(json!({
        "success": true,
        "message": "work time limit saved",
        "data": limit_json(&uuid, daily, weekly),
    }))
}

/// `removeWorkTimeLimit(userUuid, requesterId)` — soft delete.
pub async fn remove_work_time_limit(
    state: &AppState,
    user_uuid: &str,
    requester_id: i32,
) -> ApiResult<Value> {
    let member = match find_company_member(&state.db, requester_id, user_uuid).await? {
        Ok(member) => member,
        Err(miss) => return Ok(failure(miss_message(miss))),
    };
    let existing: Option<i32> = sqlx::query_scalar(
        r#"SELECT id FROM "userWorkTimeLimit" WHERE "userId" = $1 AND "companyId" = $2 AND status = 1
           ORDER BY id LIMIT 1"#,
    )
    .bind(member.user.id)
    .bind(member.company_id)
    .fetch_optional(&state.db)
    .await?;
    let Some(id) = existing else {
        return Ok(failure("Work time limit not found"));
    };
    sqlx::query(r#"UPDATE "userWorkTimeLimit" SET status = 0, "updatedAt" = $2 WHERE id = $1"#)
        .bind(id)
        .bind(JsDate::now())
        .execute(&state.db)
        .await?;
    Ok(json!({ "success": true, "message": "work time limit removed", "data": null }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weekly_must_not_be_below_daily() {
        assert!(check_limit_order(Some(8 * 3600), Some(40 * 3600)).is_ok());
        assert!(check_limit_order(Some(100), Some(100)).is_ok());
        assert!(check_limit_order(None, Some(1)).is_ok());
        assert!(check_limit_order(Some(1), None).is_ok());
        let err = check_limit_order(Some(200), Some(100));
        assert!(matches!(&err, Err(ApiError::Message { message, .. })
            if message == "weeklyLimitSec must not be less than dailyLimitSec"));
    }

    #[test]
    fn update_sql_only_sets_present_fields() {
        let dto = PayRateInput {
            pay_rate: Some(None),
            notes: Some(Some("naik".into())),
            ..Default::default()
        };
        let mut qb = QueryBuilder::<Postgres>::new("");
        assert!(push_pay_rate_update(&mut qb, &dto, JsDate::now()).is_ok());
        assert_eq!(
            qb.sql(),
            r#"UPDATE "userPayRate" SET "updatedAt" = $1, "payRate" = $2, notes = $3"#
        );
    }

    #[test]
    fn update_sql_casts_enums_and_skips_empty_strings() {
        let dto = PayRateInput {
            pay_period_type: Some("WEEKLY".into()),
            rate_type: Some(String::new()),
            timesheet_approval: Some(Some(true)),
            effective_date: Some("2026-05-01".into()),
            ..Default::default()
        };
        let mut qb = QueryBuilder::<Postgres>::new("");
        assert!(push_pay_rate_update(&mut qb, &dto, JsDate::now()).is_ok());
        let sql = qb.sql();
        assert!(sql.contains(r#""payPeriodType" = $2::"PayPeriodType""#));
        assert!(!sql.contains("rateType"));
        assert!(sql.contains(r#""effectiveDate" = $4"#));
    }

    #[test]
    fn null_timesheet_approval_is_an_error() {
        let dto = PayRateInput {
            timesheet_approval: Some(None),
            ..Default::default()
        };
        let mut qb = QueryBuilder::<Postgres>::new("");
        assert!(push_pay_rate_update(&mut qb, &dto, JsDate::now()).is_err());
    }

    #[test]
    fn effective_date_takes_utc_date_part() {
        assert_eq!(
            effective_date("2026-05-01T23:30:00-05:00")
                .ok()
                .map(|d| d.to_string())
                .as_deref(),
            Some("2026-05-02")
        );
        assert!(effective_date("garbage").is_err());
    }

    #[test]
    fn limit_json_keeps_nulls() {
        assert_eq!(
            limit_json("u", None, Some(3600)),
            json!({ "uuid": "u", "dailyLimitSec": null, "weeklyLimitSec": 3600 })
        );
    }
}
