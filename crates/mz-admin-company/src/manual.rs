//! `src/admin-company/manual-subscription/*` — langganan MANUAL (7 route): ringkasan,
//! aktivasi, perpanjangan, kontrak harga, invoice UNPAID, pembayaran invoice, stop.
//!
//! Semua tulisan: transaksi diawali `SELECT id FROM company WHERE id = $1 FOR UPDATE`
//! (kunci yang sama dengan plan boost / override / sinkron paket), semua cek diulang di
//! dalam kunci. Cache dibuang SESUDAH commit.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime};
use mz_audit::AuditHandle;
use mz_core::enums::{SubscriptionBillingSource, SubscriptionStatus};
use mz_core::subscription_lock::{self as lock, LockableSubscription};
use mz_core::time::{JsDate, now_naive, v8_date};
use mz_core::{ApiError, ApiResult, AppState};
use mz_operator::{AuditCtx, Operator, OperatorAuditEntry, record};
use mz_paypal::PaypalClient;
use serde_json::{Map, Value, json};
use sqlx::PgConnection;
use sqlx::types::Json as SqlJson;

use crate::common::{
    ALL_ROLES, CompanyChange, OWNER_FINANCE, RawBody, after_company_change, body_value, coded,
    date, date_of, gate, num, ok, uuid_pipe,
};
use crate::plan_limits::{
    Bind, Cols, CurrentSub, SALES_LIMIT_FIELDS, TIME_LIMIT_FIELDS, apply_company_plan_limits,
    apply_plan_boosts, compute_effective_limits, current_sub, current_sub_sql, is_paid_tier, obj,
    unlimited_of, update_company,
};
use crate::val::{V, absent, array_keys, as_obj, num_of, trim_field};

type Op = Result<Operator, ApiError>;

const DAY_MS: i64 = 24 * 60 * 60 * 1000;
pub const MANUAL_PAYMENT_PROVIDER: &str = "manual";
pub const MANUAL_REFERENCE_INDEX: &str = "invoices_manual_reference_key";
const PAYPAL_CANCEL_REASON: &str = "Moved to manual billing";
const SUMMARY_INVOICE_LIMIT: i64 = 10;
const MANUAL_MAX_PERIOD_YEARS: i32 = 3;
const CONTRACT_MAX_PERIOD_MONTHS: f64 = 36.0;
const CURRENCIES: [&str; 2] = ["IDR", "USD"];
const PAYMENT_METHODS: [&str; 4] = ["BANK_TRANSFER", "CASH", "VIRTUAL_ACCOUNT", "OTHER"];

// ── contract-pricing.ts ─────────────────────────────────────────────────────

/// `Math.round`.
fn js_round(x: f64) -> f64 {
    (x + 0.5).floor()
}

/// `roundMoney`: IDR ke rupiah penuh, selain itu ke sen.
pub fn round_money(amount: f64, currency: &str) -> f64 {
    let factor = if currency == "IDR" { 1.0 } else { 100.0 };
    js_round(amount * factor) / factor
}

#[derive(Debug, Clone, PartialEq)]
pub struct Contract {
    pub price_per_user: f64,
    pub currency: String,
    pub period_months: i32,
    pub tax_percent: f64,
}

impl Contract {
    pub fn to_json(&self) -> Value {
        json!({
            "pricePerUser": num(self.price_per_user),
            "currency": self.currency,
            "periodMonths": self.period_months,
            "taxPercent": num(self.tax_percent),
        })
    }
}

/// `(subtotal, tax, total)`.
pub fn compute_contract_amounts(c: &Contract, seats: i32) -> (f64, f64, f64) {
    let subtotal = round_money(
        c.price_per_user * f64::from(seats) * f64::from(c.period_months),
        &c.currency,
    );
    let tax = round_money(subtotal * c.tax_percent / 100.0, &c.currency);
    (subtotal, tax, round_money(subtotal + tax, &c.currency))
}

fn last_day_of_month(y: i32, m: u32) -> u32 {
    let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
    NaiveDate::from_ymd_opt(ny, nm, 1)
        .and_then(|d| d.pred_opt())
        .map_or(28, |d| d.day())
}

/// `addMonths(date, months)` (UTC), dijepit ke hari terakhir bulan tujuan.
pub fn add_months(d: NaiveDateTime, months: i32) -> NaiveDateTime {
    let total = d.year() * 12 + d.month0() as i32 + months;
    let y = total.div_euclid(12);
    let m = total.rem_euclid(12) as u32 + 1;
    let day = d.day().min(last_day_of_month(y, m));
    NaiveDate::from_ymd_opt(y, m, day).map_or(d, |nd| nd.and_time(d.time()))
}

/// `periodLabel(months)`.
pub fn period_label(months: i32) -> String {
    match months {
        1 => "bulanan".into(),
        12 => "tahunan".into(),
        m if m % 12 == 0 => format!("{} tahunan", m / 12),
        m => format!("{m} bulanan"),
    }
}

/// `contractOf(sub)`.
pub fn contract_of(sub: &CurrentSub) -> Option<Contract> {
    let price = sub.contract_price_per_user?;
    let currency = sub.contract_currency.clone().filter(|c| !c.is_empty())?;
    let months = sub.contract_period_months.filter(|m| *m != 0)?;
    Some(Contract {
        price_per_user: price,
        currency,
        period_months: months,
        tax_percent: sub.contract_tax_percent,
    })
}

fn start_of_utc_day(d: NaiveDateTime) -> NaiveDateTime {
    d.date().and_hms_opt(0, 0, 0).unwrap_or(d)
}

/// `nextInvoicePeriod(sub, periodMonths, now, graceDays)` → `(start, end, dueDate)`.
pub fn next_invoice_period(
    end_date: NaiveDateTime,
    period_months: i32,
    now: NaiveDateTime,
    grace_days: i64,
) -> (NaiveDateTime, NaiveDateTime, NaiveDateTime) {
    let grace = Duration::milliseconds(grace_days * DAY_MS);
    let cutoff = now - grace;
    let start = if end_date >= cutoff {
        end_date
    } else {
        start_of_utc_day(now)
    };
    let due = if start > now { start } else { now + grace };
    (start, add_months(start, period_months), due)
}

// ── DTO ─────────────────────────────────────────────────────────────────────

fn ms(raw: Option<&Value>) -> Option<i64> {
    raw.and_then(Value::as_str)
        .and_then(v8_date)
        .map(|d| d.and_utc().timestamp_millis())
}

fn now_ms() -> i64 {
    now_naive().and_utc().timestamp_millis()
}

/// `limit.setUTCFullYear(limit.getUTCFullYear() + years)` (29 Feb → 1 Mar).
fn add_years_js(d: NaiveDateTime, years: i32) -> NaiveDateTime {
    let y = d.year() + years;
    match NaiveDate::from_ymd_opt(y, d.month(), d.day()) {
        Some(nd) => nd.and_time(d.time()),
        None => NaiveDate::from_ymd_opt(y, 3, 1).map_or(d, |nd| nd.and_time(d.time())),
    }
}

#[derive(Debug, Clone)]
pub struct Payment {
    pub amount: f64,
    pub currency: String,
    pub method: String,
    pub reference: String,
    pub paid_at: NaiveDateTime,
    pub note: Option<String>,
}

const PAYMENT_FIELDS: [&str; 6] = [
    "amount",
    "currency",
    "method",
    "reference",
    "paidAt",
    "note",
];
const CONTRACT_FIELDS: [&str; 4] = ["pricePerUser", "currency", "periodMonths", "taxPercent"];

/// `ManualPaymentDto` (bawaan `currency = 'IDR'`, `reference` di-trim).
fn validate_payment(v: &mut V, raw: &Map<String, Value>) -> Map<String, Value> {
    let mut m = raw.clone();
    if !m.contains_key("currency") {
        m.insert("currency".into(), json!("IDR"));
    }
    trim_field(&mut m, "reference");
    let mut c = V::nested("payment");
    c.whitelist(raw, &PAYMENT_FIELDS);
    let amount = m.get("amount");
    c.max("amount", amount, 1e12);
    c.is_positive("amount", amount);
    c.is_number("amount", amount, Some(2));
    c.is_in("currency", m.get("currency"), &CURRENCIES);
    c.is_in("method", m.get("method"), &PAYMENT_METHODS);
    c.length("reference", m.get("reference"), 1, 100);
    c.is_string("reference", m.get("reference"));
    not_in_future(&mut c, "paidAt", m.get("paidAt"));
    c.is_iso("paidAt", m.get("paidAt"));
    if !absent(m.get("note")) {
        c.max_length("note", m.get("note"), 1000);
        c.is_string("note", m.get("note"));
    }
    v.errors.extend(c.errors);
    m
}

