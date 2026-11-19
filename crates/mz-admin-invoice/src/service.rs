//! Port `admin-invoice.controller.ts` + `admin-invoice.service.ts` + `AdminInvoiceQueryDto`
//! + `ManualSubscriptionService.voidInvoice` (`admin-company/manual-subscription`).
//!
//! Baris `invoices`/`subscriptions`/`company` dikirim utuh seperti hasil Prisma (semua kolom
//! skalar, urutan field `schema.prisma`, `DateTime` → ISO, list NULL → `[]`). Kolomnya dibaca
//! lewat DMMF tertanam di `mz-admin-jobs` (pembaca baris generik backup), jadi tidak ada daftar
//! kolom yang ditulis tangan.

use std::collections::HashMap;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use chrono::{Datelike, Duration, NaiveDateTime};
use mz_admin_jobs::company_data::pg::{Catalog, q};
use mz_admin_jobs::company_data::scope::{ModelGraph, Row, embedded_graph};
use mz_admin_jobs::validate::{
    Errors, RawBody, RawPath, RawQuery, optional_reason, present, qval_json, reason, trimmed,
};
use mz_core::qs::QVal;
use mz_core::time::JsDate;
use mz_core::{ApiError, ApiResult, AppState};
use mz_operator::{AuditCtx, Operator, OperatorAuditEntry, OperatorRole, record};
use serde_json::{Value, json};
use tokio::sync::OnceCell;

/// `RESEND_THROTTLE_SECONDS`.
pub const RESEND_THROTTLE_SECONDS: i64 = 5 * 60;
const STATUSES: &[&str] = &["UNPAID", "PAID", "CANCELLED", "EXPIRED"];
const PROVIDERS: &[&str] = &["paypal", "manual", "midtrans"];
const OWNER_FINANCE: &[OperatorRole] = &[OperatorRole::Owner, OperatorRole::Finance];
/// `HIDDEN_COMPANY_FIELDS` (`src/companies/company-response.ts`).
const HIDDEN_COMPANY_FIELDS: &[&str] = &["profilePictureFullPath"];

fn invoice_not_found() -> ApiError {
    ApiError::coded(
        StatusCode::NOT_FOUND,
        "INVOICE_NOT_FOUND",
        "Invoice not found",
    )
}

// ── Pembaca baris Prisma ────────────────────────────────────────────────────

static META: OnceCell<(ModelGraph, Catalog)> = OnceCell::const_new();

async fn meta(state: &AppState) -> anyhow::Result<&'static (ModelGraph, Catalog)> {
    META.get_or_try_init(|| async {
        let models = embedded_graph()?;
        let mut conn = state.db.acquire().await?;
        let catalog = Catalog::load(&mut conn).await?;
        let names: Vec<String> = ["invoices", "subscriptions", "company"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        catalog.verify(&models, names.iter())?;
        anyhow::Ok((models, catalog))
    })
    .await
}

/// Daftar kolom SELECT seluruh field skalar `model` (alias `t`).
fn select_all(meta: &(ModelGraph, Catalog), model: &str) -> anyhow::Result<String> {
    let (models, catalog) = meta;
    let info = models
        .get(model)
        .ok_or_else(|| anyhow::anyhow!("model {model} tidak ada di DMMF"))?;
    let fields: Vec<&str> = info.scalar_fields.iter().map(String::as_str).collect();
    catalog.select_list(info, &fields, "t")
}

fn parse_rows(raw: Vec<String>) -> anyhow::Result<Vec<Row>> {
    raw.iter()
        .map(|r| serde_json::from_str::<Row>(r).map_err(anyhow::Error::from))
        .collect()
}

