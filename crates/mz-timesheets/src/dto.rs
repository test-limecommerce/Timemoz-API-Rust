//! DTO + validasi gaya class-validator untuk keempat modul.
//!
//! Urutan pesan per properti mengikuti class-validator: dekorator paling BAWAH
//! di class Node terdaftar lebih dulu, jadi pesannya muncul lebih dulu.
//! Properti opsional disimpan sebagai `Option<Value>`: `None` = tidak dikirim,
//! `Some(Null)` = dikirim `null` (beda arti di beberapa service Node).

use chrono::NaiveDate;
use mz_core::extract::Validate;
use mz_core::iso8601::is_iso8601;
use mz_core::uuid_rules::is_uuid;
use serde::{Deserialize, Deserializer};
use serde_json::Value;

/// Properti body yang hadir (termasuk `null`).
pub fn present<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(d).map(Some)
}

/// `@IsOptional()`: tidak dikirim ATAU null = dilewati.
pub fn provided(v: &Option<Value>) -> Option<&Value> {
    v.as_ref().filter(|x| !x.is_null())
}

/// `YYYY-MM-DD` yang valid sebagai kalender.
pub fn parse_ymd(s: &str) -> Option<NaiveDate> {
    let b = s.as_bytes();
    if b.len() != 10 || b.get(4) != Some(&b'-') || b.get(7) != Some(&b'-') {
        return None;
    }
    NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()
}

/// `@Matches(/^([01]\d|2[0-3]):(00|30)$/)` untuk `sendTime`.
pub fn is_send_time(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 5 || b.get(2) != Some(&b':') {
        return false;
    }
    let digit = |i: usize| {
        b.get(i)
            .filter(|c| c.is_ascii_digit())
            .map(|c| u32::from(c - b'0'))
    };
    let (Some(h1), Some(h2)) = (digit(0), digit(1)) else {
        return false;
    };
    h1 * 10 + h2 < 24 && matches!(s.get(3..), Some("00" | "30"))
}

fn is_int(v: &Value) -> bool {
    v.as_i64().is_some()
        || v.as_u64().is_some()
        || v.as_f64()
            .is_some_and(|f| f.is_finite() && f.fract() == 0.0)
}

/// `@IsString() @IsOptional()`.
fn opt_string(out: &mut Vec<String>, f: &str, v: &Option<Value>) {
    if let Some(x) = provided(v)
        && !x.is_string()
    {
        out.push(format!("{f} must be a string"));
    }
}

/// `@IsNotEmpty()`: hanya `undefined`, `null`, dan `''` yang gagal.
fn is_empty_value(v: &Option<Value>) -> bool {
    match provided(v) {
        None => true,
        Some(x) => x.as_str().is_some_and(str::is_empty),
    }
}

/// `@IsString()` wajib (tanpa `@IsNotEmpty`).
fn req_string(out: &mut Vec<String>, f: &str, v: &Option<Value>) {
    if !provided(v).is_some_and(Value::is_string) {
        out.push(format!("{f} must be a string"));
    }
}

/// `@IsString() @IsNotEmpty()` wajib.
fn req_string_not_empty(out: &mut Vec<String>, f: &str, v: &Option<Value>) {
    if is_empty_value(v) {
        out.push(format!("{f} should not be empty"));
    }
    req_string(out, f, v);
}

/// `@IsEnum([...])` pada nilai string.
fn enum_msg(f: &str, allowed: &[&str]) -> String {
    format!(
        "{f} must be one of the following values: {}",
        allowed.join(", ")
    )
}

/// `@IsEnum(['A','B'])` (array literal) di Node: class-validator membuang kunci numerik
/// sehingga daftar nilai pada pesan kosong (perilaku aneh Node, tetap kontrak).
fn array_enum_msg(f: &str) -> String {
    format!("{f} must be one of the following values: ")
}

fn enum_ok(v: Option<&Value>, allowed: &[&str]) -> bool {
    v.and_then(Value::as_str)
        .is_some_and(|s| allowed.contains(&s))
}