fn payment_of(m: &Map<String, Value>) -> Payment {
    let s = |k: &str| {
        m.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    Payment {
        amount: num_of(m.get("amount")).unwrap_or(0.0),
        currency: s("currency"),
        method: s("method"),
        reference: s("reference"),
        paid_at: m
            .get("paidAt")
            .and_then(Value::as_str)
            .and_then(v8_date)
            .unwrap_or_default(),
        note: m.get("note").and_then(Value::as_str).map(str::to_owned),
    }
}

/// `ManualContractDto` (bawaan `currency = 'IDR'`, `taxPercent = 0`). `prefix` = `contract.`
/// untuk properti bersarang, kosong untuk `SetManualContractDto`.
fn validate_contract(v: &mut V, raw: &Map<String, Value>, nested: bool) -> Map<String, Value> {
    let mut m = raw.clone();
    if !m.contains_key("currency") {
        m.insert("currency".into(), json!("IDR"));
    }
    if !m.contains_key("taxPercent") {
        m.insert("taxPercent".into(), json!(0));
    }
    let mut c = if nested {
        V::nested("contract")
    } else {
        V::new()
    };
    if nested {
        c.whitelist(raw, &CONTRACT_FIELDS);
    }
    let p = m.get("pricePerUser");
    c.max("pricePerUser", p, 1e9);
    c.min("pricePerUser", p, 0.0);
    c.is_number("pricePerUser", p, Some(2));
    c.is_in("currency", m.get("currency"), &CURRENCIES);
    c.int_range(
        "periodMonths",
        m.get("periodMonths"),
        1.0,
        CONTRACT_MAX_PERIOD_MONTHS,
    );
    let t = m.get("taxPercent");
    c.max("taxPercent", t, 100.0);
    c.min("taxPercent", t, 0.0);
    c.is_number("taxPercent", t, Some(2));
    v.errors.extend(c.errors);
    m
}

fn contract_from(m: &Map<String, Value>) -> Contract {
    Contract {
        price_per_user: num_of(m.get("pricePerUser")).unwrap_or(0.0),
        currency: m
            .get("currency")
            .and_then(Value::as_str)
            .unwrap_or("IDR")
            .to_owned(),
        period_months: num_of(m.get("periodMonths")).map_or(0, |n| n as i32),
        tax_percent: num_of(m.get("taxPercent")).unwrap_or(0.0),
    }
}

fn not_in_future(v: &mut V, p: &str, val: Option<&Value>) {
    if ms(val).is_none_or(|t| t > now_ms()) {
        v.push(format!("{p} must not be in the future"));
    }
}

fn end_in_future(v: &mut V, val: Option<&Value>) {
    if ms(val).is_none_or(|t| t <= now_ms()) {
        v.push("endDate must be later than the current time");
    }
}

/// `@ValidateIf(o => o.payment !== null) @IsDefined @ValidateNested`. `None` = `payment: null`.
fn validate_payment_prop(v: &mut V, m: &Map<String, Value>) -> Option<Payment> {
    match m.get("payment") {
        Some(Value::Null) => None,
        None => {
            v.defined("payment", None);
            None
        }
        Some(Value::Object(p)) => {
            let t = validate_payment(v, p);
            Some(payment_of(&t))
        }
        Some(_) => {
            v.nested_not_object("payment");
            None
        }
    }
}

/// `ReasonRequiredWhenUnpaidRule` (sesudah trim).
fn reason_when_unpaid(v: &mut V, m: &Map<String, Value>, issue_invoice: bool) {
    let reason = m.get("reason");
    let payment_not_null = !matches!(m.get("payment"), Some(Value::Null));
    if reason.is_none() && (payment_not_null || issue_invoice) {
        return;
    }
    let ok =
        matches!(reason, Some(Value::String(s)) if (5..=500).contains(&s.encode_utf16().count()));
    if !ok {
        v.push("reason must be 5-500 characters and is required when payment is null");
    }
}

#[derive(Debug, Clone)]
pub struct ActivateInput {
    pub pricing_uuid: String,
    pub start: NaiveDateTime,
    pub end: NaiveDateTime,
    pub max_users: i32,
    pub payment: Option<Payment>,
    pub contract: Option<Contract>,
    pub issue_invoice: bool,
    pub reason: Option<String>,
}

/// `ActivateManualDto`.
pub fn validate_activate(body: &Value) -> ApiResult<ActivateInput> {
    let mut m = as_obj(body);
    trim_field(&mut m, "reason");
    let mut v = V::new();
    for e in array_keys(body) {
        v.push(e);
    }
    v.whitelist(
        &m,
        &[
            "pricingUuid",
            "startDate",
            "endDate",
            "maxUsers",
            "payment",
            "contract",
            "issueInvoice",
            "reason",
        ],
    );
    v.is_uuid("pricingUuid", m.get("pricingUuid"));
    v.is_iso("startDate", m.get("startDate"));
    let start = ms(m.get("startDate"));
    let end = ms(m.get("endDate"));
    // PeriodWithinYears, EndDateInFuture, EndDateAfterStartDate, IsISO8601.
    let within = match (
        m.get("startDate").and_then(Value::as_str).and_then(v8_date),
        end,
    ) {
        (Some(s), Some(e)) => {
            e <= add_years_js(s, MANUAL_MAX_PERIOD_YEARS)
                .and_utc()
                .timestamp_millis()
        }
        _ => false,
    };
    if !within {
        v.push(format!(
            "endDate must be at most {MANUAL_MAX_PERIOD_YEARS} years after startDate"
        ));
    }
    end_in_future(&mut v, m.get("endDate"));
    if !matches!((start, end), (Some(s), Some(e)) if s < e) {
        v.push("endDate must be later than startDate");
    }
    v.is_iso("endDate", m.get("endDate"));
    v.int_range("maxUsers", m.get("maxUsers"), 1.0, 100_000.0);
    let payment = validate_payment_prop(&mut v, &m);
    let contract = match m.get("contract") {
        None | Some(Value::Null) => None,
        Some(Value::Object(c)) => Some(contract_from(&validate_contract(&mut v, c, true))),
        Some(_) => {
            v.nested_not_object("contract");
            None
        }
    };
    let issue = m.get("issueInvoice");
    if !absent(issue) {
        if issue == Some(&Value::Bool(true)) {
            let has_contract = !absent(m.get("contract"));
            let payment_null = matches!(m.get("payment"), Some(Value::Null));
            if !(has_contract && payment_null) {
                v.push("issueInvoice requires a contract and payment: null");
            }
        }
        v.is_boolean("issueInvoice", issue);
    }
    let issue_invoice = issue == Some(&Value::Bool(true));
    reason_when_unpaid(&mut v, &m, issue_invoice);
    v.finish()?;
    Ok(ActivateInput {
        pricing_uuid: m
            .get("pricingUuid")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        start: m
            .get("startDate")
            .and_then(Value::as_str)
            .and_then(v8_date)
            .unwrap_or_default(),
        end: m
            .get("endDate")
            .and_then(Value::as_str)
            .and_then(v8_date)
            .unwrap_or_default(),
        max_users: num_of(m.get("maxUsers")).map_or(0, |n| n as i32),
        payment,
        contract,
        issue_invoice,
        reason: m.get("reason").and_then(Value::as_str).map(str::to_owned),
    })
}

#[derive(Debug, Clone)]
pub struct ExtendInput {
    pub end: NaiveDateTime,
    pub payment: Option<Payment>,
    pub max_users: Option<i32>,
    pub pricing_uuid: Option<String>,
    pub reason: Option<String>,
}

/// `ExtendManualDto`.
pub fn validate_extend(body: &Value) -> ApiResult<ExtendInput> {
    let mut m = as_obj(body);
    trim_field(&mut m, "reason");
    let mut v = V::new();
    for e in array_keys(body) {
        v.push(e);
    }
    v.whitelist(
        &m,
        &["endDate", "payment", "maxUsers", "pricingUuid", "reason"],
    );
    end_in_future(&mut v, m.get("endDate"));
    v.is_iso("endDate", m.get("endDate"));
    let payment = validate_payment_prop(&mut v, &m);
    if !absent(m.get("maxUsers")) {
        v.int_range("maxUsers", m.get("maxUsers"), 1.0, 100_000.0);
    }
    if !absent(m.get("pricingUuid")) {
        v.is_uuid("pricingUuid", m.get("pricingUuid"));
    }
    reason_when_unpaid(&mut v, &m, false);
    v.finish()?;
    Ok(ExtendInput {
        end: m
            .get("endDate")
            .and_then(Value::as_str)
            .and_then(v8_date)
            .unwrap_or_default(),
        payment,
        max_users: num_of(m.get("maxUsers")).map(|n| n as i32),
        pricing_uuid: m
            .get("pricingUuid")
            .and_then(Value::as_str)
            .map(str::to_owned),
        reason: m.get("reason").and_then(Value::as_str).map(str::to_owned),
    })
}

/// `SetManualContractDto` → (kontrak, maxUsers, reason).
pub fn validate_set_contract(body: &Value) -> ApiResult<(Contract, Option<i32>, Option<String>)> {
    let mut m = as_obj(body);
    trim_field(&mut m, "reason");
    let mut v = V::new();
    for e in array_keys(body) {
        v.push(e);
    }
    v.whitelist(
        &m,
        &[
            "maxUsers",
            "reason",
            "pricePerUser",
            "currency",
            "periodMonths",
            "taxPercent",
        ],
    );
    if !absent(m.get("maxUsers")) {
        v.int_range("maxUsers", m.get("maxUsers"), 1.0, 100_000.0);
    }
    if !absent(m.get("reason")) {
        v.length("reason", m.get("reason"), 5, 500);
        v.is_string("reason", m.get("reason"));
    }
    let c = validate_contract(&mut v, &m, false);
    v.finish()?;
    Ok((
        contract_from(&c),
        num_of(m.get("maxUsers")).map(|n| n as i32),
        m.get("reason").and_then(Value::as_str).map(str::to_owned),
    ))
}

fn optional_note(v: &mut V, m: &Map<String, Value>) {
    if !absent(m.get("note")) {
        v.max_length("note", m.get("note"), 1000);
        v.is_string("note", m.get("note"));
    }
}

/// `CreateManualInvoiceDto` → note.
pub fn validate_create_invoice(body: &Value) -> ApiResult<Option<String>> {
    let mut m = as_obj(body);
    trim_field(&mut m, "note");
    let mut v = V::new();
    for e in array_keys(body) {
        v.push(e);
    }
    v.whitelist(&m, &["note"]);
    optional_note(&mut v, &m);
    v.finish()?;
    Ok(m.get("note").and_then(Value::as_str).map(str::to_owned))
}

#[derive(Debug, Clone)]
pub struct PayInput {
    pub method: String,
    pub reference: String,
    pub paid_at_raw: String,
    pub paid_at: NaiveDateTime,
    pub note: Option<String>,
}

/// `PayManualInvoiceDto`.
pub fn validate_pay(body: &Value) -> ApiResult<PayInput> {
    let mut m = as_obj(body);
    trim_field(&mut m, "reference");
    trim_field(&mut m, "note");
    let mut v = V::new();
    for e in array_keys(body) {
        v.push(e);
    }
    v.whitelist(&m, &["method", "reference", "paidAt", "note"]);
    v.is_in("method", m.get("method"), &PAYMENT_METHODS);
    v.length("reference", m.get("reference"), 1, 100);
    v.is_string("reference", m.get("reference"));
    not_in_future(&mut v, "paidAt", m.get("paidAt"));
    v.is_iso("paidAt", m.get("paidAt"));
    optional_note(&mut v, &m);
    v.finish()?;
    let s = |k: &str| {
        m.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    Ok(PayInput {
        method: s("method"),
        reference: s("reference"),
        paid_at_raw: s("paidAt"),
        paid_at: v8_date(&s("paidAt")).unwrap_or_default(),
        note: m.get("note").and_then(Value::as_str).map(str::to_owned),
    })
}

/// `StopManualDto` → (mode, reason).
pub fn validate_stop(body: &Value) -> ApiResult<(String, String)> {
    let mut m = as_obj(body);
    trim_field(&mut m, "reason");
    let mut v = V::new();
    for e in array_keys(body) {
        v.push(e);
    }
    v.whitelist(&m, &["mode", "reason"]);
    v.is_in("mode", m.get("mode"), &["NOW", "AT_END"]);
    v.reason(m.get("reason"));
    v.finish()?;
    let s = |k: &str| {
        m.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    Ok((s("mode"), s("reason")))
}

// ── galat ───────────────────────────────────────────────────────────────────

fn duplicate_reference() -> ApiError {
    coded(
        StatusCode::CONFLICT,
        "DUPLICATE_PAYMENT_REFERENCE",
        "This payment reference has already been recorded for this company.",
    )
}

fn not_manual_billing() -> ApiError {
    coded(
        StatusCode::CONFLICT,
        "NOT_MANUAL_BILLING",
        "The current subscription is not on manual billing.",
    )
}

fn not_unpaid() -> ApiError {
    coded(
        StatusCode::CONFLICT,
        "NOT_UNPAID",
        "Only UNPAID invoices can be marked as paid.",
    )
}

fn not_active() -> ApiError {
    coded(
        StatusCode::CONFLICT,
        "NOT_ACTIVE",
        "The manual subscription is not active.",
    )
}

/// `isDuplicatePaymentReferenceError` → 409 `DUPLICATE_PAYMENT_REFERENCE`.
pub fn map_write_error(e: ApiError) -> ApiError {
    match &e {
        ApiError::UniqueViolation(m) => {
            let l = m.to_lowercase();
            if l.contains(MANUAL_REFERENCE_INDEX) || l.contains("paymentreference") {
                duplicate_reference()
            } else {
                e
            }
        }
        _ => e,
    }
}

// ── helper DB ───────────────────────────────────────────────────────────────

async fn find_company(state: &AppState, uuid: &uuid::Uuid) -> ApiResult<(i32, uuid::Uuid)> {
    let row: Option<(i32, uuid::Uuid)> =
        sqlx::query_as("SELECT id, uuid FROM company WHERE uuid = $1")
            .bind(uuid)
            .fetch_optional(&state.db)
            .await?;
    row.ok_or_else(crate::common::company_not_found)
}

#[derive(sqlx::FromRow)]
struct ActiveTier {
    id: i32,
    name: String,
    #[sqlx(rename = "pricePerUserUsd")]
    price_per_user_usd: f64,
    #[sqlx(rename = "billingInterval")]
    billing_interval: String,
}

impl ActiveTier {
    fn interval(&self) -> &str {
        if self.billing_interval.is_empty() {
            "MONTH"
        } else {
            &self.billing_interval
        }
    }
}

async fn find_active_tier(state: &AppState, uuid: &str) -> ApiResult<ActiveTier> {
    let row: Option<ActiveTier> = sqlx::query_as(
        r#"SELECT id, name, "pricePerUserUsd", "billingInterval" FROM "pricingTiers"
           WHERE uuid = $1::uuid AND status = 1 ORDER BY id LIMIT 1"#,
    )
    .bind(uuid)
    .fetch_optional(&state.db)
    .await?;
    row.ok_or_else(|| {
        coded(
            StatusCode::NOT_FOUND,
            "PRICING_NOT_FOUND",
            "Active pricing tier not found",
        )
    })
}

async fn lock_company(conn: &mut PgConnection, company_id: i32) -> sqlx::Result<()> {
    sqlx::query(r#"SELECT id FROM "company" WHERE id = $1 FOR UPDATE"#)
        .bind(company_id)
        .execute(conn)
        .await?;
    Ok(())
}

async fn sub_by_id(conn: &mut PgConnection, id: i32) -> ApiResult<CurrentSub> {
    Ok(
        sqlx::query_as::<_, CurrentSub>(&current_sub_sql("s.id = $1"))
            .bind(id)
            .fetch_one(conn)
            .await?,
    )
}

fn is_manual_running(sub: &CurrentSub, now: NaiveDateTime) -> bool {
    sub.billing_source == "MANUAL" && sub.status == "ACTIVE" && sub.end_date > now
}

/// `isExtendable`: MANUAL dan ACTIVE/INACTIVE.
fn is_extendable(sub: Option<&CurrentSub>) -> bool {
    sub.is_some_and(|s| {
        s.billing_source == "MANUAL" && matches!(s.status.as_str(), "ACTIVE" | "INACTIVE")
    })
}

fn assert_activatable(sub: Option<&CurrentSub>, now: NaiveDateTime) -> ApiResult<()> {
    let Some(s) = sub else { return Ok(()) };
    if is_manual_running(s, now) {
        return Err(coded(
            StatusCode::CONFLICT,
            "ALREADY_MANUAL_ACTIVE",
            "A manual subscription is already running. Use extend instead.",
        ));
    }
    if s.pending_paypal_subscription_id.is_some() {
        return Err(coded(
            StatusCode::CONFLICT,
            "PENDING_PLAN_CHANGE",
            "A PayPal plan change is in progress. Try again after it completes.",
        ));
    }
    Ok(())
}

async fn assert_start_after_previous<'e>(
    exec: impl sqlx::PgExecutor<'e>,
    company_id: i32,
    sub: Option<&CurrentSub>,
    start: NaiveDateTime,
) -> ApiResult<()> {
    let Some(s) = sub else { return Ok(()) };
    let newer: Option<i32> = sqlx::query_scalar(
        r#"SELECT id FROM subscriptions WHERE "companyId" = $1 AND id <> $2 AND "startDate" >= $3 LIMIT 1"#,
    )
    .bind(company_id)
    .bind(s.id)
    .bind(start)
    .fetch_optional(exec)
    .await?;
    if newer.is_some() {
        return Err(coded(
            StatusCode::BAD_REQUEST,
            "START_DATE_BEFORE_PREVIOUS_SUBSCRIPTION",
            "startDate must be later than the start date of every previous subscription of this company.",
        ));
    }
    Ok(())
}

async fn assert_seats<'e>(
    exec: impl sqlx::PgExecutor<'e>,
    company_id: i32,
    max_users: i32,
) -> ApiResult<()> {
    let members: i64 = sqlx::query_scalar(
        r#"SELECT COUNT(*) FROM "userCompany" WHERE "companyId" = $1 AND status = 1"#,
    )
    .bind(company_id)
    .fetch_one(exec)
    .await?;
    if i64::from(max_users) < members {
        return Err(ApiError::body(
            StatusCode::CONFLICT,
            json!({
                "code": "MAX_USERS_BELOW_MEMBERS",
                "message": format!("maxUsers ({max_users}) is below the number of active members ({members})."),
                "maxUsers": max_users,
                "members": members,
            }),
        ));
    }
    Ok(())
}

async fn assert_reference_free<'e>(
    exec: impl sqlx::PgExecutor<'e>,
    subscription_id: i32,
    reference: &str,
) -> ApiResult<()> {
    let found: Option<i32> = sqlx::query_scalar(
        r#"SELECT id FROM invoices WHERE "subscriptionId" = $1 AND "paymentProvider" = 'manual'
             AND status <> 'CANCELLED'::"InvoiceStatus" AND lower("paymentReference") = lower($2) LIMIT 1"#,
    )
    .bind(subscription_id)
    .bind(reference)
    .fetch_optional(exec)
    .await?;
    if found.is_some() {
        return Err(duplicate_reference());
    }
    Ok(())
}

async fn assert_no_open_invoice(conn: &mut PgConnection, subscription_id: i32) -> ApiResult<()> {
    let open: Option<String> = sqlx::query_scalar(
        r#"SELECT "invoiceNumber" FROM invoices WHERE "subscriptionId" = $1 AND "paymentProvider" = 'manual'
             AND status = 'UNPAID'::"InvoiceStatus" ORDER BY id LIMIT 1"#,
    )
    .bind(subscription_id)
    .fetch_optional(conn)
    .await?;
    if let Some(n) = open {
        return Err(ApiError::body(
            StatusCode::CONFLICT,
            json!({
                "code": "OPEN_INVOICE_EXISTS",
                "message": "An unpaid invoice is already open for this subscription. Record its payment or void it first.",
                "invoiceNumber": n,
            }),
        ));
    }
    Ok(())
}

fn assert_period_matches_contract(
    start: NaiveDateTime,
    end: NaiveDateTime,
    months: i32,
) -> ApiResult<()> {
    let expected = add_months(start, months);
    if start_of_utc_day(expected) != start_of_utc_day(end) {
        return Err(ApiError::body(
            StatusCode::BAD_REQUEST,
            json!({
                "code": "PERIOD_MISMATCH_CONTRACT",
                "message": format!("endDate must be {months} month(s) after startDate to match the contract billing period."),
                "expectedEndDate": date_of(expected),
            }),
        ));
    }
    Ok(())
}

/// `cancelPaypalIfLive` — SEBELUM transaksi; gagal batal = 502 tanpa tulisan apa pun.
async fn cancel_paypal_if_live(state: &AppState, sub: &CurrentSub) -> ApiResult<()> {
    let Some(pid) = sub.paypal_subscription_id.as_deref() else {
        return Ok(());
    };
    if !matches!(sub.paypal_status.as_deref(), Some("ACTIVE" | "SUSPENDED")) {
        return Ok(());
    }
    let paypal = PaypalClient::from_state(state);
    let remote = match paypal.get_subscription(pid).await {
        Ok(r) => r
            .get("status")
            .and_then(Value::as_str)
            .map(str::to_uppercase),
        Err(e) => {
            tracing::warn!("getSubscription {pid} gagal: {e}");
            None
        }
    };
    if !matches!(remote.as_deref(), Some("CANCELLED" | "EXPIRED"))
        && let Err(e) = paypal
            .cancel_subscription(pid, Some(PAYPAL_CANCEL_REASON))
            .await
    {
        tracing::error!("cancelSubscription {pid} gagal: {e}");
        return Err(coded(
            StatusCode::BAD_GATEWAY,
            "PAYPAL_CANCEL_FAILED",
            "Could not cancel the PayPal subscription. Nothing was changed; try again later.",
        ));
    }
    sqlx::query(
        r#"UPDATE subscriptions SET "paypalStatus" = 'CANCELLED' WHERE id = $1 AND "paypalSubscriptionId" = $2"#,
    )
    .bind(sub.id)
    .bind(pid)
    .execute(&state.db)
    .await?;
    Ok(())
}

/// `description.slice(0, 255)` (unit UTF-16).
fn slice_255(s: &str) -> String {
    let units: Vec<u16> = s.encode_utf16().take(255).collect();
    String::from_utf16_lossy(&units)
}

fn invoice_number(now: NaiveDateTime) -> String {
    mz_core::invoice::generate_invoice_number(now.and_utc())
}

/// `createManualInvoice` — invoice PAID + satu item. Mengembalikan uuid.
#[allow(clippy::too_many_arguments)]
async fn create_manual_invoice(
    conn: &mut PgConnection,
    subscription_id: i32,
    p: &Payment,
    period_start: NaiveDateTime,
    period_end: NaiveDateTime,
    description: &str,
    operator_id: i32,
    now: NaiveDateTime,
) -> ApiResult<uuid::Uuid> {
    let (id, uuid): (i32, uuid::Uuid) = sqlx::query_as(
        r#"INSERT INTO invoices ("subscriptionId", "invoiceNumber", "paymentUrl", status, "invoiceType",
               "paymentProvider", currency, "paymentMethod", "paymentReference", "paidAt", "periodStart",
               "periodEnd", note, "subPrice", "totalPrice", "expiredAt", "recordedByOperatorId")
           VALUES ($1, $2, '', 'PAID'::"InvoiceStatus", 'SUBSCRIPTION'::"InvoiceType", 'manual', $3, $4, $5,
                   $6, $7, $8, $9, $10, $10, $6, $11)
           RETURNING id, uuid"#,
    )
    .bind(subscription_id)
    .bind(invoice_number(now))
    .bind(&p.currency)
    .bind(&p.method)
    .bind(&p.reference)
    .bind(p.paid_at)
    .bind(period_start)
    .bind(period_end)
    .bind(p.note.as_deref())
    .bind(p.amount)
    .bind(operator_id)
    .fetch_one(&mut *conn)
    .await?;
    sqlx::query(
        r#"INSERT INTO "invoiceItems" ("invoiceId", description, quantity, "unitPrice", "totalPrice")
           VALUES ($1, $2, 1, $3, $3)"#,
    )
    .bind(id)
    .bind(slice_255(description))
    .bind(p.amount)
    .execute(&mut *conn)
    .await?;
    Ok(uuid)
}