/// `findMany({ where: { id: { in: ids } } })` → peta id → baris.
async fn rows_by_id(
    state: &AppState,
    meta: &(ModelGraph, Catalog),
    model: &str,
    ids: &[i64],
) -> anyhow::Result<HashMap<i64, Row>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let select = select_all(meta, model)?;
    let sql = format!(
        "SELECT row_to_json(x)::text FROM (SELECT {select} FROM {} t WHERE t.id = ANY($1)) x",
        q(model)
    );
    let ids32: Vec<i32> = ids.iter().filter_map(|i| i32::try_from(*i).ok()).collect();
    let raw: Vec<String> = sqlx::query_scalar(&sql)
        .bind(ids32)
        .persistent(false)
        .fetch_all(&state.db)
        .await?;
    Ok(parse_rows(raw)?
        .into_iter()
        .filter_map(|r| r.get("id").and_then(Value::as_i64).map(|id| (id, r)))
        .collect())
}

// ── Query ───────────────────────────────────────────────────────────────────

/// `AdminInvoiceQueryDto` sesudah validasi.
#[derive(Debug, Clone, Default)]
pub struct ListQuery {
    pub status: Option<String>,
    pub provider: Option<String>,
    pub company_uuid: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub search: Option<String>,
    pub page_number: i64,
    pub items_per_page: i64,
}

/// `Date.parse(s)` V8 (ms epoch; `None` = NaN → Node meneruskan `Invalid Date` ke Prisma → 500).
pub fn js_date_parse(s: &str) -> Option<i64> {
    mz_core::time::v8_date_parse(s)
}

fn is_date_only(s: &str) -> bool {
    mz_admin_jobs::validate::is_date_only(s)
}

/// `AdminInvoiceQueryDto` di bawah `ADMIN_VALIDATION_PIPE`.
pub fn validate_list(qv: &[(String, QVal)]) -> Result<ListQuery, ApiError> {
    let mut e = Errors::default();
    e.unknown_query(
        qv,
        &[
            "status",
            "provider",
            "companyUuid",
            "from",
            "to",
            "search",
            "pageNumber",
            "itemsPerPage",
        ],
    );
    let get = |k: &str| mz_core::qs::get(qv, k).map(qval_json);
    if let Some(v) = get("status") {
        e.one_of("status", Some(&v), STATUSES);
    }
    if let Some(v) = get("provider") {
        e.one_of("provider", Some(&v), PROVIDERS);
    }
    if let Some(v) = get("companyUuid") {
        e.uuid("companyUuid", Some(&v));
    }
    let from = get("from");
    let to = get("to");
    if let Some(v) = &from {
        // `@Validate(InvoiceFromBeforeToConstraint)` (dekorator paling bawah) lalu `@IsDateString`.
        if let (Some(f), Some(t)) = (v.as_str(), to.as_ref().and_then(Value::as_str))
            && !f.is_empty()
            && !t.is_empty()
            && let (Some(fm), Some(tm)) = (js_date_parse(f), js_date_parse(t))
            && fm > tm
        {
            e.push("from must not be later than to");
        }
        if !v.as_str().is_some_and(mz_core::iso8601::is_iso8601) {
            e.push("from must be a valid ISO 8601 date string");
        }
    }
    if let Some(v) = &to
        && !v.as_str().is_some_and(mz_core::iso8601::is_iso8601)
    {
        e.push("to must be a valid ISO 8601 date string");
    }
    let search = trimmed(get("search").as_ref());
    if present(search.as_ref()) {
        e.max_length("search", search.as_ref(), 50);
        e.string("search", search.as_ref());
    }
    let page = match mz_core::qs::get(qv, "pageNumber") {
        Some(v) => min_int(&mut e, "pageNumber", v, 1),
        None => Some(1),
    };
    let per = match mz_core::qs::get(qv, "itemsPerPage") {
        Some(v) => e.query_int_range("itemsPerPage", Some(v), 1, 100),
        None => Some(25),
    };
    e.into_result()?;
    let s = |v: Option<Value>| v.and_then(|v| v.as_str().map(str::to_owned));
    Ok(ListQuery {
        status: s(get("status")),
        provider: s(get("provider")),
        company_uuid: s(get("companyUuid")),
        from: s(from),
        to: s(to),
        search: s(search),
        page_number: page.unwrap_or(1),
        items_per_page: per.unwrap_or(25),
    })
}

