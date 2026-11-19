//! DTO `src/users/dto/*` + `PaginatorDto`. Body dibaca sebagai objek JSON mentah lalu divalidasi
//! manual supaya pesan class-validator (termasuk URUTANNYA: dekorator dievaluasi dari bawah ke
//! atas, jadi dekorator paling bawah menghasilkan pesan paling awal) sama persis dengan Node.

use mz_core::extract::{Validate, is_email};
use mz_core::iso8601::is_iso8601;
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::model::{InviteInput, PayRateInput, UpdateUserInput};

pub type Obj = Map<String, Value>;

const NUMBER_MSG: &str = "must be a number conforming to the specified constraints";

// ── primitif validasi ────────────────────────────────────────────────────────

/// `IsOptional` menganggap `null` dan `undefined` sama-sama "tidak ada".
fn defined(v: Option<&Value>) -> Option<&Value> {
    v.filter(|v| !v.is_null())
}

/// `@IsNotEmpty()`: bukan `''`, `null`, `undefined`.
fn not_empty(v: Option<&Value>) -> bool {
    defined(v).is_some_and(|v| v.as_str() != Some(""))
}

fn is_str(v: Option<&Value>) -> bool {
    v.is_some_and(Value::is_string)
}

fn is_num(v: Option<&Value>) -> bool {
    v.is_some_and(Value::is_number)
}

fn num_of(v: Option<&Value>) -> Option<f64> {
    v.and_then(Value::as_f64)
}

/// `@IsString() @IsNotEmpty()`.
fn v_string(out: &mut Vec<String>, key: &str, v: Option<&Value>) {
    if !not_empty(v) {
        out.push(format!("{key} should not be empty"));
    }
    if !is_str(v) {
        out.push(format!("{key} must be a string"));
    }
}

/// `@IsEmail() @IsNotEmpty()`.
fn v_email(out: &mut Vec<String>, key: &str, v: Option<&Value>) {
    if !not_empty(v) {
        out.push(format!("{key} should not be empty"));
    }
    if !v.and_then(Value::as_str).is_some_and(is_email) {
        out.push(format!("{key} must be an email"));
    }
}

/// `@IsBoolean() @IsNotEmpty()`.
fn v_bool(out: &mut Vec<String>, key: &str, v: Option<&Value>) {
    if !not_empty(v) {
        out.push(format!("{key} should not be empty"));
    }
    if !v.is_some_and(Value::is_boolean) {
        out.push(format!("{key} must be a boolean value"));
    }
}

/// `@IsOptional() @IsBoolean()`.
fn v_opt_bool(out: &mut Vec<String>, key: &str, v: Option<&Value>) {
    if let Some(v) = defined(v)
        && !v.is_boolean()
    {
        out.push(format!("{key} must be a boolean value"));
    }
}

/// `@IsNumber() @IsNotEmpty()`.
fn v_number(out: &mut Vec<String>, key: &str, v: Option<&Value>) {
    if !not_empty(v) {
        out.push(format!("{key} should not be empty"));
    }
    if !is_num(v) {
        out.push(format!("{key} {NUMBER_MSG}"));
    }
}

/// `@IsNumber() @IsPositive() @IsOptional()` (pesan: IsPositive dulu).
fn v_opt_positive(out: &mut Vec<String>, key: &str, v: Option<&Value>) {
    if let Some(v) = defined(v) {
        let n = v.as_f64();
        if !n.is_some_and(|n| n > 0.0) {
            out.push(format!("{key} must be a positive number"));
        }
        if n.is_none() {
            out.push(format!("{key} {NUMBER_MSG}"));
        }
    }
}

/// `@IsDateString()`.
fn v_date_string(out: &mut Vec<String>, key: &str, v: Option<&Value>) {
    if !v.and_then(Value::as_str).is_some_and(is_iso8601) {
        out.push(format!("{key} must be a valid ISO 8601 date string"));
    }
}

fn v_enum(out: &mut Vec<String>, key: &str, v: Option<&Value>, allowed: &[&str]) {
    if !v
        .and_then(Value::as_str)
        .is_some_and(|s| allowed.contains(&s))
    {
        out.push(format!(
            "{key} must be one of the following values: {}",
            allowed.join(", ")
        ));
    }
}