/// `@Min(0) @IsNumber() @IsOptional()` (urutan: Min lebih dulu).
fn opt_number_min0(out: &mut Vec<String>, f: &str, v: &Option<Value>) {
    if let Some(x) = provided(v) {
        let num = x.as_f64();
        if !num.is_some_and(|n| n >= 0.0) {
            out.push(format!("{f} must not be less than 0"));
        }
        if num.is_none() {
            out.push(format!(
                "{f} must be a number conforming to the specified constraints"
            ));
        }
    }
}

fn opt_bool(out: &mut Vec<String>, f: &str, v: &Option<Value>) {
    if let Some(x) = provided(v)
        && !x.is_boolean()
    {
        out.push(format!("{f} must be a boolean value"));
    }
}

/// `@IsString({each}) @IsArray() @IsOptional()`.
fn opt_string_array(out: &mut Vec<String>, f: &str, v: &Option<Value>) {
    if let Some(x) = provided(v) {
        match x.as_array() {
            Some(a) => {
                if a.iter().any(|e| !e.is_string()) {
                    out.push(format!("each value in {f} must be a string"));
                }
            }
            None => {
                out.push(format!("each value in {f} must be a string"));
                out.push(format!("{f} must be an array"));
            }
        }
    }
}

/// `@IsInt({each}) @IsArray() @IsOptional()`.
fn opt_int_array(out: &mut Vec<String>, f: &str, v: &Option<Value>) {
    if let Some(x) = provided(v) {
        match x.as_array() {
            Some(a) => {
                if a.iter().any(|e| !is_int(e)) {
                    out.push(format!("each value in {f} must be an integer number"));
                }
            }
            None => {
                out.push(format!("each value in {f} must be an integer number"));
                out.push(format!("{f} must be an array"));
            }
        }
    }
}

/// `@IsArray() @ArrayMinSize(1) @IsEmail({}, {each: true})`; `required` = tanpa `@IsOptional`.
fn email_array(out: &mut Vec<String>, f: &str, v: &Option<Value>, required: bool) {
    let x = if required { v.as_ref() } else { provided(v) };
    if !required && x.is_none() {
        return;
    }
    match x.and_then(Value::as_array) {
        Some(a) => {
            if a.iter()
                .any(|e| !e.as_str().is_some_and(mz_core::extract::is_email))
            {
                out.push(format!("each value in {f} must be an email"));
            }
            if a.is_empty() {
                out.push(format!("{f} must contain at least 1 elements"));
            }
        }
        None => {
            out.push(format!("each value in {f} must be an email"));
            out.push(format!("{f} must contain at least 1 elements"));
            out.push(format!("{f} must be an array"));
        }
    }
}

fn send_time(out: &mut Vec<String>, v: &Option<Value>, required: bool, msg: &str) {
    let x = if required {
        v.as_ref().filter(|x| !x.is_null())
    } else {
        provided(v)
    };
    if !required && x.is_none() {
        return;
    }
    if !x.and_then(Value::as_str).is_some_and(is_send_time) {
        out.push(msg.to_owned());
    }
    if !x.is_some_and(Value::is_string) {
        out.push("sendTime must be a string".to_owned());
    }
}

// ───────────────────────── timesheets ─────────────────────────

pub const PAY_PERIOD_TYPES: &[&str] = &["WEEKLY", "BIWEEKLY", "MONTHLY"];
pub const RATE_TYPES: &[&str] = &["HOURLY", "FIXED"];
pub const TIMESHEET_STATUSES: &[&str] = &["PENDING", "APPROVED", "REJECTED"];

/// `SubmitTimesheetDto`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubmitBody {
    #[serde(default, deserialize_with = "present")]
    pub notes: Option<Value>,
}

impl Validate for SubmitBody {
    fn validate(&self) -> Vec<String> {
        let mut out = Vec::new();
        opt_string(&mut out, "notes", &self.notes);
        out
    }
}