/// `@Type(() => Number) @IsInt() @Min(min)`: pesan Min lalu IsInt.
fn min_int(e: &mut Errors, field: &str, v: &QVal, min: i64) -> Option<i64> {
    let num = match v {
        QVal::One(s) => mz_core::jsfmt::number(s),
        _ => f64::NAN,
    };
    let before = e.0.len();
    #[allow(clippy::cast_precision_loss)]
    if num.is_nan() || num < min as f64 {
        e.push(format!("{field} must not be less than {min}"));
    }
    if !(num.is_finite() && num.fract() == 0.0) {
        e.push(format!("{field} must be an integer number"));
    }
    #[allow(clippy::cast_possible_truncation)]
    (e.0.len() == before).then_some(num as i64)
}

/// `toCompanyResponse(row)`.
fn to_company_response(mut row: Row) -> Row {
    for f in HIDDEN_COMPANY_FIELDS {
        row.shift_remove(*f);
    }
    row
}

/// Bentuk satu baris `findAll` (`{...invoice, recordedBy, subscription: {...sub, company}}`).
fn shape_invoice(
    mut inv: Row,
    subs: &HashMap<i64, Row>,
    companies: &HashMap<i64, Row>,
    operators: &HashMap<i32, (uuid::Uuid, String)>,
) -> Value {
    let op_id = inv
        .shift_remove("recordedByOperatorId")
        .and_then(|v| v.as_i64())
        .and_then(|v| i32::try_from(v).ok());
    let sub = inv
        .get("subscriptionId")
        .and_then(Value::as_i64)
        .and_then(|id| subs.get(&id))
        .cloned();
    let subscription = match sub {
        None => Value::Null,
        Some(mut s) => {
            let company = s
                .get("companyId")
                .and_then(Value::as_i64)
                .and_then(|id| companies.get(&id))
                .cloned()
                .map_or(Value::Null, |c| Value::Object(to_company_response(c)));
            s.insert("company".into(), company);
            Value::Object(s)
        }
    };
    inv.insert("subscription".into(), subscription);
    let recorded = op_id.and_then(|id| operators.get(&id)).map_or(
        Value::Null,
        |(u, n)| json!({ "uuid": u.to_string(), "name": n }),
    );
    inv.insert("recordedBy".into(), recorded);
    Value::Object(inv)
}