fn s(o: &Obj, key: &str) -> String {
    o.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn b(o: &Obj, key: &str) -> Option<bool> {
    o.get(key).and_then(Value::as_bool)
}

/// `Number.isInteger`.
pub fn is_integer(v: &Value) -> bool {
    v.is_i64()
        || v.is_u64()
        || v.as_f64()
            .is_some_and(|f| f.is_finite() && f.fract() == 0.0)
}

/// Nilai integer dari JSON (`3` atau `3.0`); `None` bila bukan integer atau di luar `i32`.
pub fn int_of(v: Option<&Value>) -> Option<i32> {
    let v = v?;
    if !is_integer(v) {
        return None;
    }
    let n = v.as_i64().or_else(|| v.as_f64().map(|f| f as i64))?;
    i32::try_from(n).ok()
}

/// `parseInt(s, 10)`: awalan digit (dengan tanda); `None` = `NaN`.
pub fn js_parse_int(raw: &str) -> Option<i64> {
    let t = raw.trim_start();
    let mut end = 0;
    for (i, c) in t.char_indices() {
        if c.is_ascii_digit() || (i == 0 && (c == '-' || c == '+')) {
            end = i + c.len_utf8();
        } else {
            break;
        }
    }
    t.get(..end)?.parse::<i64>().ok()
}

// ── InviteUserDto ────────────────────────────────────────────────────────────

/// `InviteUserDto` — urutan properti: username, accessLevelCode, isScreenshotActive, name, generatePassword.
#[derive(Debug, Clone, Deserialize)]
#[serde(transparent)]
pub struct InviteBody(pub Obj);

impl Validate for InviteBody {
    fn validate(&self) -> Vec<String> {
        let m = &self.0;
        let mut out = Vec::new();
        v_email(&mut out, "username", m.get("username"));
        v_string(&mut out, "accessLevelCode", m.get("accessLevelCode"));
        v_bool(&mut out, "isScreenshotActive", m.get("isScreenshotActive"));
        v_string(&mut out, "name", m.get("name"));
        v_opt_bool(&mut out, "generatePassword", m.get("generatePassword"));
        out
    }
}

fn invite_input(m: &Obj, generate_password: bool) -> InviteInput {
    InviteInput {
        username: s(m, "username"),
        name: s(m, "name"),
        access_level_code: s(m, "accessLevelCode"),
        is_screenshot_active: b(m, "isScreenshotActive").unwrap_or(false),
        generate_password,
    }
}

impl InviteBody {
    pub fn input(&self) -> InviteInput {
        // `generatePassword === true` di service.
        invite_input(&self.0, b(&self.0, "generatePassword").unwrap_or(false))
    }
}

/// `InviteUserBatchDto`.
#[derive(Debug, Clone, Deserialize)]
#[serde(transparent)]
pub struct InviteBatchBody(pub Obj);

impl Validate for InviteBatchBody {
    fn validate(&self) -> Vec<String> {
        let m = &self.0;
        let mut out = Vec::new();
        // `@IsArray() @ValidateNested({ each: true })`: IsArray (custom) dulu, lalu nested.
        let users = m.get("users");
        match users.and_then(Value::as_array) {
            None => {
                out.push("users must be an array".to_owned());
                if defined(users).is_some() {
                    out.push(
                        "each value in nested property users must be either object or array"
                            .to_owned(),
                    );
                }
            }
            Some(items) => {
                for (i, item) in items.iter().enumerate() {
                    let Some(o) = item.as_object() else {
                        out.push(format!(
                            "nested property users.{i} must be either object or array"
                        ));
                        continue;
                    };
                    let mut inner = Vec::new();
                    v_email(&mut inner, "username", o.get("username"));
                    v_string(&mut inner, "name", o.get("name"));
                    v_string(&mut inner, "accessLevelCode", o.get("accessLevelCode"));
                    v_bool(
                        &mut inner,
                        "isScreenshotActive",
                        o.get("isScreenshotActive"),
                    );
                    v_opt_bool(&mut inner, "generatePassword", o.get("generatePassword"));
                    out.extend(inner.into_iter().map(|e| format!("users.{i}.{e}")));
                }
            }
        }
        v_opt_bool(&mut out, "generatePassword", m.get("generatePassword"));
        out
    }
}

impl InviteBatchBody {
    /// Item batch dengan `generatePassword` sudah di-resolve (`item ?? batch ?? false`).
    pub fn items(&self) -> Vec<InviteInput> {
        let batch = b(&self.0, "generatePassword");
        self.0
            .get("users")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_object)
                    .map(|o| invite_input(o, b(o, "generatePassword").or(batch).unwrap_or(false)))
                    .collect()
            })
            .unwrap_or_default()
    }
}

