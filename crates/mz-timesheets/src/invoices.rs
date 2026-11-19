//! Port `src/timesheet-invoices/timesheet-invoices.{controller,service}.ts` (5 route).

use axum::Json;
use axum::extract::State;
use axum::response::Response;
use chrono::{Datelike, NaiveDate, NaiveDateTime};
use http::StatusCode;
use mz_core::company::find_company_id;
use mz_core::db::advisory_xact_lock;
use mz_core::extract::{PathParams, ValidatedJson, ValidatedQuery};
use mz_core::time::now_naive;
use mz_core::{ApiError, ApiResult, AppState, RequestCtx};
use serde_json::{Value, json};
use sqlx::{Postgres, QueryBuilder};

use crate::calc::{format_date_gb, money2};
use crate::common::{can_act_on_company, date_json, fnum, jd, jd_opt, parse_uuid};
use crate::dto::{InvoiceQuery, InvoiceStatusBody};
use crate::jsfmt;
use crate::mail::{InvoicePaid, send_invoice_paid};
use crate::pdf_reports::{InvoicePdfData, invoice_pdf};
use crate::timesheets::{fetch_by_uuid, file_response};

const INV_SELECT: &str = r#"SELECT i.id, i.uuid::text AS uuid, i."invoiceNumber", i.status::text AS status,
    i."totalSeconds", i."totalHours", i."payRate", i."rateType"::text AS "rateType",
    i."totalAmount", i.currency, i."issuedAt", i."paidAt", i.notes, i."createdAt",
    u.name AS "uName", u.username AS "uUsername", c.name AS "cName",
    ts."periodStart", ts."periodEnd", ts."payPeriodType"::text AS "payPeriodType"
    FROM "timesheetInvoice" i
    JOIN timesheet ts ON ts.id = i."timesheetId"
    LEFT JOIN users u ON u.id = i."userId"
    LEFT JOIN company c ON c.id = i."companyId""#;

#[derive(Debug, Clone, sqlx::FromRow)]
struct Inv {
    uuid: String,
    #[sqlx(rename = "invoiceNumber")]
    invoice_number: String,
    status: String,
    #[sqlx(rename = "totalSeconds")]
    total_seconds: i32,
    #[sqlx(rename = "totalHours")]
    total_hours: f64,
    #[sqlx(rename = "payRate")]
    pay_rate: Option<f64>,
    #[sqlx(rename = "rateType")]
    rate_type: String,
    #[sqlx(rename = "totalAmount")]
    total_amount: f64,
    currency: String,
    #[sqlx(rename = "issuedAt")]
    issued_at: Option<NaiveDateTime>,
    #[sqlx(rename = "paidAt")]
    paid_at: Option<NaiveDateTime>,
    notes: Option<String>,
    #[sqlx(rename = "createdAt")]
    created_at: NaiveDateTime,
    #[sqlx(rename = "uName")]
    u_name: Option<String>,
    #[sqlx(rename = "uUsername")]
    u_username: Option<String>,
    #[sqlx(rename = "cName")]
    c_name: Option<String>,
    #[sqlx(rename = "periodStart")]
    period_start: NaiveDate,
    #[sqlx(rename = "periodEnd")]
    period_end: NaiveDate,
    #[sqlx(rename = "payPeriodType")]
    pay_period_type: String,
}

impl Inv {
    /// `buildPdfData`.
    fn pdf_data(&self) -> InvoicePdfData {
        InvoicePdfData {
            invoice_number: self.invoice_number.clone(),
            status: self.status.clone(),
            issued_at: self.issued_at.map(|d| format_date_gb(d.date())),
            company_name: self.c_name.clone().unwrap_or_else(|| "Company".into()),
            employee_name: self.u_name.clone().unwrap_or_else(|| "-".into()),
            employee_email: self.u_username.clone().unwrap_or_else(|| "-".into()),
            period_start: format_date_gb(self.period_start),
            period_end: format_date_gb(self.period_end),
            pay_period_type: self.pay_period_type.clone(),
            total_hours: self.total_hours,
            pay_rate: self.pay_rate,
            rate_type: self.rate_type.clone(),
            total_amount: self.total_amount,
            currency: self.currency.clone(),
            notes: self.notes.clone(),
        }
    }
}

/// `getDefaultCompany` + `assertPrivileged`.
async fn privileged_company(state: &AppState, user_id: i32) -> ApiResult<i32> {
    let Some(company_id) = find_company_id(&state.db, user_id).await? else {
        return Err(ApiError::forbidden(
            "User tidak tergabung dalam company manapun",
        ));
    };
    if !can_act_on_company(&state.db, user_id, company_id).await? {
        return Err(ApiError::forbidden("Akses ditolak"));
    }
    Ok(company_id)
}