/// `findAll(query)`.
pub async fn find_all(state: &AppState, query: &ListQuery) -> ApiResult<Value> {
    let m = meta(state).await.map_err(ApiError::internal)?;
    let from = match &query.from {
        Some(f) => Some(
            js_date_parse(f)
                .ok_or_else(|| ApiError::internal(anyhow::anyhow!("Invalid Date: from")))?,
        ),
        None => None,
    };
    let mut to = match &query.to {
        Some(t) => Some(
            js_date_parse(t)
                .ok_or_else(|| ApiError::internal(anyhow::anyhow!("Invalid Date: to")))?,
        ),
        None => None,
    };
    if let (Some(f), Some(t)) = (from, to)
        && f > t
    {
        return Err(ApiError::coded(
            StatusCode::BAD_REQUEST,
            "INVALID_DATE_RANGE",
            "from must not be later than to",
        ));
    }
    let to_date_only = query.to.as_deref().is_some_and(is_date_only);
    if to_date_only {
        to = to.map(|t| t + Duration::days(1).num_milliseconds());
    }
    // Prisma menolak `Date` di luar tahun 0000–9999 (`toISOString` bertanda `+010000-…`) → 500.
    let ts = |ms: i64| -> ApiResult<NaiveDateTime> {
        JsDate::from_millis(ms)
            .map(|d| d.0)
            .filter(|d| (0..=9999).contains(&d.year()))
            .ok_or_else(|| ApiError::internal(anyhow::anyhow!("date out of Prisma range: {ms}")))
    };
    let from_ts: Option<NaiveDateTime> = from.map(ts).transpose()?;
    let to_ts: Option<NaiveDateTime> = to.map(ts).transpose()?;
    // Node: `skip: (pageNumber - 1) * itemsPerPage` → angka di luar Int64 ditolak Prisma (500).
    let offset = query
        .page_number
        .checked_sub(1)
        .and_then(|p| p.checked_mul(query.items_per_page))
        .ok_or_else(|| ApiError::internal(anyhow::anyhow!("skip overflow")))?;

    let filters = r#"
        FROM invoices t
        LEFT JOIN subscriptions s ON s.id = t."subscriptionId"
        LEFT JOIN company c ON c.id = s."companyId"
       WHERE ($1::text IS NULL OR t."invoiceNumber" ILIKE '%' || $1 || '%')
         AND ($2::text IS NULL OR t.status::text = $2)
         AND ($3::text IS NULL OR t."paymentProvider" = $3)
         AND ($4::uuid IS NULL OR c.uuid = $4::uuid)
         AND ($5::timestamp IS NULL OR t."createdAt" >= $5)
         AND ($6::timestamp IS NULL OR ($7 AND t."createdAt" < $6) OR (NOT $7 AND t."createdAt" <= $6))"#;
    let select = select_all(m, "invoices").map_err(ApiError::internal)?;
    let raw: Vec<String> = sqlx::query_scalar(&format!(
        r#"SELECT row_to_json(x)::text FROM (SELECT {select} {filters}
            ORDER BY t."createdAt" DESC LIMIT $8 OFFSET $9) x"#
    ))
    .bind(&query.search)
    .bind(&query.status)
    .bind(&query.provider)
    .bind(&query.company_uuid)
    .bind(from_ts)
    .bind(to_ts)
    .bind(to_date_only)
    .bind(query.items_per_page)
    .bind(offset)
    .persistent(false)
    .fetch_all(&state.db)
    .await?;
    let total: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) {filters}"))
        .bind(&query.search)
        .bind(&query.status)
        .bind(&query.provider)
        .bind(&query.company_uuid)
        .bind(from_ts)
        .bind(to_ts)
        .bind(to_date_only)
        .persistent(false)
        .fetch_one(&state.db)
        .await?;
    let invoices = parse_rows(raw).map_err(ApiError::internal)?;

    let sub_ids: Vec<i64> = invoices
        .iter()
        .filter_map(|r| r.get("subscriptionId").and_then(Value::as_i64))
        .collect();
    let subs = rows_by_id(state, m, "subscriptions", &sub_ids)
        .await
        .map_err(ApiError::internal)?;
    let company_ids: Vec<i64> = subs
        .values()
        .filter_map(|r| r.get("companyId").and_then(Value::as_i64))
        .collect();
    let companies = rows_by_id(state, m, "company", &company_ids)
        .await
        .map_err(ApiError::internal)?;
    let op_ids: Vec<i32> = invoices
        .iter()
        .filter_map(|r| r.get("recordedByOperatorId").and_then(Value::as_i64))
        .filter_map(|v| i32::try_from(v).ok())
        .collect();
    let operators: HashMap<i32, (uuid::Uuid, String)> = if op_ids.is_empty() {
        HashMap::new()
    } else {
        sqlx::query_as::<_, (i32, uuid::Uuid, String)>(
            "SELECT id, uuid, name FROM operators WHERE id = ANY($1)",
        )
        .bind(&op_ids)
        .fetch_all(&state.db)
        .await?
        .into_iter()
        .map(|(id, u, n)| (id, (u, n)))
        .collect()
    };
    let result: Vec<Value> = invoices
        .into_iter()
        .map(|inv| shape_invoice(inv, &subs, &companies, &operators))
        .collect();
    Ok(json!({
        "success": true,
        "message": "Success get all data invoices",
        "data": {
            "total": total,
            "itemsPerPage": query.items_per_page,
            "pageNumber": query.page_number,
            "result": result,
        }
    }))
}