/// `RejectTimesheetDto`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RejectBody {
    #[serde(default, deserialize_with = "present")]
    pub rejected_reason: Option<Value>,
}

impl Validate for RejectBody {
    fn validate(&self) -> Vec<String> {
        let mut out = Vec::new();
        req_string_not_empty(&mut out, "rejectedReason", &self.rejected_reason);
        out
    }
}

/// `ReopenTimesheetDto`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReopenBody {
    #[serde(default, deserialize_with = "present")]
    pub reason: Option<Value>,
}

impl Validate for ReopenBody {
    fn validate(&self) -> Vec<String> {
        let mut out = Vec::new();
        opt_string(&mut out, "reason", &self.reason);
        out
    }
}

/// `EditTimesheetDto`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditBody {
    #[serde(default, deserialize_with = "present")]
    pub notes: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub pay_rate: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub rate_type: Option<Value>,
}

impl Validate for EditBody {
    fn validate(&self) -> Vec<String> {
        let mut out = Vec::new();
        opt_string(&mut out, "notes", &self.notes);
        opt_number_min0(&mut out, "payRate", &self.pay_rate);
        opt_rate_type(&mut out, &self.rate_type);
        out
    }
}

/// `@IsString() @IsOptional() @IsEnum(['HOURLY','FIXED'])` (IsEnum lebih dulu).
fn opt_rate_type(out: &mut Vec<String>, v: &Option<Value>) {
    if let Some(x) = provided(v) {
        if !enum_ok(Some(x), RATE_TYPES) {
            out.push(array_enum_msg("rateType"));
        }
        if !x.is_string() {
            out.push("rateType must be a string".to_owned());
        }
    }
}

/// `AdminCreateTimesheetDto`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdminCreateBody {
    #[serde(default, deserialize_with = "present")]
    pub user_uuid: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub period_start: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub period_end: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub pay_period_type: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub notes: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub pay_rate: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub rate_type: Option<Value>,
}

impl Validate for AdminCreateBody {
    fn validate(&self) -> Vec<String> {
        let mut out = Vec::new();
        let s = |v: &Option<Value>| provided(v).and_then(Value::as_str).map(str::to_owned);
        // userUuid: @IsUUID @IsNotEmpty
        let uu = s(&self.user_uuid);
        if is_empty_value(&self.user_uuid) {
            out.push("userUuid should not be empty".to_owned());
        }
        if !uu.as_deref().is_some_and(is_uuid) {
            out.push("userUuid must be a UUID".to_owned());
        }
        for (f, v) in [
            ("periodStart", &self.period_start),
            ("periodEnd", &self.period_end),
        ] {
            let x = s(v);
            if is_empty_value(v) {
                out.push(format!("{f} should not be empty"));
            }
            if !x.as_deref().is_some_and(is_iso8601) {
                out.push(format!("{f} must be a valid ISO 8601 date string"));
            }
        }
        // payPeriodType: @IsString @IsNotEmpty @IsEnum
        let pt = provided(&self.pay_period_type);
        if !enum_ok(pt, PAY_PERIOD_TYPES) {
            out.push(array_enum_msg("payPeriodType"));
        }
        if is_empty_value(&self.pay_period_type) {
            out.push("payPeriodType should not be empty".to_owned());
        }
        if !pt.is_some_and(Value::is_string) {
            out.push("payPeriodType must be a string".to_owned());
        }
        opt_string(&mut out, "notes", &self.notes);
        opt_number_min0(&mut out, "payRate", &self.pay_rate);
        opt_rate_type(&mut out, &self.rate_type);
        out
    }
}

/// Parameter query `GET /timesheets/my` (`QueryMyTimesheetsDto`).
#[derive(Debug, Default, Deserialize)]
pub struct MyQuery {
    pub status: Option<String>,
    pub year: Option<String>,
}

impl MyQuery {
    /// `parseInt(year)`; `Some(None)` = NaN (gagal validasi).
    pub fn year_value(&self) -> Option<Option<i64>> {
        self.year.as_deref().map(crate::jsfmt::parse_int)
    }
}