async fn find_invoice(state: &AppState, uuid: &str, company_id: i32) -> ApiResult<Inv> {
    sqlx::query_as::<_, Inv>(&format!(
        r#"{INV_SELECT} WHERE i.uuid = $1::uuid AND i."companyId" = $2 LIMIT 1"#
    ))
    .bind(uuid)
    .bind(company_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| ApiError::not_found("Invoice tidak ditemukan"))
}

/// `INV-TS-YYYYMM-XXXX`.
pub fn invoice_number(now: NaiveDateTime, count: i64) -> String {
    format!("INV-TS-{}{:02}-{:04}", now.year(), now.month(), count + 1)
}

/// Awal bulan ini dan bulan depan (`createdAt >= start AND < next`).
pub fn month_range(now: NaiveDateTime) -> Option<(NaiveDateTime, NaiveDateTime)> {
    let start = NaiveDate::from_ymd_opt(now.year(), now.month(), 1)?.and_hms_opt(0, 0, 0)?;
    let next = if now.month() == 12 {
        NaiveDate::from_ymd_opt(now.year() + 1, 1, 1)
    } else {
        NaiveDate::from_ymd_opt(now.year(), now.month() + 1, 1)
    }?
    .and_hms_opt(0, 0, 0)?;
    Some((start, next))
}

/// Node: src/timesheet-invoices/timesheet-invoices.controller.ts:55 (`generate`, `@HttpCode(201)`)
pub async fn generate(
    State(state): State<AppState>,
    ctx: RequestCtx,
    PathParams(timesheet_uuid): PathParams<String>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let timesheet_uuid = parse_uuid(timesheet_uuid)?;
    let admin_id = ctx.require_user_id()?;
    let company_id = privileged_company(&state, admin_id).await?;

    let Some(ts) = fetch_by_uuid(&state, &timesheet_uuid, Some(company_id)).await? else {
        return Err(ApiError::not_found("Timesheet tidak ditemukan"));
    };
    if ts.status != "APPROVED" {
        return Err(ApiError::bad_request(
            "Invoice hanya bisa dibuat untuk timesheet yang sudah diapprove",
        ));
    }

    let existing: Option<(String, String, String)> = sqlx::query_as(
        r#"SELECT uuid::text, "invoiceNumber", status::text FROM "timesheetInvoice" WHERE "timesheetId" = $1"#,
    )
    .bind(ts.id)
    .fetch_optional(&state.db)
    .await?;
    if let Some((uuid, number, status)) = existing {
        return Ok((
            StatusCode::CREATED,
            Json(json!({
                "uuid": uuid, "invoiceNumber": number, "status": status,
                "message": "Invoice sudah pernah dibuat sebelumnya",
            })),
        ));
    }

    let total_hours = crate::calc::hours(i64::from(ts.total_seconds), 4);
    let now = now_naive();
    let (start, next) =
        month_range(now).ok_or_else(|| ApiError::internal(anyhow::anyhow!("bad month")))?;

    // Penomoran count+1 per company per bulan: kunci advisory mencegah dua invoice
    // bersamaan mendapat nomor yang sama (Node: bentrok unique → 409).
    let mut tx = state.db.begin().await?;
    advisory_xact_lock(&mut tx, &format!("tsinv:{company_id}")).await?;
    let count: i64 = sqlx::query_scalar(
        r#"SELECT COUNT(*) FROM "timesheetInvoice" WHERE "companyId" = $1 AND "createdAt" >= $2 AND "createdAt" < $3"#,
    )
    .bind(company_id)
    .bind(start)
    .bind(next)
    .fetch_one(&mut *tx)
    .await?;
    let number = invoice_number(now, count);
    let row: (String, String, String) = sqlx::query_as(
        r#"INSERT INTO "timesheetInvoice" ("timesheetId", "companyId", "userId", "invoiceNumber", status,
               "totalSeconds", "totalHours", "payRate", "rateType", "totalAmount", currency, notes, "issuedAt")
           VALUES ($1, $2, $3, $4, 'ISSUED'::"TimesheetInvoiceStatus", $5, $6, $7, $8::"RateType", $9, $10, $11, $12)
           RETURNING uuid::text, "invoiceNumber", status::text"#,
    )
    .bind(ts.id)
    .bind(company_id)
    .bind(ts.user_id)
    .bind(&number)
    .bind(ts.total_seconds)
    .bind(total_hours)
    .bind(ts.pay_rate)
    .bind(&ts.rate_type)
    .bind(ts.total_amount.unwrap_or(0.0))
    .bind(&ts.currency)
    .bind(&ts.notes)
    .bind(now)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "uuid": row.0, "invoiceNumber": row.1, "status": row.2, "message": "Invoice berhasil dibuat",
        })),
    ))
}