// ── Handler ─────────────────────────────────────────────────────────────────

/// Node: src/admin-invoice/admin-invoice.controller.ts:55 (`findAll`).
pub async fn h_list(
    State(state): State<AppState>,
    _op: Operator,
    RawQuery(qv): RawQuery,
) -> ApiResult<Json<Value>> {
    let query = validate_list(&qv)?;
    Ok(Json(find_all(&state, &query).await?))
}

/// Node: src/admin-invoice/admin-invoice.controller.ts:64 (`findOne`). Tanpa `ParseUUIDPipe`;
/// error apa pun (termasuk tidak ditemukan / uuid rusak) dilempar sebagai objek biasa di Node
/// → 500 `Internal server error`.
pub async fn h_detail(
    State(state): State<AppState>,
    _op: Operator,
    RawPath(raw): RawPath,
) -> ApiResult<Json<Value>> {
    let failed = || ApiError::internal(anyhow::anyhow!("Failed to get invoices data"));
    let Ok(uuid) = uuid::Uuid::try_parse(&raw) else {
        return Err(failed());
    };
    let m = meta(&state).await.map_err(|_| failed())?;
    let select = select_all(m, "invoices").map_err(|_| failed())?;
    let raw_row: Option<String> = sqlx::query_scalar(&format!(
        "SELECT row_to_json(x)::text FROM (SELECT {select} FROM invoices t WHERE t.uuid = $1 LIMIT 1) x"
    ))
    .bind(uuid)
    .persistent(false)
    .fetch_optional(&state.db)
    .await
    .map_err(|_| failed())?;
    let Some(raw_row) = raw_row else {
        return Err(failed());
    };
    let data: Value = serde_json::from_str(&raw_row).map_err(|_| failed())?;
    Ok(Json(
        json!({ "success": true, "message": "Success", "data": data }),
    ))
}

/// `ReasonDto` / `OptionalReasonDto` di bawah `ADMIN_VALIDATION_PIPE`.
pub fn validate_reason(body: &Value, required: bool) -> Result<Option<String>, ApiError> {
    let mut e = Errors::default();
    e.unknown_body(body, &["reason"]);
    let r = if required {
        reason(&mut e, body)
    } else {
        optional_reason(&mut e, body)
    };
    e.into_result()?;
    Ok(r)
}