impl Validate for MyQuery {
    fn validate(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(s) = &self.status
            && !TIMESHEET_STATUSES.contains(&s.as_str())
        {
            out.push(enum_msg("status", TIMESHEET_STATUSES));
        }
        if let Some(None) = self.year_value() {
            out.push("year must be a number conforming to the specified constraints".to_owned());
        }
        out
    }
}

/// Parameter query `GET /timesheets` (`QueryTimesheetDto`).
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdminQuery {
    pub status: Option<String>,
    pub user_uuid: Option<String>,
    pub period_start: Option<String>,
    pub period_end: Option<String>,
}

impl Validate for AdminQuery {
    fn validate(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(s) = &self.status
            && !TIMESHEET_STATUSES.contains(&s.as_str())
        {
            out.push(enum_msg("status", TIMESHEET_STATUSES));
        }
        if let Some(u) = &self.user_uuid
            && !is_uuid(u)
        {
            out.push("userUuid must be a UUID".to_owned());
        }
        for (f, v) in [
            ("periodStart", &self.period_start),
            ("periodEnd", &self.period_end),
        ] {
            if let Some(x) = v
                && !is_iso8601(x)
            {
                out.push(format!("{f} must be a valid ISO 8601 date string"));
            }
        }
        out
    }
}

// ───────────────────────── timesheet-invoices ─────────────────────────

pub const INVOICE_STATUSES: &[&str] = &["DRAFT", "ISSUED", "PAID", "CANCELLED"];

/// `QueryTimesheetInvoiceDto`; `page`/`limit` melalui `@Type(() => Number)`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InvoiceQuery {
    pub status: Option<String>,
    pub user_uuid: Option<String>,
    pub page: Option<String>,
    pub limit: Option<String>,
}

use mz_core::jsfmt::number as js_number;

fn int_positive(out: &mut Vec<String>, f: &str, raw: &Option<String>) {
    if let Some(r) = raw {
        let n = js_number(r);
        if !(n.is_finite() && n > 0.0) {
            out.push(format!("{f} must be a positive number"));
        }
        if !(n.is_finite() && n.fract() == 0.0) {
            out.push(format!("{f} must be an integer number"));
        }
    }
}

impl InvoiceQuery {
    /// `parseInt(page) || 1`.
    pub fn page_value(&self) -> i64 {
        self.page.as_deref().map_or(1, |p| num_or(p, 1))
    }
    /// `parseInt(limit) || 20`.
    pub fn limit_value(&self) -> i64 {
        self.limit.as_deref().map_or(20, |p| num_or(p, 20))
    }
}

fn num_or(raw: &str, default: i64) -> i64 {
    let n = js_number(raw);
    if n.is_finite() && n.fract() == 0.0 && (1.0..9.0e15).contains(&n) {
        n as i64
    } else {
        default
    }
}

impl Validate for InvoiceQuery {
    fn validate(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(s) = &self.status
            && !INVOICE_STATUSES.contains(&s.as_str())
        {
            out.push(enum_msg("status", INVOICE_STATUSES));
        }
        // IsInt, IsPositive terdaftar berurutan; dekorator bawah lebih dulu (Type bukan validator).
        int_positive(&mut out, "page", &self.page);
        int_positive(&mut out, "limit", &self.limit);
        out
    }
}

/// `UpdateInvoiceStatusDto`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InvoiceStatusBody {
    #[serde(default, deserialize_with = "present")]
    pub status: Option<Value>,
}

impl InvoiceStatusBody {
    pub fn status_str(&self) -> &str {
        self.status.as_ref().and_then(Value::as_str).unwrap_or("")
    }
}

impl Validate for InvoiceStatusBody {
    fn validate(&self) -> Vec<String> {
        if matches!(self.status_str(), "PAID" | "CANCELLED") {
            Vec::new()
        } else {
            vec!["Status hanya boleh PAID atau CANCELLED".to_owned()]
        }
    }
}

// ───────────────────────── manager-summary ─────────────────────────