// ── DeleteAccountDto ─────────────────────────────────────────────────────────

/// `DeleteAccountDto` — `@IsString() @IsNotEmpty() @MaxLength(255)`.
#[derive(Debug, Clone, Deserialize)]
#[serde(transparent)]
pub struct DeleteAccountBody(pub Obj);

impl Validate for DeleteAccountBody {
    fn validate(&self) -> Vec<String> {
        let v = self.0.get("password");
        let mut out = Vec::new();
        let text = v.and_then(Value::as_str);
        if text.is_none_or(|t| t.encode_utf16().count() > 255) {
            out.push("password must be shorter than or equal to 255 characters".to_owned());
        }
        if !not_empty(v) {
            out.push("password should not be empty".to_owned());
        }
        if text.is_none() {
            out.push("password must be a string".to_owned());
        }
        out
    }
}

impl DeleteAccountBody {
    pub fn password(&self) -> String {
        s(&self.0, "password")
    }
}

// ── UpdateUserDto ────────────────────────────────────────────────────────────

/// `UpdateUserDto`.
#[derive(Debug, Clone, Deserialize)]
#[serde(transparent)]
pub struct UpdateUserBody(pub Obj);

impl Validate for UpdateUserBody {
    fn validate(&self) -> Vec<String> {
        let m = &self.0;
        let mut out = Vec::new();
        v_email(&mut out, "username", m.get("username"));
        v_string(&mut out, "name", m.get("name"));
        v_number(&mut out, "status", m.get("status"));
        // `@IsNumber() @IsNotEmpty() @Max(12)`: Max dulu.
        let spp = m.get("screenshotPerHour");
        if !num_of(spp).is_some_and(|n| n <= 12.0) {
            out.push("screenshotPerHour must not be greater than 12".to_owned());
        }
        v_number(&mut out, "screenshotPerHour", spp);
        v_number(&mut out, "idleTimeInSecond", m.get("idleTimeInSecond"));
        v_bool(&mut out, "isScreenshotActive", m.get("isScreenshotActive"));
        v_bool(&mut out, "isBlur", m.get("isBlur"));
        v_string(&mut out, "accessLevel", m.get("accessLevel"));
        out
    }
}

impl UpdateUserBody {
    /// `None` bila angka bukan integer (Prisma menolak -> 500 di Node).
    pub fn input(&self) -> Option<UpdateUserInput> {
        let m = &self.0;
        Some(UpdateUserInput {
            username: s(m, "username"),
            name: s(m, "name"),
            status: int_of(m.get("status"))?,
            screenshot_per_hour: int_of(m.get("screenshotPerHour"))?,
            idle_time_in_second: int_of(m.get("idleTimeInSecond"))?,
            is_screenshot_active: b(m, "isScreenshotActive").unwrap_or(false),
            is_blur: b(m, "isBlur").unwrap_or(false),
            access_level: s(m, "accessLevel"),
        })
    }
}

// ── Pay rate DTO ─────────────────────────────────────────────────────────────

pub const PAY_PERIOD_TYPES: [&str; 3] = ["WEEKLY", "BIWEEKLY", "MONTHLY"];
pub const RATE_TYPES: [&str; 2] = ["HOURLY", "FIXED"];

/// `CreatePayRateDto`.
#[derive(Debug, Clone, Deserialize)]
#[serde(transparent)]
pub struct CreatePayRateBody(pub Obj);

impl Validate for CreatePayRateBody {
    fn validate(&self) -> Vec<String> {
        validate_pay_rate(&self.0, false)
    }
}

/// `UpdatePayRateDto` (`PartialType`: semua properti ditambah `@IsOptional()`).
#[derive(Debug, Clone, Deserialize)]
#[serde(transparent)]
pub struct UpdatePayRateBody(pub Obj);