/// Node: src/admin-invoice/admin-invoice.controller.ts:78 (`voidInvoice`) →
/// `ManualSubscriptionService.voidInvoice` (`manual-subscription.service.ts:799`).
pub async fn h_void(
    State(state): State<AppState>,
    op: Operator,
    ctx: AuditCtx,
    RawPath(raw): RawPath,
    RawBody(body): RawBody,
) -> ApiResult<(StatusCode, Json<Value>)> {
    op.require(OWNER_FINANCE)?;
    // Node: galat body (pipe global) muncul sebelum `ParseUUIDPipe` (diverifikasi paritas).
    let reason = validate_reason(&body, true)?;
    let uuid = mz_core::uuid_rules::parse_uuid_pipe(&raw)?;

    let found: Option<(i32, i32)> = sqlx::query_as(
        r#"SELECT i.id, s."companyId" FROM invoices i JOIN subscriptions s ON s.id = i."subscriptionId"
            WHERE i.uuid = $1"#,
    )
    .bind(uuid)
    .fetch_optional(&state.db)
    .await?;
    let Some((_, company_id)) = found else {
        return Err(invoice_not_found());
    };

    let mut tx = state.db.begin().await?;
    sqlx::query(r#"SELECT id FROM "company" WHERE id = $1 FOR UPDATE"#)
        .bind(company_id)
        .execute(&mut *tx)
        .await?;
    let before: Option<(uuid::Uuid, String, String, f64, String, Option<String>)> = sqlx::query_as(
        r#"SELECT uuid, status::text, "paymentProvider", "totalPrice", currency, "paymentReference"
             FROM invoices WHERE uuid = $1"#,
    )
    .bind(uuid)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(before) = before else {
        return Err(invoice_not_found());
    };
    if before.2 != "manual" {
        return Err(ApiError::coded(
            StatusCode::CONFLICT,
            "NOT_MANUAL_INVOICE",
            "Only manual invoices can be voided.",
        ));
    }
    let not_paid = || {
        ApiError::coded(
            StatusCode::CONFLICT,
            "NOT_PAID",
            "Only PAID or UNPAID invoices can be voided.",
        )
    };
    if before.1 != "PAID" && before.1 != "UNPAID" {
        return Err(not_paid());
    }
    let updated = sqlx::query(
        r#"UPDATE invoices SET status = 'CANCELLED'
            WHERE uuid = $1 AND "paymentProvider" = 'manual' AND status IN ('PAID', 'UNPAID')"#,
    )
    .bind(uuid)
    .execute(&mut *tx)
    .await?;
    if updated.rows_affected() != 1 {
        return Err(not_paid());
    }
    let before_json = json!({
        "uuid": before.0.to_string(),
        "status": before.1,
        "paymentProvider": before.2,
        "totalPrice": mz_core::jsfmt::num(before.3),
        "currency": before.4,
        "paymentReference": before.5,
    });
    record(
        &mut *tx,
        &ctx,
        &OperatorAuditEntry {
            operator_id: op.id,
            action: "invoice.void",
            target_type: "invoice",
            target_uuid: Some(uuid),
            company_id: Some(company_id),
            before: Some(&before_json),
            after: Some(&json!({ "status": "CANCELLED" })),
            reason: reason.as_deref(),
        },
    )
    .await?;
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "success": true,
            "message": "OK",
            "data": { "uuid": uuid.to_string(), "status": "CANCELLED" },
        })),
    ))
}

/// `formatDmy(d)`: `DD-MM-YYYY` (UTC).
pub fn format_dmy(d: NaiveDateTime) -> String {
    d.format("%d-%m-%Y").to_string()
}

/// Konteks template `invoice.hbs` (`sendInvoiceConfirmation(payload)`).
#[allow(clippy::too_many_arguments)]
pub fn invoice_mail_context(
    email: &str,
    name: &str,
    tier_name: &str,
    invoice_number: &str,
    payment_url: &str,
    start: NaiveDateTime,
    end: NaiveDateTime,
    total_price: f64,
    expired_at: NaiveDateTime,
) -> Value {
    json!({
        "email": email,
        "name": name,
        "tierName": tier_name,
        "invoiceNumber": invoice_number,
        "paymentUrl": payment_url,
        "period": format!("{} - {}", format_dmy(start), format_dmy(end)),
        "totalPrice": mz_core::jsfmt::num(total_price),
        "expiredAt": format_dmy(expired_at),
    })
}

type ResendRow = (
    i32,
    String,
    String,
    f64,
    NaiveDateTime,
    i32,
    NaiveDateTime,
    NaiveDateTime,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    bool,
);