/// Node: src/timesheet-invoices/timesheet-invoices.controller.ts:70 (`findAll`)
pub async fn find_all(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedQuery(query): ValidatedQuery<InvoiceQuery>,
) -> ApiResult<Json<Value>> {
    let admin_id = ctx.require_user_id()?;
    let company_id = privileged_company(&state, admin_id).await?;
    let page = query.page_value();
    let limit = query.limit_value();
    let skip = (page - 1).saturating_mul(limit);

    let mut user_id: Option<i32> = None;
    if let Some(u) = query.user_uuid.as_deref().filter(|u| !u.is_empty()) {
        let found: Option<i32> = sqlx::query_scalar("SELECT id FROM users WHERE uuid = $1::uuid")
            .bind(u)
            .fetch_optional(&state.db)
            .await?;
        user_id = Some(found.ok_or_else(|| ApiError::not_found("Employee tidak ditemukan"))?);
    }

    let mut qb = QueryBuilder::<Postgres>::new(
        r#"SELECT i.uuid::text AS uuid, i."invoiceNumber", i.status::text AS status, u.name AS "uName",
                  ts."periodStart", ts."periodEnd", i."totalAmount", i.currency, i."createdAt"
           FROM "timesheetInvoice" i
           JOIN timesheet ts ON ts.id = i."timesheetId"
           LEFT JOIN users u ON u.id = i."userId"
           WHERE i."companyId" = "#,
    );
    qb.push_bind(company_id);
    if let Some(s) = query.status.as_deref().filter(|s| !s.is_empty()) {
        qb.push(" AND i.status::text = ").push_bind(s.to_owned());
    }
    if let Some(u) = user_id {
        qb.push(r#" AND i."userId" = "#).push_bind(u);
    }
    let mut cqb = QueryBuilder::<Postgres>::new(
        r#"SELECT COUNT(*) FROM "timesheetInvoice" i WHERE i."companyId" = "#,
    );
    cqb.push_bind(company_id);
    if let Some(s) = query.status.as_deref().filter(|s| !s.is_empty()) {
        cqb.push(" AND i.status::text = ").push_bind(s.to_owned());
    }
    if let Some(u) = user_id {
        cqb.push(r#" AND i."userId" = "#).push_bind(u);
    }
    qb.push(r#" ORDER BY i."createdAt" DESC, i.id DESC LIMIT "#)
        .push_bind(limit);
    qb.push(" OFFSET ").push_bind(skip);

    type ListRow = (
        String,
        String,
        String,
        Option<String>,
        NaiveDate,
        NaiveDate,
        f64,
        String,
        NaiveDateTime,
    );
    let rows: Vec<ListRow> = qb.build_query_as().fetch_all(&state.db).await?;
    let total: i64 = cqb.build_query_scalar().fetch_one(&state.db).await?;
    let data: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "uuid": r.0, "invoiceNumber": r.1, "status": r.2,
                "employeeName": r.3.unwrap_or_else(|| "-".into()),
                "periodStart": date_json(r.4), "periodEnd": date_json(r.5),
                "totalAmount": jsfmt::num(r.6), "currency": r.7, "createdAt": jd(r.8),
            })
        })
        .collect();
    Ok(Json(
        json!({ "data": data, "total": total, "page": page, "limit": limit }),
    ))
}

/// Node: src/timesheet-invoices/timesheet-invoices.controller.ts:82 (`findOne`)
pub async fn find_one(
    State(state): State<AppState>,
    ctx: RequestCtx,
    PathParams(uuid): PathParams<String>,
) -> ApiResult<Json<Value>> {
    let uuid = parse_uuid(uuid)?;
    let admin_id = ctx.require_user_id()?;
    let company_id = privileged_company(&state, admin_id).await?;
    let inv = find_invoice(&state, &uuid, company_id).await?;
    Ok(Json(json!({
        "uuid": inv.uuid,
        "invoiceNumber": inv.invoice_number,
        "status": inv.status,
        "employeeName": inv.u_name.clone().unwrap_or_else(|| "-".into()),
        "employeeEmail": inv.u_username.clone().unwrap_or_else(|| "-".into()),
        "periodStart": date_json(inv.period_start),
        "periodEnd": date_json(inv.period_end),
        "payPeriodType": inv.pay_period_type,
        "totalSeconds": inv.total_seconds,
        "totalHours": jsfmt::num(inv.total_hours),
        "payRate": fnum(inv.pay_rate),
        "rateType": inv.rate_type,
        "totalAmount": jsfmt::num(inv.total_amount),
        "currency": inv.currency,
        "issuedAt": jd_opt(inv.issued_at),
        "paidAt": jd_opt(inv.paid_at),
        "notes": inv.notes,
        "createdAt": jd(inv.created_at),
    })))
}