impl Validate for UpdatePayRateBody {
    fn validate(&self) -> Vec<String> {
        validate_pay_rate(&self.0, true)
    }
}

fn validate_pay_rate(m: &Obj, partial: bool) -> Vec<String> {
    let mut out = Vec::new();
    let skip = |k: &str| partial && defined(m.get(k)).is_none();

    if !skip("payPeriodType") {
        let v = m.get("payPeriodType");
        if !not_empty(v) {
            out.push("payPeriodType should not be empty".to_owned());
        }
        v_enum(&mut out, "payPeriodType", v, &PAY_PERIOD_TYPES);
    }
    if !skip("timesheetApproval") {
        v_bool(&mut out, "timesheetApproval", m.get("timesheetApproval"));
    }
    v_opt_positive(&mut out, "payRate", m.get("payRate"));
    if !skip("rateType") {
        let v = m.get("rateType");
        if !not_empty(v) {
            out.push("rateType should not be empty".to_owned());
        }
        v_enum(&mut out, "rateType", v, &RATE_TYPES);
    }
    if !skip("effectiveDate") {
        let v = m.get("effectiveDate");
        if !not_empty(v) {
            out.push("effectiveDate should not be empty".to_owned());
        }
        v_date_string(&mut out, "effectiveDate", v);
    }
    if let Some(v) = defined(m.get("notes"))
        && !v.is_string()
    {
        out.push("notes must be a string".to_owned());
    }
    out
}

fn pay_rate_input(m: &Obj) -> PayRateInput {
    PayRateInput {
        pay_period_type: m
            .get("payPeriodType")
            .and_then(Value::as_str)
            .map(str::to_owned),
        timesheet_approval: m.get("timesheetApproval").map(Value::as_bool),
        pay_rate: m.get("payRate").map(Value::as_f64),
        rate_type: m.get("rateType").and_then(Value::as_str).map(str::to_owned),
        effective_date: m
            .get("effectiveDate")
            .and_then(Value::as_str)
            .map(str::to_owned),
        notes: m.get("notes").map(|v| v.as_str().map(str::to_owned)),
    }
}

impl CreatePayRateBody {
    pub fn input(&self) -> PayRateInput {
        pay_rate_input(&self.0)
    }
}

impl UpdatePayRateBody {
    pub fn input(&self) -> PayRateInput {
        pay_rate_input(&self.0)
    }
}

/// `SetProjectRateDto`.
#[derive(Debug, Clone, Deserialize)]
#[serde(transparent)]
pub struct SetProjectRateBody(pub Obj);

impl Validate for SetProjectRateBody {
    fn validate(&self) -> Vec<String> {
        let mut out = Vec::new();
        v_opt_positive(&mut out, "payRate", self.0.get("payRate"));
        v_opt_positive(&mut out, "billRate", self.0.get("billRate"));
        out
    }
}

impl SetProjectRateBody {
    pub fn pay_rate(&self) -> Option<f64> {
        num_of(self.0.get("payRate"))
    }
    pub fn bill_rate(&self) -> Option<f64> {
        num_of(self.0.get("billRate"))
    }
}

// ── UpsertWorkTimeLimitDto ───────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(transparent)]
pub struct WorkTimeLimitBody(pub Obj);

/// `@IsOptional() @IsInt() @Min(0) @Max(max)`: pesan Max, Min, IsInt.
fn v_limit(out: &mut Vec<String>, key: &str, v: Option<&Value>, max: f64) {
    let Some(v) = defined(v) else { return };
    let n = v.as_f64();
    if !n.is_some_and(|n| n <= max) {
        out.push(format!("{key} must not be greater than {}", max as i64));
    }
    if !n.is_some_and(|n| n >= 0.0) {
        out.push(format!("{key} must not be less than 0"));
    }
    if !is_integer(v) {
        out.push(format!("{key} must be an integer number"));
    }
}

impl Validate for WorkTimeLimitBody {
    fn validate(&self) -> Vec<String> {
        let mut out = Vec::new();
        v_limit(
            &mut out,
            "dailyLimitSec",
            self.0.get("dailyLimitSec"),
            86_400.0,
        );
        v_limit(
            &mut out,
            "weeklyLimitSec",
            self.0.get("weeklyLimitSec"),
            604_800.0,
        );
        out
    }
}