/// Node: src/admin-invoice/admin-invoice.controller.ts:99 (`resendEmail`) → `resendEmail`.
pub async fn h_resend_email(
    State(state): State<AppState>,
    op: Operator,
    ctx: AuditCtx,
    RawPath(raw): RawPath,
    RawBody(body): RawBody,
) -> ApiResult<Json<Value>> {
    op.require(OWNER_FINANCE)?;
    // Node: galat body (pipe global) muncul sebelum `ParseUUIDPipe` (diverifikasi paritas).
    let reason = validate_reason(&body, false)?;
    let uuid = mz_core::uuid_rules::parse_uuid_pipe(&raw)?;

    let row: Option<ResendRow> = sqlx::query_as(
        r#"SELECT i.id, i."invoiceNumber", i."paymentUrl", i."totalPrice", i."expiredAt",
                  s."companyId", s."startDate", s."endDate", pt.name,
                  ou.username, ou.name, du.username, du.name, (ou.id IS NOT NULL)
             FROM invoices i
             JOIN subscriptions s ON s.id = i."subscriptionId"
             LEFT JOIN "pricingTiers" pt ON pt.id = s."pricingTierId"
             LEFT JOIN company c ON c.id = s."companyId"
             LEFT JOIN users ou ON ou.id = c."ownerUserId"
             LEFT JOIN LATERAL (
                 SELECT u.username, u.name FROM "userCompany" uc JOIN users u ON u.id = uc."userId"
                  WHERE uc."companyId" = c.id AND uc."isDefault" = true
                  ORDER BY uc.id LIMIT 1) du ON true
            WHERE i.uuid = $1"#,
    )
    .bind(uuid)
    .fetch_optional(&state.db)
    .await?;
    let Some(r) = row else {
        return Err(invoice_not_found());
    };
    // `company.ownerUser ?? company.userCompany[0]?.user`.
    let (email, name) = if r.13 { (r.9, r.10) } else { (r.11, r.12) };
    let Some(email) = email.filter(|e| !e.is_empty()) else {
        return Err(ApiError::coded(
            StatusCode::BAD_REQUEST,
            "NO_RECIPIENT",
            "Company has no owner or default member to e-mail.",
        ));
    };

    let mut tx = state.db.begin().await?;
    mz_core::db::advisory_xact_lock(&mut tx, &format!("invoice-resend:{}", r.0)).await?;
    let now = mz_core::time::now_naive();
    let last: Option<NaiveDateTime> = sqlx::query_scalar(
        r#"SELECT "createdAt" FROM "operatorAuditLog"
            WHERE action = 'invoice.resend_email' AND "targetUuid" = $1 AND "createdAt" >= $2
            ORDER BY "createdAt" DESC LIMIT 1"#,
    )
    .bind(uuid)
    .bind(now - Duration::seconds(RESEND_THROTTLE_SECONDS))
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(last) = last {
        let elapsed = (now - last).num_milliseconds().div_euclid(1000);
        return Err(ApiError::body(
            StatusCode::TOO_MANY_REQUESTS,
            json!({
                "code": "RESEND_TOO_SOON",
                "message": "Invoice e-mail was sent recently. Try again later.",
                "retryAfterSeconds": (RESEND_THROTTLE_SECONDS - elapsed).max(1),
            }),
        ));
    }
    let after = json!({ "invoiceNumber": r.1, "recipient": email });
    record(
        &mut *tx,
        &ctx,
        &OperatorAuditEntry {
            operator_id: op.id,
            action: "invoice.resend_email",
            target_type: "invoice",
            target_uuid: Some(uuid),
            company_id: Some(r.5),
            before: None,
            after: Some(&after),
            reason: reason.as_deref(),
        },
    )
    .await?;
    tx.commit().await?;

    let name = name.unwrap_or_default();
    let subject = format!("Timemoz Subscription Invoice – #{}", r.1);
    let sent = mz_core::mail::send(
        &state,
        mz_core::mail::Mail {
            to_email: &email,
            to_name: &name,
            subject: &subject,
            text: None,
            template: "invoice",
            context: invoice_mail_context(
                &email,
                &name,
                r.8.as_deref().unwrap_or("-"),
                &r.1,
                &r.2,
                r.6,
                r.7,
                r.3,
                r.4,
            ),
            category: "Invoice",
            label: "send email confirmation invoice",
        },
    )
    .await;
    if !sent.success {
        tracing::error!(invoice = %uuid, "Resend invoice e-mail failed");
        return Err(ApiError::coded(
            StatusCode::BAD_GATEWAY,
            "EMAIL_SEND_FAILED",
            "Failed to send the invoice e-mail.",
        ));
    }
    Ok(Json(json!({
        "success": true,
        "message": "Invoice e-mail sent",
        "data": { "sentTo": email },
    })))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn golden() -> Value {
        serde_json::from_str(include_str!(
            "../../mz-admin-jobs/tests/golden/validation.json"
        ))
        .unwrap()
    }

    fn errs<T>(r: Result<T, ApiError>) -> Option<Vec<String>> {
        match r {
            Ok(_) => None,
            Err(ApiError::Validation(v)) => Some(v),
            Err(o) => Some(vec![format!("OTHER {o}")]),
        }
    }

    fn want(c: &Value) -> Option<Vec<String>> {
        c.get("errors").map(|e| {
            e.as_array()
                .unwrap()
                .iter()
                .map(|s| s.as_str().unwrap().to_owned())
                .collect()
        })
    }

    /// Pesan & urutan = `ValidationPipe` Node (`AdminInvoiceQueryDto`, `ReasonDto`,
    /// `OptionalReasonDto`).
    #[test]
    fn validasi_identik_dengan_node() {
        let g = golden();
        for c in g["invoiceQuery"].as_array().unwrap() {
            let qv = mz_core::qs::parse(c["input"].as_str().unwrap());
            assert_eq!(errs(validate_list(&qv)), want(c), "{}", c["input"]);
            if let (Ok(q), Some(ok)) = (validate_list(&qv), c.get("ok")) {
                assert_eq!(q.search.as_deref(), ok["search"].as_str());
                assert_eq!(q.page_number, ok["pageNumber"].as_i64().unwrap());
            }
        }
        for c in g["reason"].as_array().unwrap() {
            assert_eq!(
                errs(validate_reason(&c["input"], true)),
                want(c),
                "{}",
                c["input"]
            );
        }
        for c in g["optionalReason"].as_array().unwrap() {
            assert_eq!(
                errs(validate_reason(&c["input"], false)),
                want(c),
                "{}",
                c["input"]
            );
        }
    }

    #[test]
    fn date_parse_seperti_js() {
        assert_eq!(js_date_parse("2026-01-01"), Some(1_767_225_600_000));
        assert_eq!(js_date_parse("2026"), js_date_parse("2026-01-01"));
        assert_eq!(js_date_parse("2026-01"), js_date_parse("2026-01-01"));
        assert_eq!(js_date_parse("20260101"), None);
        assert_eq!(
            js_date_parse("2026-03-01T10:00:00Z").unwrap() - js_date_parse("2026-03-01").unwrap(),
            36_000_000
        );
    }

    #[test]
    fn konteks_email_invoice() {
        let d = |s: &str| mz_core::time::parse_js_date(s).unwrap();
        let ctx = invoice_mail_context(
            "o@x.com",
            "Owner",
            "Pro",
            "INV-7",
            "https://pay/7",
            d("2026-11-01T00:00:00Z"),
            d("2026-12-01T00:00:00Z"),
            100.0,
            d("2026-12-01T00:00:00Z"),
        );
        assert_eq!(ctx["period"], "01-11-2026 - 01-12-2026");
        assert_eq!(ctx["expiredAt"], "01-12-2026");
        assert_eq!(ctx["totalPrice"].to_string(), "100");
    }

    #[test]
    fn company_response_menyembunyikan_logo() {
        let row = json!({ "id": 1, "profilePictureFullPath": "x", "name": "n" })
            .as_object()
            .unwrap()
            .clone();
        let out = to_company_response(row);
        assert_eq!(
            out.keys().map(String::as_str).collect::<Vec<_>>(),
            vec!["id", "name"]
        );
    }
}