/// Hasil `createUnpaidInvoice` (`select: { id, uuid, invoiceNumber, totalPrice }`).
struct UnpaidInvoice {
    uuid: uuid::Uuid,
    invoice_number: String,
    total: f64,
}

#[allow(clippy::too_many_arguments)]
async fn create_unpaid_invoice(
    conn: &mut PgConnection,
    subscription_id: i32,
    tier_name: &str,
    c: &Contract,
    seats: i32,
    period_start: NaiveDateTime,
    period_end: NaiveDateTime,
    due: NaiveDateTime,
    note: Option<&str>,
    operator_id: i32,
    now: NaiveDateTime,
) -> ApiResult<UnpaidInvoice> {
    let (subtotal, tax, total) = compute_contract_amounts(c, seats);
    let description = format!(
        "Langganan {tier_name} ({}): {seats} user × {} bulan",
        period_label(c.period_months),
        c.period_months
    );
    let number = invoice_number(now);
    let (id, uuid): (i32, uuid::Uuid) = sqlx::query_as(
        r#"INSERT INTO invoices ("subscriptionId", "invoiceNumber", "paymentUrl", status, "invoiceType",
               "paymentProvider", currency, "periodStart", "periodEnd", note, "subPrice", "taxPrice",
               "totalPrice", "expiredAt", "recordedByOperatorId")
           VALUES ($1, $2, '', 'UNPAID'::"InvoiceStatus", 'SUBSCRIPTION'::"InvoiceType", 'manual', $3, $4, $5,
                   $6, $7, $8, $9, $10, $11)
           RETURNING id, uuid"#,
    )
    .bind(subscription_id)
    .bind(&number)
    .bind(&c.currency)
    .bind(period_start)
    .bind(period_end)
    .bind(note)
    .bind(subtotal)
    .bind(tax)
    .bind(total)
    .bind(due)
    .bind(operator_id)
    .fetch_one(&mut *conn)
    .await?;
    sqlx::query(
        r#"INSERT INTO "invoiceItems" ("invoiceId", description, quantity, "unitPrice", "totalPrice")
           VALUES ($1, $2, $3, $4, $5)"#,
    )
    .bind(id)
    .bind(slice_255(&description))
    .bind(f64::from(seats * c.period_months))
    .bind(c.price_per_user)
    .bind(subtotal)
    .execute(&mut *conn)
    .await?;
    Ok(UnpaidInvoice {
        uuid,
        invoice_number: number,
        total,
    })
}