/// `CreateManagerSummaryConfigDto` (`required_emails`) / `UpdateManagerSummaryConfigDto`
/// (`PartialType`: semuanya opsional). Satu struct; `ManagerCreate` mewajibkan `emails`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagerConfigBody {
    #[serde(default, deserialize_with = "present")]
    pub is_daily_enabled: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub is_weekly_enabled: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub send_time: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub include_weekends: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub emails: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub user_ids: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub low_hours_threshold: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub disabled_flags: Option<Value>,
}

const MANAGER_SEND_TIME_MSG: &str = "sendTime harus format HH:00 atau HH:30 (contoh: 08:00, 08:30)";

impl ManagerConfigBody {
    pub fn validate_with(&self, required_emails: bool) -> Vec<String> {
        let mut out = Vec::new();
        opt_bool(&mut out, "isDailyEnabled", &self.is_daily_enabled);
        opt_bool(&mut out, "isWeeklyEnabled", &self.is_weekly_enabled);
        send_time(&mut out, &self.send_time, false, MANAGER_SEND_TIME_MSG);
        opt_bool(&mut out, "includeWeekends", &self.include_weekends);
        email_array(&mut out, "emails", &self.emails, required_emails);
        opt_int_array(&mut out, "userIds", &self.user_ids);
        if let Some(x) = provided(&self.low_hours_threshold) {
            if !x.as_f64().is_some_and(|n| n >= 0.0) {
                out.push("lowHoursThreshold must not be less than 0".to_owned());
            }
            if !is_int(x) {
                out.push("lowHoursThreshold must be an integer number".to_owned());
            }
        }
        opt_string_array(&mut out, "disabledFlags", &self.disabled_flags);
        out
    }
}

/// Body `POST /manager-summary/config`.
#[derive(Debug, Default, Deserialize)]
#[serde(transparent)]
pub struct ManagerCreateBody(pub ManagerConfigBody);

impl Validate for ManagerCreateBody {
    fn validate(&self) -> Vec<String> {
        self.0.validate_with(true)
    }
}

/// Body `PUT /manager-summary/config`.
#[derive(Debug, Default, Deserialize)]
#[serde(transparent)]
pub struct ManagerUpdateBody(pub ManagerConfigBody);

impl Validate for ManagerUpdateBody {
    fn validate(&self) -> Vec<String> {
        self.0.validate_with(false)
    }
}

// ───────────────────────── scheduled-reports ─────────────────────────

pub const REPORT_FREQUENCIES: &[&str] = &["DAILY", "WEEKLY", "MONTHLY", "YEARLY"];

/// `CreateScheduledReportDto` / `UpdateScheduledReportDto` (`PartialType` + `isActive`).
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduledReportBody {
    #[serde(default, deserialize_with = "present")]
    pub name: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub send_time: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub frequency: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub days: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub emails: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub subject: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub message: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub user_ids: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub project_ids: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub sections: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub is_active: Option<Value>,
}

const REPORT_SEND_TIME_MSG: &str = "sendTime must be in HH:00 or HH:30 format";

