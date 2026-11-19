//! Port `src/timesheets/timesheets.{controller,service}.ts` (16 route).

use std::collections::HashMap;

use axum::Json;
use axum::body::Body;
use axum::extract::{Query, State};
use axum::response::Response;
use chrono::{Duration, NaiveDate, NaiveDateTime};
use http::{HeaderValue, StatusCode, header};
use mz_account::assignable_role::{assert_target_within_rank, require_actor_role};
use mz_core::company::find_company_id;
use mz_core::extract::{PathParams, ValidatedJson, ValidatedQuery};
use mz_core::time::now_naive;
use mz_core::{ApiError, ApiResult, AppState, RequestCtx};
use serde_json::{Map, Value, json};
use sqlx::{Postgres, QueryBuilder};

use crate::breakdown::{CsvInput, TimelineRow, build_breakdown, build_csv, daily_json};
use crate::calc::{hours2, period_bounds, total_amount};
use crate::common::{
    TIMELINE_FILTER, approvers, can_act_on_company, date_str, fail, fnum, is_system_admin, jd,
    jd_opt, parse_uuid, plain_fail, total_seconds,
};
use crate::dto::{
    AdminCreateBody, AdminQuery, EditBody, MyQuery, RejectBody, ReopenBody, SubmitBody, parse_ymd,
    provided,
};
use crate::jsfmt;
use crate::mail::{ApprovalRequest, StatusMail, send_approval_request, send_status};
use crate::pdf_reports::{DayEntry, DayProject, TimesheetPdfData, timesheet_pdf};

// ───────────────────────── baris timesheet ─────────────────────────

/// Kolom `timesheet` + relasi `user` / `approvedBy` (alias `u` / `ab`).
const TS_SELECT: &str = r#"SELECT ts.id, ts.uuid::text AS uuid, ts."userId", ts."companyId",
    ts."periodStart", ts."periodEnd", ts."payPeriodType"::text AS "payPeriodType",
    ts.status::text AS status, ts."totalSeconds", ts."payRate",
    ts."rateType"::text AS "rateType", ts."totalAmount", ts.currency, ts."submittedAt",
    ts."approvedById", ts."approvedAt", ts."rejectedReason", ts.notes,
    u.uuid::text AS "uUuid", u.name AS "uName", u.username AS "uUsername",
    ab.uuid::text AS "abUuid", ab.name AS "abName"
    FROM timesheet ts
    LEFT JOIN users u ON u.id = ts."userId"
    LEFT JOIN users ab ON ab.id = ts."approvedById""#;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Ts {
    pub id: i32,
    pub uuid: String,
    #[sqlx(rename = "userId")]
    pub user_id: i32,
    #[sqlx(rename = "companyId")]
    pub company_id: i32,
    #[sqlx(rename = "periodStart")]
    pub period_start: NaiveDate,
    #[sqlx(rename = "periodEnd")]
    pub period_end: NaiveDate,
    #[sqlx(rename = "payPeriodType")]
    pub pay_period_type: String,
    pub status: String,
    #[sqlx(rename = "totalSeconds")]
    pub total_seconds: i32,
    #[sqlx(rename = "payRate")]
    pub pay_rate: Option<f64>,
    #[sqlx(rename = "rateType")]
    pub rate_type: String,
    #[sqlx(rename = "totalAmount")]
    pub total_amount: Option<f64>,
    pub currency: String,
    #[sqlx(rename = "submittedAt")]
    pub submitted_at: NaiveDateTime,
    #[sqlx(rename = "approvedById")]
    pub approved_by_id: Option<i32>,
    #[sqlx(rename = "approvedAt")]
    pub approved_at: Option<NaiveDateTime>,
    #[sqlx(rename = "rejectedReason")]
    pub rejected_reason: Option<String>,
    pub notes: Option<String>,
    #[sqlx(rename = "uUuid")]
    pub u_uuid: Option<String>,
    #[sqlx(rename = "uName")]
    pub u_name: Option<String>,
    #[sqlx(rename = "uUsername")]
    pub u_username: Option<String>,
    #[sqlx(rename = "abUuid")]
    pub ab_uuid: Option<String>,
    #[sqlx(rename = "abName")]
    pub ab_name: Option<String>,
}

impl Ts {
    fn secs(&self) -> i64 {
        i64::from(self.total_seconds)
    }
    fn total_hours(&self) -> Value {
        jsfmt::num(hours2(self.secs()))
    }
    fn approved_by(&self) -> Value {
        match (&self.ab_uuid, &self.ab_name) {
            (Some(u), Some(n)) => json!({ "uuid": u, "name": n }),
            _ => Value::Null,
        }
    }
    fn user(&self) -> Value {
        match (&self.u_uuid, &self.u_name, &self.u_username) {
            (Some(u), Some(n), Some(e)) => json!({ "uuid": u, "name": n, "username": e }),
            _ => Value::Null,
        }
    }
    /// Objek `data` yang dipakai `submit` / `adminCreate` / `getMyTimesheets` (tanpa kolom khusus).
    fn core_json(&self) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("uuid".into(), json!(self.uuid));
        m.insert("periodStart".into(), json!(date_str(self.period_start)));
        m.insert("periodEnd".into(), json!(date_str(self.period_end)));
        m.insert("payPeriodType".into(), json!(self.pay_period_type));
        m.insert("status".into(), json!(self.status));
        m.insert("totalSeconds".into(), json!(self.total_seconds));
        m.insert("totalHours".into(), self.total_hours());
        m.insert("payRate".into(), fnum(self.pay_rate));
        m.insert("rateType".into(), json!(self.rate_type));
        m.insert("totalAmount".into(), fnum(self.total_amount));
        m.insert("currency".into(), json!(self.currency));
        m.insert("submittedAt".into(), jd(self.submitted_at));
        m
    }
}

async fn fetch_by_id(state: &AppState, id: i32) -> ApiResult<Ts> {
    Ok(
        sqlx::query_as::<_, Ts>(&format!("{TS_SELECT} WHERE ts.id = $1"))
            .bind(id)
            .fetch_one(&state.db)
            .await?,
    )
}