/// `subscriptionSnapshot(sub)`.
fn snapshot(s: &CurrentSub) -> Map<String, Value> {
    obj(json!({
        "subscriptionUuid": s.uuid.to_string(),
        "billingSource": s.billing_source,
        "status": s.status,
        "pricingTierId": s.pricing_tier_id,
        "startDate": date_of(s.start_date),
        "endDate": date_of(s.end_date),
        "maxUsers": s.max_users,
        "autoRenew": s.auto_renew,
        "paypalSubscriptionId": s.paypal_subscription_id,
        "paypalStatus": s.paypal_status,
    }))
}

fn contract_json(s: &CurrentSub) -> Value {
    contract_of(s).map_or(Value::Null, |c| c.to_json())
}

/// `recordOperatorAction(tx, { targetType: 'company', ... })`.
#[allow(clippy::too_many_arguments)]
async fn audit_company(
    conn: &mut PgConnection,
    ctx: &AuditCtx,
    action: &str,
    company: (i32, uuid::Uuid),
    operator_id: i32,
    before: Option<&Value>,
    after: &Value,
    reason: Option<&str>,
) -> ApiResult<()> {
    record(
        conn,
        ctx,
        &OperatorAuditEntry {
            operator_id,
            action,
            target_type: "company",
            target_uuid: Some(company.1),
            company_id: Some(company.0),
            before,
            after: Some(after),
            reason,
        },
    )
    .await?;
    Ok(())
}

async fn invalidate(state: &AppState, company_id: i32) {
    after_company_change(
        state,
        company_id,
        CompanyChange {
            plan: true,
            user_me: true,
            divisions: true,
            modules: true,
        },
    )
    .await;
}

// ── ringkasan ───────────────────────────────────────────────────────────────

/// `LimitUsage`.
#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub max_stores: i64,
    pub total_geofence: i64,
    pub max_auto_check_ins: i64,
    pub max_form_templates: i64,
    pub max_project: i64,
    pub seats: i64,
}

impl Usage {
    fn get(&self, key: &str) -> Option<i64> {
        Some(match key {
            "maxStores" => self.max_stores,
            "totalGeofence" => self.total_geofence,
            "maxAutoCheckIns" => self.max_auto_check_ins,
            "maxFormTemplates" => self.max_form_templates,
            "maxProject" => self.max_project,
            "seats" => self.seats,
            _ => return None,
        })
    }
}

fn as_limit_value(v: Option<&Value>) -> Value {
    match v {
        Some(x @ (Value::Number(_) | Value::Bool(_) | Value::String(_))) => x.clone(),
        _ => Value::Null,
    }
}

/// Input `buildLimitRows`.
pub struct LimitInput<'a> {
    pub tier: Option<&'a Cols>,
    pub override_cols: Option<&'a Cols>,
    pub company_time: &'a Cols,
    pub max_users: Option<i64>,
    pub boost_products: Vec<String>,
    pub time_boosted: Option<&'a Cols>,
    pub usage: Option<&'a Usage>,
}

/// `buildLimitRows`: paket → override → boost → efektif.
pub fn build_limit_rows(input: &LimitInput<'_>) -> Vec<Value> {
    let tier = input.tier;
    let paid = is_paid_tier(tier);
    let plan = tier.map(|t| compute_effective_limits(t, paid, None));
    let with_override = tier.map(|t| compute_effective_limits(t, paid, input.override_cols));
    let sales_boosted = with_override
        .as_ref()
        .map(|w| apply_plan_boosts(w, &input.boost_products));
    let empty = Map::new();
    let ovr = input.override_cols.unwrap_or(&empty);

    let row = |key: &str, group: &str, effective: Value, boost: Value| -> Value {
        let plan_value = as_limit_value(plan.as_ref().and_then(|p| p.get(key)));
        let probe = if plan_value.is_null() {
            &effective
        } else {
            &plan_value
        };
        let kind = if unlimited_of(key).is_some() {
            "count"
        } else if probe.is_boolean() {
            "gate"
        } else {
            "value"
        };
        let used = input
            .usage
            .and_then(|u| u.get(key))
            .map_or(Value::Null, Value::from);
        let mut r = obj(json!({
            "key": key,
            "group": group,
            "kind": kind,
            "plan": plan_value,
            "override": as_limit_value(ovr.get(key)),
            "boost": boost,
            "effective": effective,
            "used": used,
        }));
        if kind == "count" {
            r.insert(
                "unlimitedValue".into(),
                unlimited_of(key).map_or(Value::Null, Value::from),
            );
        }
        Value::Object(r)
    };

    let mut out = Vec::new();
    for key in SALES_LIMIT_FIELDS {
        let before = as_limit_value(with_override.as_ref().and_then(|w| w.get(key)));
        let after = as_limit_value(sales_boosted.as_ref().and_then(|w| w.get(key)));
        let boost = if crate::plan_limits::js_eq(Some(&after), Some(&before)) {
            Value::Null
        } else {
            after.clone()
        };
        out.push(row(key, "SALES", after, boost));
    }
    for key in TIME_LIMIT_FIELDS {
        let boost = input
            .time_boosted
            .filter(|b| b.contains_key(key))
            .map_or(Value::Null, |b| as_limit_value(b.get(key)));
        out.push(row(
            key,
            "TIME",
            as_limit_value(input.company_time.get(key)),
            boost,
        ));
    }
    out.push(json!({
        "key": "maxUsers",
        "group": "SEATS",
        "kind": "count",
        "plan": tier.and_then(|t| t.get("maxUsers")).cloned().unwrap_or(Value::Null),
        "override": Value::Null,
        "boost": Value::Null,
        "effective": input.max_users,
        "used": input.usage.map(|u| u.seats),
        "unlimitedValue": 0,
    }));
    out
}

async fn count_usage(state: &AppState, company_id: i32) -> sqlx::Result<Usage> {
    let row: (i64, i64, i64, i64, i64, i64) = sqlx::query_as(
        r#"SELECT
             (SELECT COUNT(*) FROM store WHERE "companyId" = $1 AND "deletedAt" IS NULL),
             (SELECT COUNT(*) FROM geofence WHERE "companyId" = $1 AND "isActive" = true),
             (SELECT COUNT(*) FROM "storeVisitFormTemplate" WHERE "companyId" = $1 AND "deletedAt" IS NULL),
             (SELECT COUNT(*) FROM "storeVisitLocationEvent" e JOIN "storeVisit" v ON v.id = e."visitId"
                WHERE v."companyId" = $1 AND e.source = 'ping'),
             (SELECT COUNT(*) FROM "projectCompany" pc JOIN project p ON p.id = pc."projectId"
                WHERE pc."companyId" = $1 AND p.status = 1),
             (SELECT COUNT(*) FROM "userCompany" WHERE "companyId" = $1 AND status IN (1, 2))"#,
    )
    .bind(company_id)
    .fetch_one(&state.db)
    .await?;
    Ok(Usage {
        max_stores: row.0,
        total_geofence: row.1,
        max_form_templates: row.2,
        max_auto_check_ins: row.3,
        max_project: row.4,
        seats: row.5,
    })
}

#[derive(sqlx::FromRow)]
struct SummaryCompany {
    cols: SqlJson<Value>,
    #[sqlx(rename = "isTrial")]
    is_trial: bool,
    #[sqlx(rename = "maxUsers")]
    max_users: i32,
    boosts: SqlJson<Vec<(String, Option<Value>)>>,
}

#[derive(sqlx::FromRow)]
struct SummaryOverride {
    cols: SqlJson<Value>,
    note: Option<String>,
    #[sqlx(rename = "updatedAt")]
    updated_at: NaiveDateTime,
    op_uuid: Option<uuid::Uuid>,
    op_name: Option<String>,
}

#[derive(sqlx::FromRow)]
struct SummaryInvoice {
    uuid: uuid::Uuid,
    #[sqlx(rename = "invoiceNumber")]
    invoice_number: String,
    status: String,
    #[sqlx(rename = "paymentProvider")]
    payment_provider: String,
    currency: String,
    #[sqlx(rename = "totalPrice")]
    total_price: f64,
    #[sqlx(rename = "periodStart")]
    period_start: Option<NaiveDateTime>,
    #[sqlx(rename = "periodEnd")]
    period_end: Option<NaiveDateTime>,
    #[sqlx(rename = "paidAt")]
    paid_at: Option<NaiveDateTime>,
    #[sqlx(rename = "paymentReference")]
    payment_reference: Option<String>,
    #[sqlx(rename = "expiredAt")]
    expired_at: NaiveDateTime,
    #[sqlx(rename = "subPrice")]
    sub_price: f64,
    #[sqlx(rename = "taxPrice")]
    tax_price: f64,
    note: Option<String>,
}