impl WorkTimeLimitBody {
    pub fn daily(&self) -> Option<i32> {
        int_of(defined(self.0.get("dailyLimitSec")))
    }
    pub fn weekly(&self) -> Option<i32> {
        int_of(defined(self.0.get("weeklyLimitSec")))
    }
}

// ── UpdateProductTourDto ─────────────────────────────────────────────────────

pub const PRODUCT_TOUR_STEP_IDS: [&str; 19] = [
    "dashboard-welcome",
    "dashboard-calendar",
    "dashboard-calendar-progress",
    "dashboard-total-time",
    "dashboard-employee-filter",
    "download-intro",
    "download-page",
    "employee-menu",
    "employee-invite-button",
    "employee-invite-form",
    "project-menu",
    "project-create-button",
    "project-create-form",
    "project-budget-tab",
    "project-budget-form",
    "manual-time-button",
    "manual-time-form",
    "report-menu",
    "integrations-menu",
];

pub const PRODUCT_TOUR_INPUT_STATUSES: [&str; 3] = ["IN_PROGRESS", "COMPLETED", "SKIPPED"];

#[derive(Debug, Clone, Deserialize)]
#[serde(transparent)]
pub struct ProductTourBody(pub Obj);

impl Validate for ProductTourBody {
    fn validate(&self) -> Vec<String> {
        let mut out = Vec::new();
        v_enum(
            &mut out,
            "status",
            self.0.get("status"),
            &PRODUCT_TOUR_INPUT_STATUSES,
        );
        // `@IsOptional() @IsString() @MaxLength(64) @IsIn(...)`: IsIn, MaxLength, IsString.
        if let Some(v) = defined(self.0.get("step")) {
            let text = v.as_str();
            v_enum(&mut out, "step", Some(v), &PRODUCT_TOUR_STEP_IDS);
            if text.is_none_or(|t| t.encode_utf16().count() > 64) {
                out.push("step must be shorter than or equal to 64 characters".to_owned());
            }
            if text.is_none() {
                out.push("step must be a string".to_owned());
            }
        }
        out
    }
}

impl ProductTourBody {
    pub fn status(&self) -> String {
        s(&self.0, "status")
    }
    pub fn step(&self) -> Option<String> {
        defined(self.0.get("step"))
            .and_then(Value::as_str)
            .map(str::to_owned)
    }
}

// ── UpsertLocalDbDto / CreateUserLogDto ──────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(transparent)]
pub struct LocalDbBody(pub Obj);

impl Validate for LocalDbBody {
    fn validate(&self) -> Vec<String> {
        let m = &self.0;
        let mut out = Vec::new();
        let date = m.get("date");
        if !not_empty(date) {
            out.push("date should not be empty".to_owned());
        }
        v_date_string(&mut out, "date", date);
        let data = m.get("data");
        if !not_empty(data) {
            out.push("data should not be empty".to_owned());
        }
        if !data.is_some_and(Value::is_object) {
            out.push("data must be an object".to_owned());
        }
        out
    }
}

impl LocalDbBody {
    pub fn date(&self) -> String {
        s(&self.0, "date")
    }
    pub fn data(&self) -> Value {
        self.0.get("data").cloned().unwrap_or(Value::Null)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(transparent)]
pub struct UserLogBody(pub Obj);

impl Validate for UserLogBody {
    fn validate(&self) -> Vec<String> {
        let m = &self.0;
        let mut out = Vec::new();
        let date = m.get("date");
        if !not_empty(date) {
            out.push("date should not be empty".to_owned());
        }
        v_date_string(&mut out, "date", date);
        v_string(&mut out, "content", m.get("content"));
        out
    }
}

impl UserLogBody {
    pub fn date(&self) -> String {
        s(&self.0, "date")
    }
    pub fn content(&self) -> String {
        s(&self.0, "content")
    }
}

// ── PaginatorDto (query) ─────────────────────────────────────────────────────

/// `PaginatorDto`: `@Transform` mengubah string angka lewat `parseInt`, `@IsNumber` menolak `NaN`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PaginatorQuery {
    pub page_number: Option<String>,
    pub items_per_page: Option<String>,
    pub order_by: Option<String>,
    pub order_column: Option<String>,
    pub search: Option<String>,
}