/// `findFirst({ where: { uuid[, companyId] } })` dengan relasi.
pub(crate) async fn fetch_by_uuid(
    state: &AppState,
    uuid: &str,
    company_id: Option<i32>,
) -> ApiResult<Option<Ts>> {
    let mut qb = QueryBuilder::<Postgres>::new(TS_SELECT);
    qb.push(" WHERE ts.uuid = ")
        .push_bind(uuid.to_owned())
        .push("::uuid");
    if let Some(c) = company_id {
        qb.push(r#" AND ts."companyId" = "#).push_bind(c);
    }
    qb.push(" LIMIT 1");
    Ok(qb.build_query_as::<Ts>().fetch_optional(&state.db).await?)
}

#[derive(sqlx::FromRow)]
struct ActiveRate {
    #[sqlx(rename = "payPeriodType")]
    pay_period_type: String,
    #[sqlx(rename = "timesheetApproval")]
    timesheet_approval: bool,
    #[sqlx(rename = "payRate")]
    pay_rate: Option<f64>,
    #[sqlx(rename = "rateType")]
    rate_type: String,
}

/// `getActivePayRate`: `effectiveDate` paling baru yang sudah lewat, `status = 1`.
async fn active_pay_rate(
    state: &AppState,
    user_id: i32,
    company_id: i32,
) -> sqlx::Result<Option<ActiveRate>> {
    sqlx::query_as(
        r#"SELECT "payPeriodType"::text AS "payPeriodType", "timesheetApproval", "payRate",
                  "rateType"::text AS "rateType"
           FROM "userPayRate"
           WHERE "userId" = $1 AND "companyId" = $2 AND "effectiveDate" <= $3 AND status = 1
           ORDER BY "effectiveDate" DESC, id DESC LIMIT 1"#,
    )
    .bind(user_id)
    .bind(company_id)
    .bind(now_naive())
    .fetch_optional(&state.db)
    .await
}

async fn timelines(
    state: &AppState,
    user_id: i32,
    start: NaiveDate,
    end: NaiveDate,
) -> sqlx::Result<Vec<TimelineRow>> {
    sqlx::query_as(&format!(
        r#"SELECT tl.uuid::text AS uuid, tl."startAt", tl."endAt", tl."startAtUtc", tl."endAtUtc",
                  tl."offsetMinutes", tl.billable, tl.note, tl."isOffline",
                  tl."manualTimeStatus"::text AS "manualTimeStatus",
                  tl."manualTimeApprovalSource"::text AS "manualTimeApprovalSource",
                  p.uuid::text AS "projectUuid", p.name AS "projectName",
                  p.billable AS "projectBillable", t."takenDate"
           FROM "trackingTimeline" tl
           JOIN tracking t ON t.id = tl."trackingId"
           LEFT JOIN project p ON p.id = tl."projectId"
           WHERE {TIMELINE_FILTER}
           ORDER BY tl."startAt" ASC, tl.id ASC"#
    ))
    .bind(user_id)
    .bind(start)
    .bind(end)
    .fetch_all(&state.db)
    .await
}

fn host_be(state: &AppState) -> String {
    state
        .config
        .var("HOST_BE")
        .unwrap_or_else(|| "http://localhost:3000".to_owned())
}

fn host_fe(state: &AppState) -> String {
    state
        .config
        .var("HOST_FE")
        .unwrap_or_else(|| "http://localhost:3001".to_owned())
}

fn created(v: Value) -> (StatusCode, Json<Value>) {
    (StatusCode::CREATED, Json(v))
}

/// `new Date(str + 'T00:00:00')`: hanya bentuk `YYYY-MM-DD` yang valid di sini.
fn ymd_or_500(s: &str) -> ApiResult<NaiveDate> {
    parse_ymd(s).ok_or_else(|| ApiError::internal(anyhow::anyhow!("Invalid time value: {s}")))
}

// ───────────────────────── employee ─────────────────────────

/// Node: src/timesheets/timesheets.controller.ts:64 (`submit`, `@HttpCode(201)`)
pub async fn submit(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedJson(body): ValidatedJson<SubmitBody>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let user_id = ctx.require_user_id()?;
    Ok(created(submit_inner(&state, user_id, &body).await?))
}

/// Node: src/timesheets/timesheets.service.ts:178 (`submit`)
async fn submit_inner(state: &AppState, user_id: i32, body: &SubmitBody) -> ApiResult<Value> {
    let Some(company_id) = find_company_id(&state.db, user_id).await? else {
        return Ok(fail("Company not found"));
    };
    let company: Option<(Option<bool>, String)> =
        sqlx::query_as(r#"SELECT "useTimesheetApproval", currency FROM company WHERE id = $1"#)
            .bind(company_id)
            .fetch_optional(&state.db)
            .await?;
    let Some((use_approval, currency)) = company else {
        return Ok(fail("Company not found"));
    };
    if !use_approval.unwrap_or(false) {
        return Ok(fail("Timesheet approval is not enabled for this company"));
    }
    let Some(rate) = active_pay_rate(state, user_id, company_id).await? else {
        return Ok(fail("Pay rate not configured"));
    };
    if !rate.timesheet_approval {
        return Ok(fail("Timesheet approval is not enabled for your account"));
    }

    let (period_start, period_end) = period_bounds(&rate.pay_period_type, now_naive().date());

    let existing: Option<i32> = sqlx::query_scalar(
        r#"SELECT id FROM timesheet
           WHERE "userId" = $1 AND "companyId" = $2 AND "periodStart" = $3
             AND status::text = ANY($4) LIMIT 1"#,
    )
    .bind(user_id)
    .bind(company_id)
    .bind(period_start)
    .bind(vec!["PENDING", "APPROVED"])
    .fetch_optional(&state.db)
    .await?;
    if existing.is_some() {
        return Ok(fail(
            "You already have a pending or approved timesheet for this period",
        ));
    }

    let secs = total_seconds(&state.db, user_id, period_start, period_end).await?;
    let amount = total_amount(secs, rate.pay_rate, &rate.rate_type);
    let notes = provided(&body.notes)
        .and_then(Value::as_str)
        .map(str::to_owned);

    let id: i32 = sqlx::query_scalar(
        r#"INSERT INTO timesheet ("userId", "companyId", "periodStart", "periodEnd", "payPeriodType",
               status, "totalSeconds", "payRate", "rateType", "totalAmount", currency, "submittedAt", notes)
           VALUES ($1, $2, $3, $4, $5::"PayPeriodType", 'PENDING'::"TimesheetStatus", $6, $7,
                   $8::"RateType", $9, $10, $11, $12)
           RETURNING id"#,
    )
    .bind(user_id)
    .bind(company_id)
    .bind(period_start)
    .bind(period_end)
    .bind(&rate.pay_period_type)
    .bind(secs as i32)
    .bind(rate.pay_rate)
    .bind(&rate.rate_type)
    .bind(amount)
    .bind(&currency)
    .bind(now_naive())
    .bind(&notes)
    .fetch_one(&state.db)
    .await?;
    let ts = fetch_by_id(state, id).await?;

    let employee: Option<(String, String)> =
        sqlx::query_as("SELECT name, username FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_optional(&state.db)
            .await?;
    let employee_name = employee.map_or_else(|| "Employee".to_owned(), |e| e.0);

    let base = host_be(state);
    let expired_at = now_naive() + Duration::days(7);
    for (approver_id, name, username) in approvers(&state.db, company_id).await? {
        let token: String = sqlx::query_scalar(
            r#"INSERT INTO "timesheetApprovalToken" ("timesheetId", "approverId", "expiredAt")
               VALUES ($1, $2, $3) RETURNING uuid::text"#,
        )
        .bind(ts.id)
        .bind(approver_id)
        .bind(expired_at)
        .fetch_one(&state.db)
        .await?;
        send_approval_request(
            state,
            ApprovalRequest {
                approver_email: username,
                approver_name: name,
                employee_name: employee_name.clone(),
                period_start: date_str(ts.period_start),
                period_end: date_str(ts.period_end),
                pay_period_type: ts.pay_period_type.clone(),
                total_hours: hours2(ts.secs()),
                total_amount: ts.total_amount,
                currency: ts.currency.clone(),
                notes: ts.notes.clone(),
                approve_url: format!("{base}/timesheets/email-action?token={token}&action=approve"),
                reject_url: format!("{base}/timesheets/email-action?token={token}&action=reject"),
            },
        );
    }

    let mut data = ts.core_json();
    data.insert("notes".into(), json!(ts.notes));
    Ok(json!({ "success": true, "message": "Timesheet submitted successfully", "data": data }))
}

/// Node: src/timesheets/timesheets.controller.ts:78 (`getMyTimesheets`)
pub async fn get_my_timesheets(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedQuery(query): ValidatedQuery<MyQuery>,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    let mut qb = QueryBuilder::<Postgres>::new(TS_SELECT);
    qb.push(r#" WHERE ts."userId" = "#).push_bind(user_id);
    if let Some(s) = &query.status {
        qb.push(" AND ts.status::text = ").push_bind(s.clone());
    }
    if let Some(Some(year)) = query.year_value() {
        let y =
            i32::try_from(year).map_err(|_| ApiError::internal(anyhow::anyhow!("Invalid year")))?;
        let start = NaiveDate::from_ymd_opt(y, 1, 1);
        let end = NaiveDate::from_ymd_opt(y, 12, 31);
        let (Some(start), Some(end)) = (start, end) else {
            return Err(ApiError::internal(anyhow::anyhow!("Invalid time value")));
        };
        qb.push(r#" AND ts."periodStart" >= "#).push_bind(start);
        qb.push(r#" AND ts."periodStart" <= "#).push_bind(end);
    }
    qb.push(r#" ORDER BY ts."periodStart" DESC, ts.id DESC"#);
    let rows = qb.build_query_as::<Ts>().fetch_all(&state.db).await?;
    let data: Vec<Value> = rows
        .iter()
        .map(|ts| {
            let mut m = ts.core_json();
            m.insert("approvedAt".into(), jd_opt(ts.approved_at));
            m.insert("approvedBy".into(), ts.approved_by());
            m.insert("rejectedReason".into(), json!(ts.rejected_reason));
            m.insert("notes".into(), json!(ts.notes));
            Value::Object(m)
        })
        .collect();
    Ok(Json(
        json!({ "success": true, "message": "success", "data": data }),
    ))
}

/// Node: src/timesheets/timesheets.controller.ts:92 (`getMyCurrent`)
pub async fn get_my_current(
    State(state): State<AppState>,
    ctx: RequestCtx,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    let Some(company_id) = find_company_id(&state.db, user_id).await? else {
        return Ok(Json(fail("Company not found")));
    };
    let currency: Option<String> = sqlx::query_scalar("SELECT currency FROM company WHERE id = $1")
        .bind(company_id)
        .fetch_optional(&state.db)
        .await?;
    let Some(currency) = currency else {
        return Ok(Json(fail("Company not found")));
    };
    let Some(rate) = active_pay_rate(&state, user_id, company_id).await? else {
        return Ok(Json(fail("Pay rate not configured")));
    };
    let (start, end) = period_bounds(&rate.pay_period_type, now_naive().date());

    let existing: Option<(String, NaiveDateTime, String)> = sqlx::query_as(
        r#"SELECT uuid::text, "submittedAt", status::text FROM timesheet
           WHERE "userId" = $1 AND "companyId" = $2 AND "periodStart" = $3
           ORDER BY "createdAt" DESC LIMIT 1"#,
    )
    .bind(user_id)
    .bind(company_id)
    .bind(start)
    .fetch_optional(&state.db)
    .await?;

    let secs = total_seconds(&state.db, user_id, start, end).await?;
    let estimated = total_amount(secs, rate.pay_rate, &rate.rate_type);
    let status = existing.as_ref().map_or("NOT_SUBMITTED", |e| e.2.as_str());
    Ok(Json(json!({
        "success": true,
        "message": "success",
        "data": {
            "status": status,
            "periodStart": date_str(start),
            "periodEnd": date_str(end),
            "payPeriodType": rate.pay_period_type,
            "totalSeconds": secs,
            "totalHours": jsfmt::num(hours2(secs)),
            "estimatedAmount": fnum(estimated),
            "currency": currency,
            "timesheet": existing.map(|(uuid, submitted, status)| json!({
                "uuid": uuid, "submittedAt": jd(submitted), "status": status
            })),
        }
    })))
}

/// Node: src/timesheets/timesheets.controller.ts:111 (`withdrawMyTimesheet`)
pub async fn withdraw_my(
    State(state): State<AppState>,
    ctx: RequestCtx,
    PathParams(uuid): PathParams<String>,
) -> ApiResult<Json<Value>> {
    let uuid = parse_uuid(uuid)?;
    let user_id = ctx.require_user_id()?;
    let row: Option<(i32, String, bool)> = sqlx::query_as(
        r#"SELECT ts.id, ts.status::text,
                  EXISTS(SELECT 1 FROM "timesheetInvoice" i WHERE i."timesheetId" = ts.id)
           FROM timesheet ts WHERE ts.uuid = $1::uuid AND ts."userId" = $2 LIMIT 1"#,
    )
    .bind(&uuid)
    .bind(user_id)
    .fetch_optional(&state.db)
    .await?;
    let Some((id, status, invoiced)) = row else {
        return Ok(Json(fail("Timesheet not found")));
    };
    if status != "PENDING" {
        return Ok(Json(fail("Only PENDING timesheets can be withdrawn")));
    }
    if invoiced {
        return Ok(Json(fail(
            "Cannot withdraw timesheet that has been invoiced. Contact admin to remove the invoice first.",
        )));
    }
    sqlx::query("DELETE FROM timesheet WHERE id = $1")
        .bind(id)
        .execute(&state.db)
        .await?;
    Ok(Json(
        json!({ "success": true, "message": "Timesheet withdrawn successfully", "data": { "uuid": uuid } }),
    ))
}

// ───────────────────────── admin / manager ─────────────────────────

/// Node: src/timesheets/timesheets.controller.ts:132 (`adminCreate`, `@HttpCode(201)`)
pub async fn admin_create(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedJson(dto): ValidatedJson<AdminCreateBody>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let admin_id = ctx.require_user_id()?;
    Ok(created(admin_create_inner(&state, admin_id, &dto).await?))
}

/// Node: src/timesheets/timesheets.service.ts:615 (`adminCreate`)
async fn admin_create_inner(
    state: &AppState,
    admin_id: i32,
    dto: &AdminCreateBody,
) -> ApiResult<Value> {
    let Some(company_id) = find_company_id(&state.db, admin_id).await? else {
        return Ok(fail("Company not found"));
    };
    let user_uuid = provided(&dto.user_uuid)
        .and_then(Value::as_str)
        .unwrap_or_default();
    let employee: Option<i32> = sqlx::query_scalar("SELECT id FROM users WHERE uuid = $1::uuid")
        .bind(user_uuid)
        .fetch_optional(&state.db)
        .await?;
    let Some(employee_id) = employee else {
        return Ok(fail("Employee not found"));
    };

    let start = ymd_or_500(
        provided(&dto.period_start)
            .and_then(Value::as_str)
            .unwrap_or_default(),
    )?;
    let end = ymd_or_500(
        provided(&dto.period_end)
            .and_then(Value::as_str)
            .unwrap_or_default(),
    )?;
    if start > end {
        return Ok(fail("Period start must be before or equal to period end"));
    }

    let existing: Option<i32> = sqlx::query_scalar(
        r#"SELECT id FROM timesheet
           WHERE "userId" = $1 AND "companyId" = $2 AND "periodStart" = $3
             AND status::text = ANY($4) LIMIT 1"#,
    )
    .bind(employee_id)
    .bind(company_id)
    .bind(start)
    .bind(vec!["PENDING", "APPROVED"])
    .fetch_optional(&state.db)
    .await?;
    if existing.is_some() {
        return Ok(fail(
            "Employee already has a pending or approved timesheet for this period",
        ));
    }

    let mut pay_rate = provided(&dto.pay_rate).and_then(Value::as_f64);
    let mut rate_type = provided(&dto.rate_type)
        .and_then(Value::as_str)
        .unwrap_or("HOURLY")
        .to_owned();
    if pay_rate.is_none() {
        let Some(active) = active_pay_rate(state, employee_id, company_id).await? else {
            return Ok(fail(
                "No active pay rate configured for this employee and no override provided",
            ));
        };
        pay_rate = active.pay_rate;
        rate_type = active.rate_type;
    }

    let secs = total_seconds(&state.db, employee_id, start, end).await?;
    let amount = total_amount(secs, pay_rate, &rate_type);
    let currency: Option<String> = sqlx::query_scalar("SELECT currency FROM company WHERE id = $1")
        .bind(company_id)
        .fetch_optional(&state.db)
        .await?;
    let notes = provided(&dto.notes)
        .and_then(Value::as_str)
        .map(str::to_owned);
    let pay_period_type = provided(&dto.pay_period_type)
        .and_then(Value::as_str)
        .unwrap_or_default();

    let id: i32 = sqlx::query_scalar(
        r#"INSERT INTO timesheet ("userId", "companyId", "periodStart", "periodEnd", "payPeriodType",
               status, "totalSeconds", "payRate", "rateType", "totalAmount", currency, "submittedAt", notes)
           VALUES ($1, $2, $3, $4, $5::"PayPeriodType", 'PENDING'::"TimesheetStatus", $6, $7,
                   $8::"RateType", $9, $10, $11, $12)
           RETURNING id"#,
    )
    .bind(employee_id)
    .bind(company_id)
    .bind(start)
    .bind(end)
    .bind(pay_period_type)
    .bind(secs as i32)
    .bind(pay_rate)
    .bind(&rate_type)
    .bind(amount)
    .bind(currency.unwrap_or_else(|| "IDR".to_owned()))
    .bind(now_naive())
    .bind(&notes)
    .fetch_one(&state.db)
    .await?;
    let ts = fetch_by_id(state, id).await?;
    let mut data = ts.core_json();
    data.insert("notes".into(), json!(ts.notes));
    Ok(json!({ "success": true, "message": "Timesheet created successfully", "data": data }))
}

/// Node: src/timesheets/timesheets.controller.ts:146 (`findAll`)
pub async fn find_all(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedQuery(query): ValidatedQuery<AdminQuery>,
) -> ApiResult<Json<Value>> {
    let admin_id = ctx.require_user_id()?;
    let company_id = if is_system_admin(&state.db, admin_id).await? {
        None
    } else {
        match find_company_id(&state.db, admin_id).await? {
            Some(c) => Some(c),
            None => return Ok(Json(fail("Company not found"))),
        }
    };

    let mut qb = QueryBuilder::<Postgres>::new(TS_SELECT);
    qb.push(" WHERE TRUE");
    if let Some(c) = company_id {
        qb.push(r#" AND ts."companyId" = "#).push_bind(c);
    }
    if let Some(s) = &query.status {
        qb.push(" AND ts.status::text = ").push_bind(s.clone());
    }
    if let Some(u) = &query.user_uuid {
        let target: Option<i32> = sqlx::query_scalar("SELECT id FROM users WHERE uuid = $1::uuid")
            .bind(u)
            .fetch_optional(&state.db)
            .await?;
        // Node: pengguna tak ditemukan = filter user dilewati.
        if let Some(t) = target {
            qb.push(r#" AND ts."userId" = "#).push_bind(t);
        }
    }
    if let Some(s) = &query.period_start {
        qb.push(r#" AND ts."periodStart" >= "#)
            .push_bind(ymd_or_500(s)?);
    }
    if let Some(s) = &query.period_end {
        qb.push(r#" AND ts."periodStart" <= "#)
            .push_bind(ymd_or_500(s)?);
    }
    qb.push(r#" ORDER BY ts."submittedAt" DESC, ts.id DESC"#);
    let rows = qb.build_query_as::<Ts>().fetch_all(&state.db).await?;
    let total = rows.len();
    let data: Vec<Value> = rows
        .iter()
        .map(|ts| {
            let mut m = ts.core_json();
            m.insert("approvedAt".into(), jd_opt(ts.approved_at));
            m.insert("approvedBy".into(), ts.approved_by());
            m.insert("rejectedReason".into(), json!(ts.rejected_reason));
            m.insert("user".into(), ts.user());
            Value::Object(m)
        })
        .collect();
    Ok(Json(
        json!({ "success": true, "message": "success", "total": total, "data": data }),
    ))
}

/// Node: src/timesheets/timesheets.controller.ts:158 (`findOne`)
pub async fn find_one(
    State(state): State<AppState>,
    ctx: RequestCtx,
    PathParams(uuid): PathParams<String>,
) -> ApiResult<Json<Value>> {
    let uuid = parse_uuid(uuid)?;
    let admin_id = ctx.require_user_id()?;
    let company_id = if is_system_admin(&state.db, admin_id).await? {
        None
    } else {
        match find_company_id(&state.db, admin_id).await? {
            Some(c) => Some(c),
            None => return Ok(Json(fail("Company not found"))),
        }
    };
    let Some(ts) = fetch_by_uuid(&state, &uuid, company_id).await? else {
        return Ok(Json(fail("Timesheet not found")));
    };
    let rows = timelines(&state, ts.user_id, ts.period_start, ts.period_end).await?;
    let b = build_breakdown(&rows);
    let pct = if ts.total_seconds > 0 {
        crate::calc::js_round(b.billable as f64 / ts.secs() as f64 * 100.0)
    } else {
        0
    };
    Ok(Json(json!({
        "success": true,
        "message": "success",
        "data": {
            "uuid": ts.uuid,
            "periodStart": date_str(ts.period_start),
            "periodEnd": date_str(ts.period_end),
            "payPeriodType": ts.pay_period_type,
            "status": ts.status,
            "totalSeconds": ts.total_seconds,
            "totalHours": ts.total_hours(),
            "payRate": fnum(ts.pay_rate),
            "rateType": ts.rate_type,
            "totalAmount": fnum(ts.total_amount),
            "currency": ts.currency,
            "submittedAt": jd(ts.submitted_at),
            "notes": ts.notes,
            "user": ts.user(),
            "approvedBy": ts.approved_by(),
            "approvedAt": jd_opt(ts.approved_at),
            "rejectedReason": ts.rejected_reason,
            "billableSeconds": b.billable,
            "nonBillableSeconds": b.non_billable,
            "billablePercentage": pct,
            "dailyBreakdown": daily_json(&b.days),
        }
    })))
}

/// Node: src/timesheets/timesheets.controller.ts:232 (`approve`)
pub async fn approve(
    State(state): State<AppState>,
    ctx: RequestCtx,
    PathParams(uuid): PathParams<String>,
) -> ApiResult<Json<Value>> {
    let uuid = parse_uuid(uuid)?;
    let admin_id = ctx.require_user_id()?;
    Ok(Json(decide(&state, &uuid, admin_id, true, None).await?))
}

/// Node: src/timesheets/timesheets.controller.ts:243 (`reject`)
pub async fn reject(
    State(state): State<AppState>,
    ctx: RequestCtx,
    PathParams(uuid): PathParams<String>,
    ValidatedJson(dto): ValidatedJson<RejectBody>,
) -> ApiResult<Json<Value>> {
    let uuid = parse_uuid(uuid)?;
    let admin_id = ctx.require_user_id()?;
    let reason = provided(&dto.rejected_reason)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    Ok(Json(
        decide(&state, &uuid, admin_id, false, Some(reason)).await?,
    ))
}

/// Node: src/timesheets/timesheets.service.ts:469 (`approve`) dan :541 (`reject`)
async fn decide(
    state: &AppState,
    uuid: &str,
    admin_id: i32,
    approve: bool,
    reason: Option<String>,
) -> ApiResult<Value> {
    let verb = if approve { "approved" } else { "rejected" };
    let Some(ts) = fetch_by_uuid(state, uuid, None).await? else {
        return Ok(fail("Timesheet not found"));
    };
    if ts.status != "PENDING" {
        let m = if approve {
            "Only PENDING timesheets can be approved"
        } else {
            "Only PENDING timesheets can be rejected"
        };
        return Ok(fail(m));
    }
    if !can_act_on_company(&state.db, admin_id, ts.company_id).await? {
        return Ok(fail(&format!(
            "You are not authorized to {} this timesheet",
            if approve { "approve" } else { "reject" }
        )));
    }
    let now = now_naive();
    if approve {
        sqlx::query(
            r#"UPDATE timesheet SET status = 'APPROVED'::"TimesheetStatus", "approvedById" = $2,
               "approvedAt" = $3, "updatedAt" = $3 WHERE id = $1"#,
        )
        .bind(ts.id)
        .bind(admin_id)
        .bind(now)
        .execute(&state.db)
        .await?;
    } else {
        sqlx::query(
            r#"UPDATE timesheet SET status = 'REJECTED'::"TimesheetStatus", "rejectedReason" = $4,
               "approvedById" = $2, "approvedAt" = $3, "updatedAt" = $3 WHERE id = $1"#,
        )
        .bind(ts.id)
        .bind(admin_id)
        .bind(now)
        .bind(&reason)
        .execute(&state.db)
        .await?;
    }
    let updated = fetch_by_id(state, ts.id).await?;

    let employee: Option<(String, String)> =
        sqlx::query_as("SELECT name, username FROM users WHERE id = $1")
            .bind(ts.user_id)
            .fetch_optional(&state.db)
            .await?;
    let approver: Option<String> = sqlx::query_scalar("SELECT name FROM users WHERE id = $1")
        .bind(admin_id)
        .fetch_optional(&state.db)
        .await?;
    send_status(
        state,
        StatusMail {
            employee_email: employee.as_ref().map(|e| e.1.clone()).unwrap_or_default(),
            employee_name: employee.map_or_else(|| "Employee".to_owned(), |e| e.0),
            is_approved: approve,
            period_start: date_str(updated.period_start),
            period_end: date_str(updated.period_end),
            pay_period_type: updated.pay_period_type.clone(),
            total_hours: hours2(updated.secs()),
            total_amount: updated.total_amount,
            currency: updated.currency.clone(),
            approver_name: approver.unwrap_or_else(|| "Administrator".to_owned()),
            rejected_reason: if approve {
                None
            } else {
                updated.rejected_reason.clone()
            },
        },
    );

    let mut data = Map::new();
    data.insert("uuid".into(), json!(updated.uuid));
    data.insert("status".into(), json!(updated.status));
    if !approve {
        data.insert("rejectedReason".into(), json!(updated.rejected_reason));
    }
    data.insert("approvedAt".into(), jd_opt(updated.approved_at));
    data.insert("approvedBy".into(), updated.approved_by());
    Ok(json!({ "success": true, "message": format!("Timesheet {verb}"), "data": data }))
}

/// Node: src/timesheets/timesheets.controller.ts:258 (`editTimesheet`)
pub async fn edit(
    State(state): State<AppState>,
    ctx: RequestCtx,
    PathParams(uuid): PathParams<String>,
    ValidatedJson(dto): ValidatedJson<EditBody>,
) -> ApiResult<Json<Value>> {
    let uuid = parse_uuid(uuid)?;
    let admin_id = ctx.require_user_id()?;
    Ok(Json(edit_inner(&state, &uuid, &dto, admin_id).await?))
}

fn null_in_non_null_column() -> ApiError {
    // Prisma: `null` untuk kolom non-nullable = galat validasi → 500.
    ApiError::internal(anyhow::anyhow!("Argument is missing / null is not allowed"))
}

/// Node: src/timesheets/timesheets.service.ts:703 (`editTimesheet`)
async fn edit_inner(
    state: &AppState,
    uuid: &str,
    dto: &EditBody,
    admin_id: i32,
) -> ApiResult<Value> {
    let Some(company_id) = find_company_id(&state.db, admin_id).await? else {
        return Ok(fail("Company not found"));
    };
    let Some(ts) = fetch_by_uuid(state, uuid, Some(company_id)).await? else {
        return Ok(fail("Timesheet not found"));
    };

    // Tarif: MANAGER tidak boleh mengubah tarif pemilik yang jabatannya di atasnya.
    if dto.pay_rate.is_some() || dto.rate_type.is_some() {
        let mut conn = state.db.acquire().await?;
        let actor = require_actor_role(&mut conn, admin_id, company_id).await?;
        assert_target_within_rank(&mut conn, &actor, ts.user_id, company_id).await?;
    }
    if ts.status == "REJECTED" {
        return Ok(fail("Rejected timesheets cannot be edited"));
    }

    let now = now_naive();
    let mut qb = QueryBuilder::<Postgres>::new(r#"UPDATE timesheet SET "updatedAt" = "#);
    qb.push_bind(now);

    if let Some(n) = &dto.notes {
        qb.push(", notes = ")
            .push_bind(n.as_str().map(str::to_owned));
    }

    let mut new_total: Option<Option<f64>> = None;
    if let Some(rate) = provided(&dto.pay_rate).and_then(Value::as_f64) {
        qb.push(r#", "payRate" = "#).push_bind(rate);
        let rt = provided(&dto.rate_type)
            .and_then(Value::as_str)
            .unwrap_or(&ts.rate_type);
        new_total = Some(total_amount(ts.secs(), Some(rate), rt));
    }
    if let Some(rt) = &dto.rate_type {
        let rt = rt.as_str().ok_or_else(null_in_non_null_column)?;
        qb.push(r#", "rateType" = "#)
            .push_bind(rt.to_owned())
            .push(r#"::"RateType""#);
        let rate = provided(&dto.pay_rate)
            .and_then(Value::as_f64)
            .or(ts.pay_rate);
        new_total = Some(total_amount(ts.secs(), rate, rt));
    }
    if let Some(t) = new_total {
        qb.push(r#", "totalAmount" = "#).push_bind(t);
    }
    qb.push(" WHERE id = ").push_bind(ts.id);
    qb.push(r#" RETURNING uuid::text, notes, "payRate", "rateType"::text, "totalAmount""#);
    let (u, notes, pay_rate, rate_type, amount): (
        String,
        Option<String>,
        Option<f64>,
        String,
        Option<f64>,
    ) = qb.build_query_as().fetch_one(&state.db).await?;
    Ok(json!({
        "success": true,
        "message": "Timesheet updated successfully",
        "data": {
            "uuid": u, "notes": notes, "payRate": fnum(pay_rate), "rateType": rate_type,
            "totalAmount": fnum(amount), "updatedAt": jd(now),
        }
    }))
}

/// Node: src/timesheets/timesheets.controller.ts:274 (`recalculate`)
pub async fn recalculate(
    State(state): State<AppState>,
    ctx: RequestCtx,
    PathParams(uuid): PathParams<String>,
) -> ApiResult<Json<Value>> {
    let uuid = parse_uuid(uuid)?;
    let admin_id = ctx.require_user_id()?;
    let Some(company_id) = find_company_id(&state.db, admin_id).await? else {
        return Ok(Json(fail("Company not found")));
    };
    let Some(ts) = fetch_by_uuid(&state, &uuid, Some(company_id)).await? else {
        return Ok(Json(fail("Timesheet not found")));
    };
    if ts.status == "REJECTED" {
        return Ok(Json(fail("Rejected timesheets cannot be recalculated")));
    }
    let secs = total_seconds(&state.db, ts.user_id, ts.period_start, ts.period_end).await?;
    let amount = total_amount(secs, ts.pay_rate, &ts.rate_type);
    let now = now_naive();
    sqlx::query(r#"UPDATE timesheet SET "totalSeconds" = $2, "totalAmount" = $3, "updatedAt" = $4 WHERE id = $1"#)
        .bind(ts.id)
        .bind(secs as i32)
        .bind(amount)
        .bind(now)
        .execute(&state.db)
        .await?;
    Ok(Json(json!({
        "success": true,
        "message": "Timesheet recalculated successfully",
        "data": {
            "uuid": ts.uuid,
            "totalSeconds": secs,
            "totalHours": jsfmt::num(hours2(secs)),
            "totalAmount": fnum(amount),
            "updatedAt": jd(now),
        }
    })))
}

/// Node: src/timesheets/timesheets.controller.ts:290 (`reopen`)
pub async fn reopen(
    State(state): State<AppState>,
    ctx: RequestCtx,
    PathParams(uuid): PathParams<String>,
    ValidatedJson(dto): ValidatedJson<ReopenBody>,
) -> ApiResult<Json<Value>> {
    let uuid = parse_uuid(uuid)?;
    let admin_id = ctx.require_user_id()?;
    let Some(company_id) = find_company_id(&state.db, admin_id).await? else {
        return Ok(Json(fail("Company not found")));
    };
    let row: Option<(i32, String)> = sqlx::query_as(
        r#"SELECT id, status::text FROM timesheet WHERE uuid = $1::uuid AND "companyId" = $2 LIMIT 1"#,
    )
    .bind(&uuid)
    .bind(company_id)
    .fetch_optional(&state.db)
    .await?;
    let Some((id, status)) = row else {
        return Ok(Json(fail("Timesheet not found")));
    };
    if status == "PENDING" {
        return Ok(Json(fail(
            "Only APPROVED or REJECTED timesheets can be reopened",
        )));
    }
    let notes = match provided(&dto.reason)
        .and_then(Value::as_str)
        .filter(|r| !r.is_empty())
    {
        Some(r) => format!("[REOPENED] {r}"),
        None => "[REOPENED]".to_owned(),
    };
    let now = now_naive();
    sqlx::query(
        r#"UPDATE timesheet SET status = 'PENDING'::"TimesheetStatus", "approvedById" = NULL,
           "approvedAt" = NULL, "rejectedReason" = NULL, "updatedAt" = $2, notes = $3 WHERE id = $1"#,
    )
    .bind(id)
    .bind(now)
    .bind(&notes)
    .execute(&state.db)
    .await?;
    Ok(Json(json!({
        "success": true,
        "message": "Timesheet reopened successfully",
        "data": { "uuid": uuid, "status": "PENDING", "notes": notes, "updatedAt": jd(now) }
    })))
}

/// Node: src/timesheets/timesheets.controller.ts:306 (`deleteTimesheet`)
pub async fn delete(
    State(state): State<AppState>,
    ctx: RequestCtx,
    PathParams(uuid): PathParams<String>,
) -> ApiResult<Json<Value>> {
    let uuid = parse_uuid(uuid)?;
    let admin_id = ctx.require_user_id()?;
    let Some(company_id) = find_company_id(&state.db, admin_id).await? else {
        return Ok(Json(fail("Company not found")));
    };
    let row: Option<(i32, bool)> = sqlx::query_as(
        r#"SELECT ts.id, EXISTS(SELECT 1 FROM "timesheetInvoice" i WHERE i."timesheetId" = ts.id)
           FROM timesheet ts WHERE ts.uuid = $1::uuid AND ts."companyId" = $2 LIMIT 1"#,
    )
    .bind(&uuid)
    .bind(company_id)
    .fetch_optional(&state.db)
    .await?;
    let Some((id, invoiced)) = row else {
        return Ok(Json(fail("Timesheet not found")));
    };
    if invoiced {
        return Ok(Json(fail(
            "Cannot delete timesheet that has been invoiced. Remove the invoice first.",
        )));
    }
    sqlx::query("DELETE FROM timesheet WHERE id = $1")
        .bind(id)
        .execute(&state.db)
        .await?;
    Ok(Json(
        json!({ "success": true, "message": "Timesheet deleted successfully", "data": { "uuid": uuid } }),
    ))
}

// ───────────────────────── email action (publik) ─────────────────────────

/// `res.redirect(url)` Express, lihat [`mz_core::redirect`].
fn redirect(headers: &axum::http::HeaderMap, url: &str) -> Response {
    mz_core::redirect::redirect_302(headers, url)
}

/// Node: path statis (`submit`, `my`, `email-action`) dengan method yang hanya dimiliki
/// `:uuid` (GET/DELETE) ditelan `ParseUUIDPipe` -> 400 `Validation failed (uuid is expected)`.
pub async fn static_as_uuid(ctx: RequestCtx, uri: axum::http::Uri) -> ApiResult<Json<Value>> {
    let segment = uri.path().rsplit('/').next().unwrap_or_default().to_owned();
    parse_uuid(segment)?;
    ctx.require_user_id()?;
    Err(ApiError::not_found("Cannot handle request"))
}

/// Node: src/timesheets/timesheets.controller.ts:51 (`handleEmailAction`, `@HttpCode(302)`)
pub async fn email_action(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Response> {
    let token = q.get("token").cloned().unwrap_or_default();
    let action = q.get("action").cloned().unwrap_or_default();
    Ok(redirect(
        &headers,
        &handle_email_action(&state, &token, &action).await?,
    ))
}

#[derive(sqlx::FromRow)]
struct TokenRow {
    #[sqlx(rename = "timesheetId")]
    timesheet_id: i32,
    #[sqlx(rename = "approverId")]
    approver_id: i32,
    #[sqlx(rename = "usedAt")]
    used_at: Option<NaiveDateTime>,
    #[sqlx(rename = "expiredAt")]
    expired_at: NaiveDateTime,
}

/// Node: src/timesheets/timesheets.service.ts:1352 (`handleEmailAction`)
async fn handle_email_action(state: &AppState, token: &str, action: &str) -> ApiResult<String> {
    let fe = host_fe(state);
    if action != "approve" && action != "reject" {
        return Ok(format!("{fe}/timesheet"));
    }
    // Token bukan UUID: Prisma melempar → 500 (cast uuid Postgres gagal).
    let rec: Option<TokenRow> = sqlx::query_as(
        r#"SELECT "timesheetId", "approverId", "usedAt", "expiredAt" FROM "timesheetApprovalToken" WHERE uuid = $1::uuid"#,
    )
    .bind(token)
    .fetch_optional(&state.db)
    .await?;
    let Some(rec) = rec else {
        return Ok(format!("{fe}/timesheet"));
    };
    let ts_id = rec.timesheet_id;
    let ts = fetch_by_id(state, ts_id).await?;
    let detail = format!("{fe}/timesheet/{}", ts.uuid);

    if rec.used_at.is_some() || now_naive() > rec.expired_at || ts.status != "PENDING" {
        return Ok(detail);
    }
    let approve = action == "approve";
    match apply_email_action(state, token, ts.id, rec.approver_id, approve).await {
        Ok(true) => {}
        Ok(false) | Err(_) => return Ok(detail),
    }

    let employee: Option<(String, String)> =
        sqlx::query_as("SELECT name, username FROM users WHERE id = $1")
            .bind(ts.user_id)
            .fetch_optional(&state.db)
            .await?;
    let approver: Option<String> = sqlx::query_scalar("SELECT name FROM users WHERE id = $1")
        .bind(rec.approver_id)
        .fetch_optional(&state.db)
        .await?;
    send_status(
        state,
        StatusMail {
            employee_email: employee.as_ref().map(|e| e.1.clone()).unwrap_or_default(),
            employee_name: employee.map_or_else(|| "Employee".to_owned(), |e| e.0),
            is_approved: approve,
            period_start: date_str(ts.period_start),
            period_end: date_str(ts.period_end),
            pay_period_type: ts.pay_period_type.clone(),
            total_hours: hours2(ts.secs()),
            total_amount: ts.total_amount,
            currency: ts.currency.clone(),
            approver_name: approver.unwrap_or_else(|| "Administrator".to_owned()),
            rejected_reason: if approve {
                None
            } else {
                Some("Rejected via email".to_owned())
            },
        },
    );
    Ok(detail)
}

/// Transaksi sekali-pakai: tandai token lalu ubah status. `Ok(false)` = token sudah dipakai.
async fn apply_email_action(
    state: &AppState,
    token: &str,
    ts_id: i32,
    approver_id: i32,
    approve: bool,
) -> sqlx::Result<bool> {
    let mut tx = state.db.begin().await?;
    let now = now_naive();
    let marked = sqlx::query(r#"UPDATE "timesheetApprovalToken" SET "usedAt" = $2 WHERE uuid = $1::uuid AND "usedAt" IS NULL"#)
        .bind(token)
        .bind(now)
        .execute(&mut *tx)
        .await?;
    if marked.rows_affected() == 0 {
        tx.rollback().await?;
        return Ok(false);
    }
    if approve {
        sqlx::query(
            r#"UPDATE timesheet SET status = 'APPROVED'::"TimesheetStatus", "approvedById" = $2,
               "approvedAt" = $3, "updatedAt" = $3 WHERE id = $1"#,
        )
        .bind(ts_id)
        .bind(approver_id)
        .bind(now)
        .execute(&mut *tx)
        .await?;
    } else {
        sqlx::query(
            r#"UPDATE timesheet SET status = 'REJECTED'::"TimesheetStatus", "rejectedReason" = 'Rejected via email',
               "approvedById" = $2, "approvedAt" = $3, "updatedAt" = $3 WHERE id = $1"#,
        )
        .bind(ts_id)
        .bind(approver_id)
        .bind(now)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(true)
}

// ───────────────────────── ekspor ─────────────────────────

/// Kasus umum ekspor: company admin + timesheet di company itu (tanpa bypass SYSTEM_ADMIN).
async fn export_target(state: &AppState, uuid: &str, admin_id: i32) -> ApiResult<Ts> {
    let Some(company_id) = find_company_id(&state.db, admin_id).await? else {
        return Err(plain_fail(StatusCode::BAD_REQUEST, "Company not found"));
    };
    fetch_by_uuid(state, uuid, Some(company_id))
        .await?
        .ok_or_else(|| plain_fail(StatusCode::NOT_FOUND, "Timesheet not found"))
}

/// `filename.replace(/\s+/g, '-')`.
fn dash_whitespace(s: &str) -> String {
    let mut out = String::new();
    let mut in_ws = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !in_ws {
                out.push('-');
            }
            in_ws = true;
        } else {
            out.push(c);
            in_ws = false;
        }
    }
    out
}

pub(crate) fn file_response(
    bytes: Vec<u8>,
    content_type: &'static str,
    filename: &str,
    with_length: bool,
) -> ApiResult<Response> {
    let disposition =
        HeaderValue::from_bytes(format!("attachment; filename=\"{filename}\"").as_bytes())
            .map_err(|e| ApiError::internal(anyhow::anyhow!("invalid header: {e}")))?;
    let mut b = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CONTENT_DISPOSITION, disposition);
    if with_length {
        b = b.header(header::CONTENT_LENGTH, bytes.len());
    }
    b.body(Body::from(bytes))
        .map_err(|e| ApiError::internal(anyhow::anyhow!("response: {e}")))
}

/// Node: src/timesheets/timesheets.controller.ts:176 (`exportPdf`)
pub async fn export_pdf(
    State(state): State<AppState>,
    ctx: RequestCtx,
    PathParams(uuid): PathParams<String>,
) -> ApiResult<Response> {
    let uuid = parse_uuid(uuid)?;
    let admin_id = ctx.require_user_id()?;
    let ts = export_target(&state, &uuid, admin_id).await?;
    let rows = timelines(&state, ts.user_id, ts.period_start, ts.period_end).await?;
    let b = build_breakdown(&rows);
    let data = TimesheetPdfData {
        employee_name: ts.u_name.clone().unwrap_or_else(|| "Unknown".into()),
        employee_email: ts.u_username.clone().unwrap_or_default(),
        period_start: ts.period_start,
        period_end: ts.period_end,
        pay_period_type: ts.pay_period_type.clone(),
        status: ts.status.clone(),
        total_seconds: ts.secs(),
        pay_rate: ts.pay_rate,
        rate_type: Some(ts.rate_type.clone()),
        total_amount: ts.total_amount,
        currency: Some(ts.currency.clone()),
        submitted_at: Some(ts.submitted_at.date()),
        approved_by_name: ts.ab_name.clone(),
        rejected_reason: ts.rejected_reason.clone(),
        notes: ts.notes.clone(),
        billable_seconds: Some(b.billable),
        non_billable_seconds: Some(b.non_billable),
        daily: b
            .days
            .iter()
            .map(|d| DayEntry {
                date: d.date,
                total_seconds: d.total,
                total_hours: hours2(d.total),
                projects: d
                    .projects
                    .iter()
                    .map(|p| DayProject {
                        project_name: p.name.clone(),
                        total_seconds: p.total,
                    })
                    .collect(),
            })
            .collect(),
    };
    let bytes = timesheet_pdf(&data);
    let name = ts.u_name.as_deref().unwrap_or("Employee");
    let filename = dash_whitespace(&format!(
        "Timesheet-{name}-{}.pdf",
        date_str(ts.period_start)
    ));
    file_response(bytes, "application/pdf", &filename, true)
}

/// Node: src/timesheets/timesheets.controller.ts:190 (`exportCsv`)
pub async fn export_csv(
    State(state): State<AppState>,
    ctx: RequestCtx,
    PathParams(uuid): PathParams<String>,
) -> ApiResult<Response> {
    let uuid = parse_uuid(uuid)?;
    let admin_id = ctx.require_user_id()?;
    let ts = export_target(&state, &uuid, admin_id).await?;
    let rows = timelines(&state, ts.user_id, ts.period_start, ts.period_end).await?;
    let b = build_breakdown(&rows);
    let name = ts.u_name.clone().unwrap_or_else(|| "Unknown".into());
    let csv = build_csv(
        &CsvInput {
            employee_name: &name,
            period_start: ts.period_start,
            period_end: ts.period_end,
            pay_period_type: &ts.pay_period_type,
            status: &ts.status,
            submitted_at: Some(ts.submitted_at.date()),
            total_seconds: ts.secs(),
            total_amount: ts.total_amount,
            currency: &ts.currency,
        },
        &b.days,
    );
    let filename = dash_whitespace(&format!(
        "Timesheet-{name}-{}.csv",
        date_str(ts.period_start)
    ));
    file_response(
        format!("\u{FEFF}{csv}").into_bytes(),
        "text/csv; charset=utf-8",
        &filename,
        false,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filename_whitespace_collapses() {
        assert_eq!(
            dash_whitespace("Timesheet-Budi  Santoso-2026-04-01.pdf"),
            "Timesheet-Budi-Santoso-2026-04-01.pdf"
        );
        assert_eq!(dash_whitespace("a\t b"), "a-b");
    }

    #[test]
    fn redirect_sets_location_and_302() {
        let r = redirect(&axum::http::HeaderMap::new(), "http://fe/timesheet/abc");
        assert_eq!(r.status(), StatusCode::FOUND);
        assert_eq!(
            r.headers()
                .get(header::LOCATION)
                .and_then(|v| v.to_str().ok()),
            Some("http://fe/timesheet/abc")
        );
    }

    #[test]
    fn ymd_rejects_datetimes() {
        assert!(ymd_or_500("2026-04-01").is_ok());
        assert!(ymd_or_500("2026-04-01T10:00:00Z").is_err());
    }
}