/// `lockOf(sub, isTrial, now)`.
fn lock_of(state: &AppState, s: &CurrentSub, is_trial: bool, now: NaiveDateTime) -> Value {
    let cfg = &state.config;
    let lockable = LockableSubscription {
        status: s
            .status
            .parse::<SubscriptionStatus>()
            .unwrap_or(SubscriptionStatus::PENDING),
        end_date: JsDate(s.end_date),
        price_per_user: s.price_per_user,
        billing_source: s
            .billing_source
            .parse::<SubscriptionBillingSource>()
            .unwrap_or(SubscriptionBillingSource::PAYPAL),
        company_is_trial: Some(is_trial),
        product_type: None,
    };
    if s.is_manual() {
        let expired = lock::is_manual_expired(&lockable, now);
        let locked = lock::is_manual_locked(cfg, &lockable, now);
        return json!({
            "locked": locked,
            "lockAt": if expired { date_of(lock::manual_lock_at(cfg, s.end_date)) } else { Value::Null },
            "inGracePeriod": expired && !locked,
            "graceDays": cfg.manual_subscription_grace_days,
        });
    }
    let grace = cfg.free_subscription_grace_days;
    if cfg.free_subscription_days.is_none() {
        return json!({ "locked": false, "lockAt": Value::Null, "inGracePeriod": false, "graceDays": grace });
    }
    let expired = lock::is_free_expired(&lockable, now);
    let locked = lock::is_free_locked(cfg, &lockable, now);
    json!({
        "locked": locked,
        "lockAt": date(lock::free_lock_at_for(cfg, &lockable, now)),
        "inGracePeriod": expired && !locked,
        "graceDays": grace,
    })
}

/// `buildSummary(companyId, now)` — dibaca di luar transaksi, sesudah commit.
pub async fn build_summary(
    state: &AppState,
    company_id: i32,
    now: NaiveDateTime,
) -> ApiResult<Value> {
    let company: Option<SummaryCompany> = sqlx::query_as(
        r#"SELECT json_build_object(
                 'maxProject', c."maxProject", 'dataHistoryMonths', c."dataHistoryMonths",
                 'reportExportsPerMonth', c."reportExportsPerMonth",
                 'screenshotRetentionDays', c."screenshotRetentionDays",
                 'screenshotResolution', c."screenshotResolution",
                 'budgetAlertDeliveryChannel', c."budgetAlertDeliveryChannel"::text,
                 'clientProofExportAllowed', c."clientProofExportAllowed",
                 'storeVisitCountsAsAttendance', c."storeVisitCountsAsAttendance",
                 'isTrial', c."isTrial", 'maxUsers', c."maxUsers") AS cols,
                  c."isTrial", c."maxUsers",
                  COALESCE((SELECT json_agg(json_build_array(pb.product::text, pb."boostedValues") ORDER BY pb.id)
                     FROM "companyPlanBoost" pb WHERE pb."companyId" = c.id AND pb.enabled = true), '[]'::json) AS boosts
           FROM company c WHERE c.id = $1"#,
    )
    .bind(company_id)
    .fetch_optional(&state.db)
    .await?;
    let sub = current_sub(&state.db, company_id).await?;
    let ovr: Option<SummaryOverride> = sqlx::query_as(&format!(
        r#"SELECT {} AS cols, po.note, po."updatedAt", o.uuid AS op_uuid, o.name AS op_name
           FROM "companyPlanOverride" po LEFT JOIN operators o ON o.id = po."updatedByOperatorId"
           WHERE po."companyId" = $1"#,
        crate::plan_limits::OVERRIDE_JSON
    ))
    .bind(company_id)
    .fetch_optional(&state.db)
    .await?;
    let invoices: Vec<SummaryInvoice> = sqlx::query_as(
        r#"SELECT i.uuid, i."invoiceNumber", i.status::text AS status, i."paymentProvider", i.currency,
                  i."totalPrice", i."periodStart", i."periodEnd", i."paidAt", i."paymentReference",
                  i."expiredAt", i."subPrice", i."taxPrice", i.note
           FROM invoices i JOIN subscriptions s ON s.id = i."subscriptionId"
           WHERE s."companyId" = $1 ORDER BY i."createdAt" DESC, i.id DESC LIMIT $2"#,
    )
    .bind(company_id)
    .bind(SUMMARY_INVOICE_LIMIT)
    .fetch_all(&state.db)
    .await?;
    let usage = count_usage(state, company_id).await?;

    let tier = sub.as_ref().and_then(CurrentSub::tier);
    let ovr_cols = ovr.as_ref().map(|o| obj(o.cols.0.clone()));
    let boosts = company
        .as_ref()
        .map(|c| c.boosts.0.clone())
        .unwrap_or_default();
    let time_boost = boosts
        .iter()
        .find(|(p, _)| p == "TIME")
        .and_then(|(_, v)| v.as_ref())
        .and_then(Value::as_object)
        .cloned();
    let is_paypal = sub.as_ref().is_some_and(|s| s.billing_source == "PAYPAL");
    let contract = sub.as_ref().filter(|s| s.is_manual()).and_then(contract_of);
    let company_time = company
        .as_ref()
        .map(|c| obj(c.cols.0.clone()))
        .unwrap_or_default();
    let max_users = match &sub {
        Some(s) if s.is_manual() => Some(i64::from(s.max_users)),
        _ => company.as_ref().map(|c| i64::from(c.max_users)),
    };
    let limits = build_limit_rows(&LimitInput {
        tier,
        override_cols: ovr_cols.as_ref(),
        company_time: &company_time,
        max_users,
        boost_products: boosts.iter().map(|(p, _)| p.clone()).collect(),
        time_boosted: time_boost.as_ref(),
        usage: Some(&usage),
    });
    let grace = state.config.manual_subscription_grace_days;

    let contract_v = match (&sub, &contract) {
        (Some(s), Some(c)) => {
            let (start, end, due) = next_invoice_period(s.end_date, c.period_months, now, grace);
            let (subtotal, tax, total) = compute_contract_amounts(c, s.max_users);
            let mut m = obj(c.to_json());
            m.insert("seats".into(), json!(s.max_users));
            m.insert(
                "perPeriod".into(),
                json!({ "subtotal": num(subtotal), "tax": num(tax), "total": num(total) }),
            );
            m.insert(
                "nextPeriod".into(),
                json!({ "start": date_of(start), "end": date_of(end), "dueDate": date_of(due) }),
            );
            Value::Object(m)
        }
        _ => Value::Null,
    };

    let invoices_v: Vec<Value> = invoices
        .into_iter()
        .map(|i| {
            json!({
                "uuid": i.uuid.to_string(),
                "invoiceNumber": i.invoice_number,
                "status": i.status,
                "paymentProvider": i.payment_provider,
                "currency": i.currency,
                "totalPrice": num(i.total_price),
                "periodStart": date(i.period_start),
                "periodEnd": date(i.period_end),
                "paidAt": date(i.paid_at),
                "paymentReference": i.payment_reference,
                "subPrice": num(i.sub_price),
                "taxPrice": num(i.tax_price),
                "note": i.note,
                "dueDate": if i.status == "UNPAID" { date_of(i.expired_at) } else { Value::Null },
            })
        })
        .collect();

    let days_left = sub.as_ref().map(|s| {
        let diff = (s.end_date - now).num_milliseconds() as f64 / DAY_MS as f64;
        diff.ceil().max(0.0) as i64
    });

    Ok(json!({
        "billingSource": sub.as_ref().map(|s| s.billing_source.clone()),
        "status": sub.as_ref().map(|s| s.status.clone()),
        "plan": tier.map_or(Value::Null, |t| json!({
            "uuid": t.get("uuid").cloned().unwrap_or(Value::Null),
            "name": t.get("name").cloned().unwrap_or(Value::Null),
            "productType": t.get("productType").cloned().unwrap_or(Value::Null),
            "isPaid": is_paid_tier(Some(t)),
        })),
        "startDate": date(sub.as_ref().map(|s| s.start_date)),
        "endDate": date(sub.as_ref().map(|s| s.end_date)),
        "daysLeft": days_left,
        "maxUsers": sub.as_ref().map(|s| s.max_users),
        "autoRenew": sub.as_ref().is_some_and(|s| s.auto_renew),
        "paypal": sub.as_ref().map_or(Value::Null, |s| json!({
            "subscriptionId": s.paypal_subscription_id,
            "status": s.paypal_status,
            "paidUntil": if is_paypal && s.price_per_user > 0.0 && s.end_date > now { date_of(s.end_date) } else { Value::Null },
        })),
        "lock": sub.as_ref().map_or(Value::Null, |s| lock_of(state, s, company.as_ref().is_some_and(|c| c.is_trial), now)),
        "limits": limits,
        "override": ovr.as_ref().map_or(Value::Null, |o| json!({
            "note": o.note,
            "updatedAt": date_of(o.updated_at),
            "updatedBy": match (&o.op_uuid, &o.op_name) {
                (Some(u), Some(n)) => json!({ "uuid": u.to_string(), "name": n }),
                _ => Value::Null,
            },
        })),
        "contract": contract_v,
        "invoices": invoices_v,
    }))
}

// ── handler ─────────────────────────────────────────────────────────────────

/// Node: src/admin-company/manual-subscription/manual-subscription.controller.ts:53 (`GET /admin-company/:uuid/subscription`)
pub async fn h_summary(
    State(state): State<AppState>,
    op: Op,
    audit: AuditHandle,
    Path(raw): Path<String>,
) -> ApiResult<Json<Value>> {
    gate(op, &audit, None, Some(ALL_ROLES))?;
    let uuid = uuid_pipe(&raw)?;
    let (company_id, _) = find_company(&state, &uuid).await?;
    Ok(Json(ok(
        build_summary(&state, company_id, now_naive()).await?
    )))
}

fn created(v: Value) -> (StatusCode, Json<Value>) {
    (StatusCode::CREATED, Json(v))
}

/// Node: src/admin-company/manual-subscription/manual-subscription.controller.ts:59 (`POST /admin-company/:uuid/subscription/manual/activate`)
pub async fn h_activate(
    State(state): State<AppState>,
    op: Op,
    ctx: AuditCtx,
    audit: AuditHandle,
    Path(raw): Path<String>,
    body: RawBody,
) -> ApiResult<impl IntoResponse> {
    let op = gate(op, &audit, Some(&body), Some(OWNER_FINANCE))?;
    // Node: galat body (pipe global) muncul sebelum `ParseUUIDPipe` param (diverifikasi paritas).
    let dto = validate_activate(&body_value(body))?;
    let uuid = uuid_pipe(&raw)?;
    let now = now_naive();
    let company = find_company(&state, &uuid).await?;
    let tier = find_active_tier(&state, &dto.pricing_uuid).await?;
    if let (true, Some(c)) = (dto.issue_invoice, &dto.contract) {
        assert_period_matches_contract(dto.start, dto.end, c.period_months)?;
    }

    // Pra-cek di luar kunci (agar PayPal tidak dibatalkan untuk permintaan yang pasti ditolak).
    let current = current_sub(&state.db, company.0).await?;
    assert_activatable(current.as_ref(), now)?;
    assert_start_after_previous(&state.db, company.0, current.as_ref(), dto.start).await?;
    assert_seats(&state.db, company.0, dto.max_users).await?;
    if let (Some(c), Some(p)) = (&current, &dto.payment) {
        assert_reference_free(&state.db, c.id, &p.reference).await?;
    }
    if let Some(c) = &current {
        cancel_paypal_if_live(&state, c).await?;
    }

    activate_tx(&state, &ctx, &audit, &op, company, &tier, &dto, now)
        .await
        .map_err(map_write_error)?;
    invalidate(&state, company.0).await;
    Ok(created(ok(build_summary(&state, company.0, now).await?)))
}