impl Validate for PaginatorQuery {
    fn validate(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (key, v) in [
            ("pageNumber", &self.page_number),
            ("itemsPerPage", &self.items_per_page),
        ] {
            if let Some(raw) = v
                && js_parse_int(raw).is_none()
            {
                out.push(format!("{key} {NUMBER_MSG}"));
            }
        }
        out
    }
}

impl PaginatorQuery {
    pub fn page_number_value(&self) -> Option<i64> {
        self.page_number.as_deref().and_then(js_parse_int)
    }
    pub fn items_per_page_value(&self) -> Option<i64> {
        self.items_per_page.as_deref().and_then(js_parse_int)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(v: Value) -> Obj {
        v.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn invite_missing_fields_use_class_validator_order() {
        let errs = InviteBody(obj(json!({}))).validate();
        assert_eq!(
            errs,
            vec![
                "username should not be empty",
                "username must be an email",
                "accessLevelCode should not be empty",
                "accessLevelCode must be a string",
                "isScreenshotActive should not be empty",
                "isScreenshotActive must be a boolean value",
                "name should not be empty",
                "name must be a string",
            ]
        );
    }

    #[test]
    fn invite_valid_and_generate_password_type() {
        let ok = InviteBody(obj(json!({
            "username": "a@b.co", "accessLevelCode": "STAFF",
            "isScreenshotActive": true, "name": "A", "generatePassword": true
        })));
        assert!(ok.validate().is_empty());
        assert!(ok.input().generate_password);
        let bad = InviteBody(obj(json!({
            "username": "a@b.co", "accessLevelCode": "STAFF",
            "isScreenshotActive": true, "name": "A", "generatePassword": "yes"
        })));
        assert_eq!(
            bad.validate(),
            vec!["generatePassword must be a boolean value"]
        );
    }

    #[test]
    fn batch_prefixes_nested_errors_and_resolves_generate_password() {
        let body = InviteBatchBody(obj(json!({
            "generatePassword": true,
            "users": [
                {"username": "a@b.co", "name": "A", "accessLevelCode": "STAFF", "isScreenshotActive": true},
                {"username": "c@d.co", "name": "C", "accessLevelCode": "STAFF", "isScreenshotActive": false, "generatePassword": false},
            ]
        })));
        assert!(body.validate().is_empty());
        let items = body.items();
        assert_eq!(items.len(), 2);
        assert_eq!(items.first().map(|i| i.generate_password), Some(true));
        assert_eq!(items.get(1).map(|i| i.generate_password), Some(false));

        let bad = InviteBatchBody(obj(json!({ "users": [ { "username": "x" } ] })));
        let errs = bad.validate();
        assert_eq!(
            errs.first().map(String::as_str),
            Some("users.0.username must be an email")
        );
        assert!(errs.iter().all(|e| e.starts_with("users.0.")));

        assert_eq!(
            InviteBatchBody(obj(json!({}))).validate(),
            vec!["users must be an array"]
        );
        assert_eq!(
            InviteBatchBody(obj(json!({ "users": "x" }))).validate(),
            vec![
                "users must be an array",
                "each value in nested property users must be either object or array"
            ]
        );
    }

    #[test]
    fn update_user_max_and_types() {
        let errs = UpdateUserBody(obj(json!({
            "username": "a@b.co", "name": "A", "status": 1, "screenshotPerHour": 13,
            "idleTimeInSecond": 180, "isScreenshotActive": true, "isBlur": false, "accessLevel": "STAFF"
        })))
        .validate();
        assert_eq!(errs, vec!["screenshotPerHour must not be greater than 12"]);
    }

    #[test]
    fn update_user_rejects_fractional_numbers_for_prisma_int() {
        let body = UpdateUserBody(obj(json!({
            "username": "a@b.co", "name": "A", "status": 1.5, "screenshotPerHour": 3,
            "idleTimeInSecond": 180, "isScreenshotActive": true, "isBlur": false, "accessLevel": "STAFF"
        })));
        assert!(body.validate().is_empty());
        assert!(body.input().is_none());
    }

    #[test]
    fn pay_rate_create_messages() {
        let errs = CreatePayRateBody(obj(json!({ "payRate": -1, "rateType": "DAILY" }))).validate();
        assert_eq!(
            errs,
            vec![
                "payPeriodType should not be empty",
                "payPeriodType must be one of the following values: WEEKLY, BIWEEKLY, MONTHLY",
                "timesheetApproval should not be empty",
                "timesheetApproval must be a boolean value",
                "payRate must be a positive number",
                "rateType must be one of the following values: HOURLY, FIXED",
                "effectiveDate should not be empty",
                "effectiveDate must be a valid ISO 8601 date string",
            ]
        );
    }

    #[test]
    fn pay_rate_update_is_partial_and_keeps_null() {
        let body = UpdatePayRateBody(obj(json!({ "payRate": null, "notes": null })));
        assert!(body.validate().is_empty());
        let input = body.input();
        assert_eq!(input.pay_rate, Some(None));
        assert_eq!(input.notes, Some(None));
        assert_eq!(input.pay_period_type, None);
    }

    #[test]
    fn work_time_limit_bounds() {
        let errs = WorkTimeLimitBody(obj(
            json!({ "dailyLimitSec": 90000.5, "weeklyLimitSec": -1 }),
        ))
        .validate();
        assert_eq!(
            errs,
            vec![
                "dailyLimitSec must not be greater than 86400",
                "dailyLimitSec must be an integer number",
                "weeklyLimitSec must not be less than 0",
            ]
        );
        assert!(
            WorkTimeLimitBody(obj(json!({ "dailyLimitSec": null })))
                .validate()
                .is_empty()
        );
    }

    #[test]
    fn product_tour_step_messages() {
        let errs = ProductTourBody(obj(json!({ "status": "DONE", "step": 5 }))).validate();
        assert_eq!(errs.len(), 4);
        assert_eq!(
            errs.first().map(String::as_str),
            Some("status must be one of the following values: IN_PROGRESS, COMPLETED, SKIPPED")
        );
        assert_eq!(
            errs.last().map(String::as_str),
            Some("step must be a string")
        );
        assert!(
            ProductTourBody(obj(
                json!({ "status": "IN_PROGRESS", "step": "report-menu" })
            ))
            .validate()
            .is_empty()
        );
    }

    #[test]
    fn local_db_and_log() {
        assert_eq!(
            LocalDbBody(obj(json!({ "date": "nope", "data": [] }))).validate(),
            vec![
                "date must be a valid ISO 8601 date string",
                "data must be an object"
            ]
        );
        assert!(
            LocalDbBody(obj(json!({ "date": "2024-01-15", "data": {} })))
                .validate()
                .is_empty()
        );
        assert_eq!(
            UserLogBody(obj(json!({ "date": "2024-01-15" }))).validate(),
            vec!["content should not be empty", "content must be a string"]
        );
    }

    #[test]
    fn delete_account_password() {
        assert_eq!(
            DeleteAccountBody(obj(json!({}))).validate(),
            vec![
                "password must be shorter than or equal to 255 characters",
                "password should not be empty",
                "password must be a string"
            ]
        );
        assert!(
            DeleteAccountBody(obj(json!({ "password": "x" })))
                .validate()
                .is_empty()
        );
    }

    #[test]
    fn iso_dates() {
        for ok in [
            "2024-01-15",
            "2024-01-15T10:00:00Z",
            "2024-01-15T10:00:00.123Z",
            "2024-01-15T10:00:00+07:00",
            "2024-01-15 10:00",
            "20240115",
            "2024-01",
            "2024",
        ] {
            assert!(is_iso8601(ok), "{ok}");
        }
        for bad in [
            "",
            "nope",
            "2024-13-01",
            "2024-01-32",
            "2024-1-5",
            "2024-01-15T25:00",
            "15/01/2024",
        ] {
            assert!(!is_iso8601(bad), "{bad}");
        }
    }

    #[test]
    fn parse_int_prefix() {
        assert_eq!(js_parse_int("12abc"), Some(12));
        assert_eq!(js_parse_int("abc"), None);
        assert_eq!(js_parse_int(""), None);
        assert_eq!(js_parse_int("-3"), Some(-3));
    }

    #[test]
    fn paginator_rejects_nan() {
        let q = PaginatorQuery {
            page_number: Some("abc".into()),
            items_per_page: Some("10".into()),
            ..Default::default()
        };
        assert_eq!(
            q.validate(),
            vec!["pageNumber must be a number conforming to the specified constraints"]
        );
    }
}