impl ScheduledReportBody {
    /// `required` = DTO create; `false` = `PartialType` (update).
    pub fn validate_with(&self, required: bool) -> Vec<String> {
        let mut out = Vec::new();
        let gate = |v: &Option<Value>| required || provided(v).is_some();
        if gate(&self.name) {
            req_string(&mut out, "name", &self.name);
        }
        if gate(&self.send_time) {
            send_time(&mut out, &self.send_time, true, REPORT_SEND_TIME_MSG);
        }
        if gate(&self.frequency) && !enum_ok(provided(&self.frequency), REPORT_FREQUENCIES) {
            out.push(enum_msg("frequency", REPORT_FREQUENCIES));
        }
        if let Some(x) = provided(&self.days) {
            match x.as_array() {
                Some(a) => {
                    if a.iter().any(|e| !e.as_f64().is_some_and(|n| n <= 6.0)) {
                        out.push("each value in days must not be greater than 6".to_owned());
                    }
                    if a.iter().any(|e| !e.as_f64().is_some_and(|n| n >= 0.0)) {
                        out.push("each value in days must not be less than 0".to_owned());
                    }
                    if a.iter().any(|e| !is_int(e)) {
                        out.push("each value in days must be an integer number".to_owned());
                    }
                }
                None => {
                    out.push("each value in days must not be greater than 6".to_owned());
                    out.push("each value in days must not be less than 0".to_owned());
                    out.push("each value in days must be an integer number".to_owned());
                    out.push("days must be an array".to_owned());
                }
            }
        }
        if gate(&self.emails) {
            email_array(&mut out, "emails", &self.emails, true);
        }
        if gate(&self.subject) {
            req_string(&mut out, "subject", &self.subject);
        }
        if gate(&self.message) {
            req_string(&mut out, "message", &self.message);
        }
        opt_int_array(&mut out, "userIds", &self.user_ids);
        opt_int_array(&mut out, "projectIds", &self.project_ids);
        opt_string_array(&mut out, "sections", &self.sections);
        opt_bool(&mut out, "isActive", &self.is_active);
        out
    }
}

/// Body `POST /scheduled-reports`.
#[derive(Debug, Default, Deserialize)]
#[serde(transparent)]
pub struct ScheduledCreateBody(pub ScheduledReportBody);

impl Validate for ScheduledCreateBody {
    fn validate(&self) -> Vec<String> {
        self.0.validate_with(true)
    }
}

/// Body `PATCH /scheduled-reports/:uuid`.
#[derive(Debug, Default, Deserialize)]
#[serde(transparent)]
pub struct ScheduledUpdateBody(pub ScheduledReportBody);