/// Node: src/timesheet-invoices/timesheet-invoices.controller.ts:95 (`downloadPdf`)
pub async fn download_pdf(
    State(state): State<AppState>,
    ctx: RequestCtx,
    PathParams(uuid): PathParams<String>,
) -> ApiResult<Response> {
    let uuid = parse_uuid(uuid)?;
    let admin_id = ctx.require_user_id()?;
    let company_id = privileged_company(&state, admin_id).await?;
    let inv = find_invoice(&state, &uuid, company_id).await?;
    let bytes = invoice_pdf(&inv.pdf_data());
    // Node mem-pipe PDFKit ke response (chunked, tanpa Content-Length).
    file_response(
        bytes,
        "application/pdf",
        &format!("{}.pdf", inv.invoice_number),
        false,
    )
}

/// Node: src/timesheet-invoices/timesheet-invoices.controller.ts:111 (`updateStatus`)
pub async fn update_status(
    State(state): State<AppState>,
    ctx: RequestCtx,
    PathParams(uuid): PathParams<String>,
    ValidatedJson(dto): ValidatedJson<InvoiceStatusBody>,
) -> ApiResult<Json<Value>> {
    let uuid = parse_uuid(uuid)?;
    let admin_id = ctx.require_user_id()?;
    let company_id = privileged_company(&state, admin_id).await?;
    let inv = find_invoice(&state, &uuid, company_id).await?;
    if inv.status == "PAID" {
        return Err(ApiError::bad_request(
            "Invoice yang sudah PAID tidak bisa diubah",
        ));
    }
    if inv.status == "CANCELLED" {
        return Err(ApiError::bad_request(
            "Invoice yang sudah CANCELLED tidak bisa diubah",
        ));
    }

    let new_status = dto.status_str().to_owned();
    let paid_at = (new_status == "PAID").then(now_naive);
    let updated: (String, String, String, Option<NaiveDateTime>) = sqlx::query_as(
        r#"UPDATE "timesheetInvoice"
           SET status = $2::"TimesheetInvoiceStatus", "paidAt" = COALESCE($3, "paidAt")
           WHERE uuid = $1::uuid
           RETURNING uuid::text, "invoiceNumber", status::text, "paidAt""#,
    )
    .bind(&uuid)
    .bind(&new_status)
    .bind(paid_at)
    .fetch_one(&state.db)
    .await?;

    if let (Some(paid), Some(email)) =
        (paid_at, inv.u_username.as_deref().filter(|e| !e.is_empty()))
    {
        // Data PDF memakai status terbaru (PAID).
        let mut fresh = find_invoice(&state, &uuid, company_id).await?;
        fresh.status = updated.2.clone();
        let pdf = invoice_pdf(&fresh.pdf_data());
        let amount = if fresh.total_amount == 0.0 {
            "0.00".to_owned()
        } else {
            money2(fresh.total_amount)
        };
        let _ = send_invoice_paid(
            &state,
            InvoicePaid {
                employee_email: email.to_owned(),
                employee_name: fresh.u_name.clone().unwrap_or_else(|| "-".into()),
                invoice_number: fresh.invoice_number.clone(),
                company_name: fresh.c_name.clone().unwrap_or_else(|| "-".into()),
                period_start: format_date_gb(fresh.period_start),
                period_end: format_date_gb(fresh.period_end),
                total_amount: amount,
                currency: fresh.currency.clone(),
                paid_at: format_date_gb(paid.date()),
                pdf,
            },
        )
        .await;
    }

    Ok(Json(json!({
        "uuid": updated.0, "invoiceNumber": updated.1, "status": updated.2, "paidAt": jd_opt(updated.3),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dt(y: i32, m: u32, d: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, m, d)
            .and_then(|x| x.and_hms_opt(10, 0, 0))
            .unwrap_or_default()
    }

    #[test]
    fn numbering_is_per_month_sequence() {
        assert_eq!(invoice_number(dt(2026, 4, 15), 0), "INV-TS-202604-0001");
        assert_eq!(invoice_number(dt(2026, 12, 1), 41), "INV-TS-202612-0042");
        assert_eq!(invoice_number(dt(2026, 1, 1), 9999), "INV-TS-202601-10000");
    }

    #[test]
    fn month_range_wraps_year() {
        let (s, n) = month_range(dt(2026, 12, 20)).unwrap_or_default();
        assert_eq!(s.date().to_string(), "2026-12-01");
        assert_eq!(n.date().to_string(), "2027-01-01");
        let (s, n) = month_range(dt(2026, 2, 3)).unwrap_or_default();
        assert_eq!(s.date().to_string(), "2026-02-01");
        assert_eq!(n.date().to_string(), "2026-03-01");
    }
}