#[allow(clippy::too_many_arguments)]
async fn activate_tx(
    state: &AppState,
    ctx: &AuditCtx,
    audit: &AuditHandle,
    op: &Operator,
    company: (i32, uuid::Uuid),
    tier: &ActiveTier,
    dto: &ActivateInput,
    now: NaiveDateTime,
) -> ApiResult<()> {
    let mut tx = state.db.begin().await?;
    lock_company(&mut tx, company.0).await?;
    let sub = current_sub(&mut *tx, company.0).await?;
    assert_activatable(sub.as_ref(), now)?;
    assert_start_after_previous(&mut *tx, company.0, sub.as_ref(), dto.start).await?;
    assert_seats(&mut *tx, company.0, dto.max_users).await?;
    if let (Some(s), Some(p)) = (&sub, &dto.payment) {
        assert_reference_free(&mut *tx, s.id, &p.reference).await?;
    }

    let sub_id = match &sub {
        Some(s) => {
            let mut qb = sqlx::QueryBuilder::<sqlx::Postgres>::new(
                r#"UPDATE subscriptions SET "billingSource" = 'MANUAL'::"SubscriptionBillingSource",
                     status = 'ACTIVE'::"SubscriptionStatus", "startDate" = "#,
            );
            qb.push_bind(dto.start)
                .push(r#", "endDate" = "#)
                .push_bind(dto.end)
                .push(r#", "pricingTierId" = "#)
                .push_bind(tier.id)
                .push(r#", "pricePerUser" = "#)
                .push_bind(tier.price_per_user_usd)
                .push(r#", "billingInterval" = "#)
                .push_bind(tier.interval().to_owned())
                .push(r#", "maxUsers" = "#)
                .push_bind(dto.max_users)
                .push(
                    r#", "autoRenew" = false, "nextInvoiceDate" = NULL, "pendingPricingTierId" = NULL,
                       "pendingPaypalSubscriptionId" = NULL, "pendingPaypalPlanId" = NULL,
                       "paymentFailedAt" = NULL, "d1ReminderSentAt" = NULL, "manualReminderForEndDate" = NULL"#,
                );
            if let Some(c) = &dto.contract {
                qb.push(r#", "contractPricePerUser" = "#)
                    .push_bind(c.price_per_user)
                    .push(r#", "contractCurrency" = "#)
                    .push_bind(c.currency.clone())
                    .push(r#", "contractPeriodMonths" = "#)
                    .push_bind(c.period_months)
                    .push(r#", "contractTaxPercent" = "#)
                    .push_bind(c.tax_percent);
            }
            if s.paypal_subscription_id.is_some() {
                qb.push(r#", "paypalStatus" = 'CANCELLED'"#);
            }
            qb.push(" WHERE id = ").push_bind(s.id);
            qb.build().execute(&mut *tx).await?;
            s.id
        }
        None => {
            let c = dto.contract.as_ref();
            sqlx::query_scalar(
                r#"INSERT INTO subscriptions ("companyId", "billingSource", status, "startDate", "endDate",
                       "pricingTierId", "pricePerUser", "billingInterval", "maxUsers", "autoRenew",
                       "nextInvoiceDate", "pendingPricingTierId", "pendingPaypalSubscriptionId",
                       "pendingPaypalPlanId", "paymentFailedAt", "d1ReminderSentAt", "manualReminderForEndDate",
                       "contractPricePerUser", "contractCurrency", "contractPeriodMonths", "contractTaxPercent")
                   VALUES ($1, 'MANUAL'::"SubscriptionBillingSource", 'ACTIVE'::"SubscriptionStatus", $2, $3, $4, $5,
                           $6, $7, false, NULL, NULL, NULL, NULL, NULL, NULL, NULL, $8, $9, $10, COALESCE($11, 0))
                   RETURNING id"#,
            )
            .bind(company.0)
            .bind(dto.start)
            .bind(dto.end)
            .bind(tier.id)
            .bind(tier.price_per_user_usd)
            .bind(tier.interval())
            .bind(dto.max_users)
            .bind(c.map(|c| c.price_per_user))
            .bind(c.map(|c| c.currency.clone()))
            .bind(c.map(|c| c.period_months))
            .bind(c.map(|c| c.tax_percent))
            .fetch_one(&mut *tx)
            .await?
        }
    };
    let updated = sub_by_id(&mut tx, sub_id).await?;

    let invoice_uuid = if let Some(p) = &dto.payment {
        Some(
            create_manual_invoice(
                &mut tx,
                sub_id,
                p,
                dto.start,
                dto.end,
                &format!("{} manual subscription", tier.name),
                op.id,
                now,
            )
            .await?,
        )
    } else if let (true, Some(c)) = (dto.issue_invoice, &dto.contract) {
        let due = if dto.start > now {
            dto.start
        } else {
            now + Duration::milliseconds(state.config.manual_subscription_grace_days * DAY_MS)
        };
        Some(
            create_unpaid_invoice(
                &mut tx,
                sub_id,
                &tier.name,
                c,
                dto.max_users,
                dto.start,
                dto.end,
                due,
                None,
                op.id,
                now,
            )
            .await?
            .uuid,
        )
    } else {
        None
    };

    apply_company_plan_limits(&mut tx, audit, company.0).await?;
    update_company(
        &mut tx,
        audit,
        company.0,
        &[
            ("isTrial".into(), Bind::Bool(false)),
            ("maxUsers".into(), Bind::I32(dto.max_users)),
            ("reportExportsUsedThisMonth".into(), Bind::I32(0)),
            ("reportExportsResetAt".into(), Bind::Ts(now)),
        ],
    )
    .await?;

    let before = sub.as_ref().map(|s| Value::Object(snapshot(s)));
    let mut after = snapshot(&updated);
    after.insert(
        "invoiceUuid".into(),
        invoice_uuid.map_or(Value::Null, |u| json!(u.to_string())),
    );
    after.insert(
        "complimentary".into(),
        json!(dto.payment.is_none() && !dto.issue_invoice),
    );
    after.insert("invoiceIssued".into(), json!(dto.issue_invoice));
    after.insert("contract".into(), contract_json(&updated));
    audit_company(
        &mut tx,
        ctx,
        "subscription.manual.activate",
        company,
        op.id,
        before.as_ref(),
        &Value::Object(after),
        dto.reason.as_deref(),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Node: src/admin-company/manual-subscription/manual-subscription.controller.ts:69 (`POST /admin-company/:uuid/subscription/manual/extend`)
pub async fn h_extend(
    State(state): State<AppState>,
    op: Op,
    ctx: AuditCtx,
    audit: AuditHandle,
    Path(raw): Path<String>,
    body: RawBody,
) -> ApiResult<impl IntoResponse> {
    let op = gate(op, &audit, Some(&body), Some(OWNER_FINANCE))?;
    // Node: galat body (pipe global) muncul sebelum `ParseUUIDPipe` param (diverifikasi paritas).
    let dto = validate_extend(&body_value(body))?;
    let uuid = uuid_pipe(&raw)?;
    let now = now_naive();
    let company = find_company(&state, &uuid).await?;
    let tier = match dto.pricing_uuid.as_deref() {
        Some(p) => Some(find_active_tier(&state, p).await?),
        None => None,
    };
    extend_tx(&state, &ctx, &audit, &op, company, tier.as_ref(), &dto, now)
        .await
        .map_err(map_write_error)?;
    invalidate(&state, company.0).await;
    Ok(created(ok(build_summary(&state, company.0, now).await?)))
}

#[allow(clippy::too_many_arguments)]
async fn extend_tx(
    state: &AppState,
    ctx: &AuditCtx,
    audit: &AuditHandle,
    op: &Operator,
    company: (i32, uuid::Uuid),
    tier: Option<&ActiveTier>,
    dto: &ExtendInput,
    now: NaiveDateTime,
) -> ApiResult<()> {
    let mut tx = state.db.begin().await?;
    lock_company(&mut tx, company.0).await?;
    let sub = current_sub(&mut *tx, company.0).await?;
    let Some(sub) = sub.filter(|s| is_extendable(Some(s))) else {
        return Err(not_manual_billing());
    };
    let floor = now.max(sub.end_date);
    if dto.end <= floor {
        return Err(ApiError::body(
            StatusCode::CONFLICT,
            json!({
                "code": "END_DATE_NOT_LATER",
                "message": "endDate must be later than both now and the current end date.",
                "currentEndDate": date_of(sub.end_date),
            }),
        ));
    }
    if let Some(n) = dto.max_users {
        assert_seats(&mut *tx, company.0, n).await?;
    }
    if let Some(p) = &dto.payment {
        assert_reference_free(&mut *tx, sub.id, &p.reference).await?;
    }
    let tier_changed = tier.filter(|t| t.id != sub.pricing_tier_id);

    let mut qb = sqlx::QueryBuilder::<sqlx::Postgres>::new(
        r#"UPDATE subscriptions SET status = 'ACTIVE'::"SubscriptionStatus", "endDate" = "#,
    );
    qb.push_bind(dto.end)
        .push(r#", "autoRenew" = false, "nextInvoiceDate" = NULL"#);
    if let Some(n) = dto.max_users {
        qb.push(r#", "maxUsers" = "#).push_bind(n);
    }
    if let Some(t) = tier_changed {
        qb.push(r#", "pricingTierId" = "#)
            .push_bind(t.id)
            .push(r#", "pricePerUser" = "#)
            .push_bind(t.price_per_user_usd)
            .push(r#", "billingInterval" = "#)
            .push_bind(t.interval().to_owned());
    }
    qb.push(" WHERE id = ").push_bind(sub.id);
    qb.build().execute(&mut *tx).await?;
    let updated = sub_by_id(&mut tx, sub.id).await?;

    let invoice_uuid = match &dto.payment {
        Some(p) => {
            let name = tier
                .map(|t| t.name.clone())
                .or_else(|| sub.tier_name().map(str::to_owned))
                .unwrap_or_else(|| "Plan".into());
            Some(
                create_manual_invoice(
                    &mut tx,
                    sub.id,
                    p,
                    floor,
                    dto.end,
                    &format!("{name} manual subscription extension"),
                    op.id,
                    now,
                )
                .await?,
            )
        }
        None => None,
    };

    apply_company_plan_limits(&mut tx, audit, company.0).await?;
    let mut sets: Vec<(String, Bind)> = Vec::new();
    if let Some(n) = dto.max_users {
        sets.push(("maxUsers".into(), Bind::I32(n)));
    }
    if tier_changed.is_some() {
        sets.push(("reportExportsUsedThisMonth".into(), Bind::I32(0)));
        sets.push(("reportExportsResetAt".into(), Bind::Ts(now)));
    }
    update_company(&mut tx, audit, company.0, &sets).await?;

    let before = Value::Object(snapshot(&sub));
    let mut after = snapshot(&updated);
    after.insert(
        "invoiceUuid".into(),
        invoice_uuid.map_or(Value::Null, |u| json!(u.to_string())),
    );
    after.insert("complimentary".into(), json!(dto.payment.is_none()));
    audit_company(
        &mut tx,
        ctx,
        "subscription.manual.extend",
        company,
        op.id,
        Some(&before),
        &Value::Object(after),
        dto.reason.as_deref(),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Node: src/admin-company/manual-subscription/manual-subscription.controller.ts:77 (`PUT /admin-company/:uuid/subscription/manual/contract`)
pub async fn h_contract(
    State(state): State<AppState>,
    op: Op,
    ctx: AuditCtx,
    audit: AuditHandle,
    Path(raw): Path<String>,
    body: RawBody,
) -> ApiResult<Json<Value>> {
    let op = gate(op, &audit, Some(&body), Some(OWNER_FINANCE))?;
    // Node: galat body (pipe global) muncul sebelum `ParseUUIDPipe` param (diverifikasi paritas).
    let (contract, max_users, reason) = validate_set_contract(&body_value(body))?;
    let uuid = uuid_pipe(&raw)?;
    let now = now_naive();
    let company = find_company(&state, &uuid).await?;

    let mut tx = state.db.begin().await?;
    lock_company(&mut tx, company.0).await?;
    let sub = current_sub(&mut *tx, company.0).await?;
    let Some(sub) = sub.filter(|s| is_extendable(Some(s))) else {
        return Err(not_manual_billing());
    };
    if let Some(n) = max_users {
        assert_seats(&mut *tx, company.0, n).await?;
    }
    let mut qb = sqlx::QueryBuilder::<sqlx::Postgres>::new(
        r#"UPDATE subscriptions SET "contractPricePerUser" = "#,
    );
    qb.push_bind(contract.price_per_user)
        .push(r#", "contractCurrency" = "#)
        .push_bind(contract.currency.clone())
        .push(r#", "contractPeriodMonths" = "#)
        .push_bind(contract.period_months)
        .push(r#", "contractTaxPercent" = "#)
        .push_bind(contract.tax_percent);
    if let Some(n) = max_users {
        qb.push(r#", "maxUsers" = "#).push_bind(n);
    }
    qb.push(" WHERE id = ").push_bind(sub.id);
    qb.build().execute(&mut *tx).await?;
    let updated = sub_by_id(&mut tx, sub.id).await?;
    if let Some(n) = max_users {
        update_company(
            &mut tx,
            &audit,
            company.0,
            &[("maxUsers".into(), Bind::I32(n))],
        )
        .await?;
    }
    let before = json!({ "maxUsers": sub.max_users, "contract": contract_json(&sub) });
    let after = json!({ "maxUsers": updated.max_users, "contract": contract_json(&updated) });
    audit_company(
        &mut tx,
        &ctx,
        "subscription.manual.contract",
        company,
        op.id,
        Some(&before),
        &after,
        reason.as_deref(),
    )
    .await?;
    tx.commit().await?;

    if max_users.is_some() {
        invalidate(&state, company.0).await;
    }
    Ok(Json(ok(build_summary(&state, company.0, now).await?)))
}

/// Node: src/admin-company/manual-subscription/manual-subscription.controller.ts:85 (`POST /admin-company/:uuid/subscription/manual/invoices`)
pub async fn h_create_invoice(
    State(state): State<AppState>,
    op: Op,
    ctx: AuditCtx,
    audit: AuditHandle,
    Path(raw): Path<String>,
    body: RawBody,
) -> ApiResult<impl IntoResponse> {
    let op = gate(op, &audit, Some(&body), Some(OWNER_FINANCE))?;
    // Node: galat body (pipe global) muncul sebelum `ParseUUIDPipe` param (diverifikasi paritas).
    let note = validate_create_invoice(&body_value(body))?;
    let uuid = uuid_pipe(&raw)?;
    let now = now_naive();
    let company = find_company(&state, &uuid).await?;

    let mut tx = state.db.begin().await?;
    lock_company(&mut tx, company.0).await?;
    let sub = current_sub(&mut *tx, company.0).await?;
    let Some(sub) = sub.filter(|s| is_extendable(Some(s))) else {
        return Err(not_manual_billing());
    };
    let Some(contract) = contract_of(&sub) else {
        return Err(coded(
            StatusCode::CONFLICT,
            "CONTRACT_NOT_SET",
            "Set the contract price and billing period before issuing an invoice.",
        ));
    };
    assert_no_open_invoice(&mut tx, sub.id).await?;
    let (start, end, due) = next_invoice_period(
        sub.end_date,
        contract.period_months,
        now,
        state.config.manual_subscription_grace_days,
    );
    let inv = create_unpaid_invoice(
        &mut tx,
        sub.id,
        sub.tier_name().unwrap_or("Plan"),
        &contract,
        sub.max_users,
        start,
        end,
        due,
        note.as_deref().filter(|n| !n.is_empty()),
        op.id,
        now,
    )
    .await?;
    let after = json!({
        "invoiceNumber": inv.invoice_number,
        "totalPrice": num(inv.total),
        "currency": contract.currency,
        "periodStart": date_of(start),
        "periodEnd": date_of(end),
        "seats": sub.max_users,
        "contract": contract.to_json(),
    });
    record(
        &mut *tx,
        &ctx,
        &OperatorAuditEntry {
            operator_id: op.id,
            action: "invoice.manual.create",
            target_type: "invoice",
            target_uuid: Some(inv.uuid),
            company_id: Some(company.0),
            before: None,
            after: Some(&after),
            reason: None,
        },
    )
    .await?;
    tx.commit().await?;
    Ok(created(ok(build_summary(&state, company.0, now).await?)))
}

#[derive(sqlx::FromRow)]
struct PayInvoiceRow {
    id: i32,
    uuid: uuid::Uuid,
    status: String,
    #[sqlx(rename = "paymentProvider")]
    payment_provider: String,
    #[sqlx(rename = "subscriptionId")]
    subscription_id: i32,
    #[sqlx(rename = "periodEnd")]
    period_end: Option<NaiveDateTime>,
    #[sqlx(rename = "totalPrice")]
    total_price: f64,
    currency: String,
    #[sqlx(rename = "invoiceNumber")]
    invoice_number: String,
    note: Option<String>,
}

/// Node: src/admin-company/manual-subscription/manual-subscription.controller.ts:93 (`POST /admin-company/:uuid/subscription/manual/invoices/:invoiceUuid/pay`)
pub async fn h_pay_invoice(
    State(state): State<AppState>,
    op: Op,
    ctx: AuditCtx,
    audit: AuditHandle,
    Path((raw, raw_invoice)): Path<(String, String)>,
    body: RawBody,
) -> ApiResult<impl IntoResponse> {
    let op = gate(op, &audit, Some(&body), Some(OWNER_FINANCE))?;
    // Node: galat body (pipe global) muncul sebelum `ParseUUIDPipe` param (diverifikasi paritas).
    let dto = validate_pay(&body_value(body))?;
    let uuid = uuid_pipe(&raw)?;
    let invoice_uuid = uuid_pipe(&raw_invoice)?;
    let now = now_naive();
    let company = find_company(&state, &uuid).await?;

    pay_tx(&state, &ctx, &op, company, &invoice_uuid, &dto)
        .await
        .map_err(map_write_error)?;
    invalidate(&state, company.0).await;
    Ok(created(ok(build_summary(&state, company.0, now).await?)))
}

async fn pay_tx(
    state: &AppState,
    ctx: &AuditCtx,
    op: &Operator,
    company: (i32, uuid::Uuid),
    invoice_uuid: &uuid::Uuid,
    dto: &PayInput,
) -> ApiResult<()> {
    let mut tx = state.db.begin().await?;
    lock_company(&mut tx, company.0).await?;
    let invoice: Option<PayInvoiceRow> = sqlx::query_as(
        r#"SELECT i.id, i.uuid, i.status::text AS status, i."paymentProvider", i."subscriptionId",
                  i."periodEnd", i."totalPrice", i.currency, i."invoiceNumber", i.note
           FROM invoices i JOIN subscriptions s ON s.id = i."subscriptionId"
           WHERE i.uuid = $1 AND s."companyId" = $2 ORDER BY i.id LIMIT 1"#,
    )
    .bind(invoice_uuid)
    .bind(company.0)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(invoice) = invoice else {
        return Err(coded(
            StatusCode::NOT_FOUND,
            "INVOICE_NOT_FOUND",
            "Invoice not found",
        ));
    };
    if invoice.payment_provider != MANUAL_PAYMENT_PROVIDER {
        return Err(coded(
            StatusCode::CONFLICT,
            "NOT_MANUAL_INVOICE",
            "Only manual invoices can be paid here.",
        ));
    }
    if invoice.status != "UNPAID" {
        return Err(not_unpaid());
    }
    let sub = current_sub(&mut *tx, company.0).await?;
    let Some(sub) = sub.filter(|s| is_extendable(Some(s))) else {
        return Err(not_manual_billing());
    };
    if sub.id != invoice.subscription_id {
        return Err(coded(
            StatusCode::CONFLICT,
            "STALE_INVOICE",
            "This invoice belongs to an earlier subscription. Void it instead.",
        ));
    }
    assert_reference_free(&mut *tx, sub.id, &dto.reference).await?;

    // Deviasi: PDF lama (bertanda belum dibayar) tidak dibangun ulang di sini; penunjuknya
    // dikosongkan supaya unduhan berikutnya membangunnya lazim (`ensureInvoicePdfKey`).
    let note = dto
        .note
        .clone()
        .filter(|n| !n.is_empty())
        .or_else(|| invoice.note.clone());
    let paid = sqlx::query(
        r#"UPDATE invoices SET status = 'PAID'::"InvoiceStatus", "paymentMethod" = $2, "paymentReference" = $3,
               "paidAt" = $4, note = $5, "recordedByOperatorId" = $6, "invoicePathPdf" = ''
           WHERE id = $1 AND status = 'UNPAID'::"InvoiceStatus""#,
    )
    .bind(invoice.id)
    .bind(&dto.method)
    .bind(&dto.reference)
    .bind(dto.paid_at)
    .bind(note)
    .bind(op.id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if paid != 1 {
        return Err(not_unpaid());
    }
    let end = invoice
        .period_end
        .map_or(sub.end_date, |p| sub.end_date.max(p));
    sqlx::query(
        r#"UPDATE subscriptions SET status = 'ACTIVE'::"SubscriptionStatus", "endDate" = $2,
               "autoRenew" = false, "nextInvoiceDate" = NULL WHERE id = $1"#,
    )
    .bind(sub.id)
    .bind(end)
    .execute(&mut *tx)
    .await?;
    let updated = sub_by_id(&mut tx, sub.id).await?;
    let before = json!({
        "status": invoice.status,
        "subscriptionEndDate": date_of(sub.end_date),
        "subscriptionStatus": sub.status,
    });
    let after = json!({
        "status": "PAID",
        "invoiceNumber": invoice.invoice_number,
        "totalPrice": num(invoice.total_price),
        "currency": invoice.currency,
        "paymentMethod": dto.method,
        "paymentReference": dto.reference,
        "paidAt": dto.paid_at_raw,
        "subscriptionEndDate": date_of(updated.end_date),
        "subscriptionStatus": updated.status,
    });
    record(
        &mut *tx,
        ctx,
        &OperatorAuditEntry {
            operator_id: op.id,
            action: "invoice.manual.pay",
            target_type: "invoice",
            target_uuid: Some(invoice.uuid),
            company_id: Some(company.0),
            before: Some(&before),
            after: Some(&after),
            reason: None,
        },
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Node: src/admin-company/manual-subscription/manual-subscription.controller.ts:107 (`POST /admin-company/:uuid/subscription/manual/stop`)
pub async fn h_stop(
    State(state): State<AppState>,
    op: Op,
    ctx: AuditCtx,
    audit: AuditHandle,
    Path(raw): Path<String>,
    body: RawBody,
) -> ApiResult<impl IntoResponse> {
    let op = gate(op, &audit, Some(&body), Some(OWNER_FINANCE))?;
    // Node: galat body (pipe global) muncul sebelum `ParseUUIDPipe` param (diverifikasi paritas).
    let (mode, reason) = validate_stop(&body_value(body))?;
    let uuid = uuid_pipe(&raw)?;
    let now = now_naive();
    let company = find_company(&state, &uuid).await?;

    let mut tx = state.db.begin().await?;
    lock_company(&mut tx, company.0).await?;
    let sub = current_sub(&mut *tx, company.0).await?;
    let Some(sub) = sub.filter(CurrentSub::is_manual) else {
        return Err(not_manual_billing());
    };
    if mode == "AT_END" {
        let after = json!({ "mode": "AT_END", "endDate": date_of(sub.end_date) });
        audit_company(
            &mut tx,
            &ctx,
            "subscription.manual.stop",
            company,
            op.id,
            Some(&Value::Object(snapshot(&sub))),
            &after,
            Some(&reason),
        )
        .await?;
        tx.commit().await?;
        return Ok(created(ok(build_summary(&state, company.0, now).await?)));
    }
    if sub.status != "ACTIVE" {
        return Err(not_active());
    }
    let n = sqlx::query(
        r#"UPDATE subscriptions SET status = 'INACTIVE'::"SubscriptionStatus", "endDate" = $2
           WHERE id = $1 AND "billingSource" = 'MANUAL'::"SubscriptionBillingSource"
             AND status = 'ACTIVE'::"SubscriptionStatus""#,
    )
    .bind(sub.id)
    .bind(now)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if n != 1 {
        return Err(not_active());
    }
    let after = json!({ "mode": "NOW", "status": "INACTIVE", "endDate": date_of(now) });
    audit_company(
        &mut tx,
        &ctx,
        "subscription.manual.stop",
        company,
        op.id,
        Some(&Value::Object(snapshot(&sub))),
        &after,
        Some(&reason),
    )
    .await?;
    tx.commit().await?;
    invalidate(&state, company.0).await;
    Ok(created(ok(build_summary(&state, company.0, now).await?)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dt(s: &str) -> NaiveDateTime {
        v8_date(s).unwrap_or_default()
    }

    fn c(price: f64, cur: &str, months: i32, tax: f64) -> Contract {
        Contract {
            price_per_user: price,
            currency: cur.into(),
            period_months: months,
            tax_percent: tax,
        }
    }

    #[test]
    fn contract_amounts_like_node() {
        assert_eq!(
            compute_contract_amounts(&c(30000.0, "IDR", 3, 0.0), 20),
            (1_800_000.0, 0.0, 1_800_000.0)
        );
        assert_eq!(
            compute_contract_amounts(&c(33333.0, "IDR", 1, 11.0), 7),
            (233_331.0, 25_666.0, 258_997.0)
        );
        assert_eq!(
            compute_contract_amounts(&c(2.335, "USD", 12, 10.0), 3),
            (84.06, 8.41, 92.47)
        );
    }

    #[test]
    fn add_months_clamps() {
        assert_eq!(
            add_months(dt("2026-11-13T00:00:00Z"), 3),
            dt("2027-02-13T00:00:00Z")
        );
        assert_eq!(
            add_months(dt("2026-11-13T00:00:00Z"), 12),
            dt("2027-11-13T00:00:00Z")
        );
        assert_eq!(
            add_months(dt("2027-01-31T00:00:00Z"), 1),
            dt("2027-02-28T00:00:00Z")
        );
        assert_eq!(
            add_months(dt("2028-01-31T00:00:00Z"), 1),
            dt("2028-02-29T00:00:00Z")
        );
    }

    #[test]
    fn period_labels() {
        assert_eq!(period_label(1), "bulanan");
        assert_eq!(period_label(3), "3 bulanan");
        assert_eq!(period_label(12), "tahunan");
        assert_eq!(period_label(24), "2 tahunan");
    }

    #[test]
    fn next_period_continues_within_grace() {
        let now = dt("2026-11-17T10:00:00Z");
        // endDate masih dalam masa tenggang → lanjut dari endDate.
        let (s, e, d) = next_invoice_period(dt("2026-11-12T00:00:00Z"), 1, now, 7);
        assert_eq!(s, dt("2026-11-12T00:00:00Z"));
        assert_eq!(e, dt("2026-12-12T00:00:00Z"));
        assert_eq!(d, dt("2026-11-24T10:00:00Z"));
        // lewat masa tenggang → mulai hari ini (UTC).
        let (s, _, _) = next_invoice_period(dt("2026-10-01T00:00:00Z"), 1, now, 7);
        assert_eq!(s, dt("2026-11-17T00:00:00Z"));
        // awal periode di masa depan → jatuh tempo = awal periode.
        let (s, _, d) = next_invoice_period(dt("2026-12-01T00:00:00Z"), 3, now, 7);
        assert_eq!(d, s);
    }

    fn errs<T: std::fmt::Debug>(r: ApiResult<T>) -> Vec<String> {
        match r {
            Err(ApiError::Validation(m)) => m,
            other => {
                assert!(other.is_ok(), "{other:?}");
                Vec::new()
            }
        }
    }

    #[test]
    fn activate_requires_reason_when_complimentary() {
        let e = errs(validate_activate(&json!({
            "pricingUuid": "3f2c1a9e-8b7d-4c6e-9a1b-2d3e4f5a6b7c",
            "startDate": "2030-01-01", "endDate": "2030-06-01", "maxUsers": 5, "payment": null
        })));
        assert_eq!(
            e,
            vec!["reason must be 5-500 characters and is required when payment is null"]
        );
        // payment hilang (undefined) → IsDefined, reason opsional.
        let e = errs(validate_activate(&json!({
            "pricingUuid": "3f2c1a9e-8b7d-4c6e-9a1b-2d3e4f5a6b7c",
            "startDate": "2030-01-01", "endDate": "2030-06-01", "maxUsers": 5
        })));
        assert_eq!(e, vec!["payment should not be null or undefined"]);
    }

    #[test]
    fn activate_date_rules() {
        let e = errs(validate_activate(&json!({
            "pricingUuid": "3f2c1a9e-8b7d-4c6e-9a1b-2d3e4f5a6b7c",
            "startDate": "2020-01-01", "endDate": "2024-01-02", "maxUsers": 0, "payment": null,
            "reason": "gratis promo"
        })));
        assert_eq!(
            e,
            vec![
                "endDate must be at most 3 years after startDate",
                "endDate must be later than the current time",
                "maxUsers must not be less than 1",
            ]
        );
    }

    #[test]
    fn payment_nested_messages() {
        let e = errs(validate_activate(&json!({
            "pricingUuid": "3f2c1a9e-8b7d-4c6e-9a1b-2d3e4f5a6b7c",
            "startDate": "2030-01-01", "endDate": "2030-06-01", "maxUsers": 5,
            "payment": { "amount": 10.555, "method": "CRYPTO", "reference": "  ", "paidAt": "2999-01-01", "x": 1 }
        })));
        assert_eq!(
            e,
            vec![
                "payment.property x should not exist",
                "payment.amount must be a number conforming to the specified constraints",
                "payment.method must be one of the following values: BANK_TRANSFER, CASH, VIRTUAL_ACCOUNT, OTHER",
                "payment.reference must be longer than or equal to 1 characters",
                "payment.paidAt must not be in the future",
            ]
        );
    }

    #[test]
    fn issue_invoice_rules() {
        let base = json!({
            "pricingUuid": "3f2c1a9e-8b7d-4c6e-9a1b-2d3e4f5a6b7c",
            "startDate": "2030-01-01", "endDate": "2030-02-01", "maxUsers": 5,
            "payment": null, "issueInvoice": true
        });
        assert_eq!(
            errs(validate_activate(&base)),
            vec!["issueInvoice requires a contract and payment: null"]
        );
        let mut ok_body = base.clone();
        if let Some(o) = ok_body.as_object_mut() {
            o.insert(
                "contract".into(),
                json!({ "pricePerUser": 30000, "periodMonths": 1 }),
            );
        }
        let ok = validate_activate(&ok_body).ok();
        assert_eq!(ok.and_then(|a| a.contract), Some(c(30000.0, "IDR", 1, 0.0)));
    }

    #[test]
    fn contract_dto_and_stop_dto() {
        let e = errs(validate_set_contract(&json!({
            "pricePerUser": -1, "currency": "EUR", "periodMonths": 37, "taxPercent": 12.345
        })));
        assert_eq!(
            e,
            vec![
                "pricePerUser must not be less than 0",
                "currency must be one of the following values: IDR, USD",
                "periodMonths must not be greater than 36",
                "taxPercent must be a number conforming to the specified constraints",
            ]
        );
        assert_eq!(
            errs(validate_stop(
                &json!({ "mode": "LATER", "reason": "  stop sekarang " })
            )),
            vec!["mode must be one of the following values: NOW, AT_END"]
        );
    }

    #[test]
    fn duplicate_reference_mapping() {
        let e = map_write_error(ApiError::UniqueViolation(
            "Unique constraint failed on the constraint: `invoices_manual_reference_key`".into(),
        ));
        assert!(matches!(e, ApiError::Body { status, .. } if status == StatusCode::CONFLICT));
    }

    #[test]
    fn limit_rows_shape() {
        let tier = obj(json!({
            "maxUsers": 10, "basePrice": 0, "pricePerUserUsd": 5, "maxProject": 3,
            "screenshotResolution": "640x360", "screenshotRetentionDays": 7, "reportExportsPerMonth": 3,
            "dataHistoryMonths": 3, "budgetAlertDeliveryChannel": "INAPP_ONLY",
            "clientProofExportAllowed": false, "maxStores": 10, "totalGeofence": 2,
            "maxAutoCheckIns": 0, "maxFormTemplates": 0, "visitHistoryDays": 0, "auditLog": false,
            "form": false, "timelineRoute": false, "product": false, "order": false, "divisions": false
        }));
        let ovr = obj(json!({ "maxStores": 50 }));
        let company = obj(json!({ "maxProject": 100 }));
        let boosted = obj(json!({ "maxProject": 100 }));
        let usage = Usage {
            max_stores: 4,
            seats: 3,
            ..Usage::default()
        };
        let rows = build_limit_rows(&LimitInput {
            tier: Some(&tier),
            override_cols: Some(&ovr),
            company_time: &company,
            max_users: Some(10),
            boost_products: vec!["SALES".into(), "TIME".into()],
            time_boosted: Some(&boosted),
            usage: Some(&usage),
        });
        assert_eq!(rows.len(), 19);
        assert_eq!(
            rows.first().cloned().unwrap_or_default(),
            json!({ "key": "maxStores", "group": "SALES", "kind": "count", "plan": 10, "override": 50,
                    "boost": 100, "effective": 100, "used": 4, "unlimitedValue": 0 })
        );
        let audit = rows.get(5).cloned().unwrap_or_default();
        assert_eq!(audit.get("kind"), Some(&json!("gate")));
        let mp = rows.get(11).cloned().unwrap_or_default();
        assert_eq!(mp.get("plan"), Some(&json!(3)));
        assert_eq!(mp.get("boost"), Some(&json!(100)));
        let seats = rows.last().cloned().unwrap_or_default();
        assert_eq!(seats.get("used"), Some(&json!(3)));
    }
}