impl Validate for ScheduledUpdateBody {
    fn validate(&self) -> Vec<String> {
        self.0.validate_with(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse<T: for<'de> Deserialize<'de>>(v: Value) -> T {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn reject_requires_reason() {
        let b: RejectBody = parse(json!({}));
        assert_eq!(
            b.validate(),
            vec![
                "rejectedReason should not be empty",
                "rejectedReason must be a string"
            ]
        );
        let b: RejectBody = parse(json!({ "rejectedReason": "no" }));
        assert!(b.validate().is_empty());
        let b: RejectBody = parse(json!({ "rejectedReason": 5 }));
        assert_eq!(b.validate(), vec!["rejectedReason must be a string"]);
    }

    #[test]
    fn admin_create_messages() {
        let b: AdminCreateBody = parse(json!({}));
        let m = b.validate();
        assert!(m.contains(&"userUuid must be a UUID".to_owned()));
        assert!(m.contains(&"periodStart must be a valid ISO 8601 date string".to_owned()));
        assert!(m.contains(&"payPeriodType must be one of the following values: ".to_owned()));
        let b: AdminCreateBody = parse(json!({
            "userUuid": "ccae960e-ec50-4d1a-929a-d6d68285efb5",
            "periodStart": "2026-03-01", "periodEnd": "2026-03-31",
            "payPeriodType": "MONTHLY", "payRate": -1, "rateType": "DAILY"
        }));
        assert_eq!(
            b.validate(),
            vec![
                "payRate must not be less than 0".to_owned(),
                "rateType must be one of the following values: ".to_owned()
            ]
        );
    }

    #[test]
    fn edit_allows_null_and_absent() {
        let b: EditBody = parse(json!({ "payRate": null }));
        assert!(b.validate().is_empty());
        assert!(b.pay_rate.is_some());
        let b: EditBody = parse(json!({ "payRate": "x", "notes": 3 }));
        assert_eq!(b.validate().len(), 3);
    }

    #[test]
    fn year_query_semantics() {
        let q = MyQuery {
            status: None,
            year: Some("2026abc".into()),
        };
        assert!(q.validate().is_empty());
        assert_eq!(q.year_value(), Some(Some(2026)));
        let q = MyQuery {
            status: Some("X".into()),
            year: Some("abc".into()),
        };
        assert_eq!(q.validate().len(), 2);
    }

    #[test]
    fn iso8601_and_uuid() {
        assert!(is_uuid("ccae960e-ec50-4d1a-929a-d6d68285efb5"));
        assert!(!is_uuid("nope"));
    }

    #[test]
    fn send_time_pattern() {
        for ok in ["00:00", "08:30", "19:00", "23:30"] {
            assert!(is_send_time(ok), "{ok}");
        }
        for bad in ["24:00", "08:15", "8:00", "08:0", "28:00", "08-00"] {
            assert!(!is_send_time(bad), "{bad}");
        }
    }

    #[test]
    fn invoice_query_and_status() {
        let q = InvoiceQuery {
            page: Some("0".into()),
            limit: Some("1.5".into()),
            ..Default::default()
        };
        assert_eq!(
            q.validate(),
            vec![
                "page must be a positive number",
                "limit must be an integer number"
            ]
        );
        assert_eq!(InvoiceQuery::default().page_value(), 1);
        assert_eq!(
            InvoiceQuery {
                limit: Some("50".into()),
                ..Default::default()
            }
            .limit_value(),
            50
        );
        let b: InvoiceStatusBody = parse(json!({ "status": "DRAFT" }));
        assert_eq!(b.validate(), vec!["Status hanya boleh PAID atau CANCELLED"]);
        let b: InvoiceStatusBody = parse(json!({ "status": "PAID" }));
        assert!(b.validate().is_empty());
    }

    #[test]
    fn manager_config_validation() {
        let b: ManagerCreateBody = parse(json!({}));
        assert_eq!(
            b.validate(),
            vec![
                "each value in emails must be an email",
                "emails must contain at least 1 elements",
                "emails must be an array"
            ]
        );
        let b: ManagerCreateBody = parse(json!({
            "emails": ["a@b.co"], "sendTime": "08:15", "lowHoursThreshold": -1.5, "userIds": [1, "x"]
        }));
        assert_eq!(
            b.validate(),
            vec![
                MANAGER_SEND_TIME_MSG,
                "each value in userIds must be an integer number",
                "lowHoursThreshold must not be less than 0",
                "lowHoursThreshold must be an integer number"
            ]
        );
        let b: ManagerUpdateBody = parse(json!({ "isDailyEnabled": false }));
        assert!(b.validate().is_empty());
        let b: ManagerUpdateBody = parse(json!({ "emails": [] }));
        assert_eq!(
            b.validate(),
            vec!["emails must contain at least 1 elements"]
        );
    }

    #[test]
    fn scheduled_report_validation() {
        let b: ScheduledCreateBody = parse(json!({}));
        let m = b.validate();
        assert!(m.contains(&"name must be a string".to_owned()));
        assert!(m.contains(&REPORT_SEND_TIME_MSG.to_owned()));
        assert!(
            m.contains(
                &"frequency must be one of the following values: DAILY, WEEKLY, MONTHLY, YEARLY"
                    .to_owned()
            )
        );
        let b: ScheduledCreateBody = parse(json!({
            "name": "n", "sendTime": "09:00", "frequency": "WEEKLY", "days": [1, 7, -1],
            "emails": ["x@y.com"], "subject": "s", "message": "m"
        }));
        assert_eq!(
            b.validate(),
            vec![
                "each value in days must not be greater than 6",
                "each value in days must not be less than 0"
            ]
        );
        let b: ScheduledUpdateBody = parse(json!({ "isActive": "yes" }));
        assert_eq!(b.validate(), vec!["isActive must be a boolean value"]);
        let b: ScheduledUpdateBody = parse(json!({ "isActive": false }));
        assert!(b.validate().is_empty());
    }
}

#[cfg(test)]
mod array_enum_tests {
    use super::array_enum_msg;

    #[test]
    fn array_literal_enum_lists_no_values() {
        assert_eq!(
            array_enum_msg("rateType"),
            "rateType must be one of the following values: "
        );
    }
}
