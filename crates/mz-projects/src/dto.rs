//! DTO Nest (`CreateProjectDto`, `UpdateProjectDto`, `UpdateProjectHourDto`,
//! `UpsertProjectBudgetDto`, `PaginatorDto`). Body dibaca sebagai objek JSON mentah
//! lalu divalidasi manual supaya pesan class-validator (termasuk urutannya:
//! dekorator dievaluasi dari bawah ke atas) sama persis.

use mz_core::extract::Validate;
use mz_core::iso8601::is_iso8601;
use mz_core::time::parse_js_date;
use mz_core::uuid_rules::is_uuid;
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::jsfmt;

const NUMBER_MSG: &str = "must be a number conforming to the specified constraints";

fn defined(v: Option<&Value>) -> Option<&Value> {
    v.filter(|v| !v.is_null())
}

fn not_empty(v: Option<&Value>) -> bool {
    defined(v).is_some_and(|v| v.as_str() != Some(""))
}

fn is_number(v: Option<&Value>) -> bool {
    v.is_some_and(Value::is_number)
}

fn string_prop(out: &mut Vec<String>, key: &str, v: Option<&Value>, max: usize) {
    let s = v.and_then(Value::as_str);
    if s.is_none_or(|s| s.encode_utf16().count() > max) {
        out.push(format!(
            "{key} must be shorter than or equal to {max} characters"
        ));
    }
    if !not_empty(v) {
        out.push(format!("{key} should not be empty"));
    }
    if s.is_none() {
        out.push(format!("{key} must be a string"));
    }
}

fn number_prop(out: &mut Vec<String>, key: &str, v: Option<&Value>) {
    if !not_empty(v) {
        out.push(format!("{key} should not be empty"));
    }
    if !is_number(v) {
        out.push(format!("{key} {NUMBER_MSG}"));
    }
}

/// Validasi `CreateProjectDto`; `partial` = `PartialType` (`UpdateProjectDto`).
pub fn validate_project(m: &Map<String, Value>, partial: bool) -> Vec<String> {
    let mut out = Vec::new();
    let skip = |k: &str| partial && defined(m.get(k)).is_none();

    if !skip("code") {
        string_prop(&mut out, "code", m.get("code"), 6);
    }
    if !skip("name") {
        string_prop(&mut out, "name", m.get("name"), 50);
    }
    if !skip("screenshotPerHour") {
        number_prop(&mut out, "screenshotPerHour", m.get("screenshotPerHour"));
    }
    if !skip("projectTypeCode") {
        let v = m.get("projectTypeCode");
        if !not_empty(v) {
            out.push("projectTypeCode should not be empty".into());
        }
        if v.and_then(Value::as_str).is_none() {
            out.push("projectTypeCode must be a string".into());
        }
    }
    if !skip("startDate") {
        let v = m.get("startDate");
        if !not_empty(v) {
            out.push("startDate should not be empty".into());
        }
        if !v.and_then(Value::as_str).is_some_and(is_iso8601) {
            out.push("startDate must be a valid ISO 8601 date string".into());
        }
    }
    if !skip("usersUserName") {
        let v = m.get("usersUserName");
        if !not_empty(v) {
            out.push("usersUserName should not be empty".into());
        }
        if !v.and_then(Value::as_array).is_some_and(|a| !a.is_empty()) {
            out.push("usersUserName must contain at least 1 elements".into());
        }
        if !v.is_some_and(Value::is_array) {
            out.push("usersUserName must be an array".into());
        }
    }
    if !skip("clientsCode") {
        let v = m.get("clientsCode");
        if !v.is_some_and(Value::is_array) {
            out.push("clientsCode must contain at least 0 elements".into());
            out.push("clientsCode must be an array".into());
        }
    }
    if !skip("totalHour") {
        number_prop(&mut out, "totalHour", m.get("totalHour"));
    }
    if let Some(v) = defined(m.get("billable"))
        && !v.is_boolean()
    {
        out.push("billable must be a boolean value".into());
    }
    if let Some(v) = defined(m.get("budget")) {
        match v.as_object() {
            Some(b) => out.extend(
                validate_budget(b)
                    .into_iter()
                    .map(|e| format!("budget.{e}")),
            ),
            None => out.push("nested property budget must be either object or array".into()),
        }
    }
    if let Some(v) = defined(m.get("manualTimeApprovers")) {
        let bad = match v.as_array() {
            Some(a) => a.iter().any(|x| !x.as_str().is_some_and(is_uuid)),
            None => true,
        };
        if bad {
            out.push("each value in manualTimeApprovers must be a UUID".into());
        }
        if !v.is_array() {
            out.push("manualTimeApprovers must be an array".into());
        }
    }
    out
}

/// Validasi `UpsertProjectBudgetDto` (nama properti tanpa prefix).
pub fn validate_budget(m: &Map<String, Value>) -> Vec<String> {
    let mut out = Vec::new();
    if !m.get("isEnabled").is_some_and(Value::is_boolean) {
        out.push("isEnabled must be a boolean value".into());
    }
    let enum_prop = |out: &mut Vec<String>, key: &str, allowed: &[&str]| {
        if let Some(v) = defined(m.get(key))
            && !v.as_str().is_some_and(|s| allowed.contains(&s))
        {
            out.push(format!(
                "{key} must be one of the following values: {}",
                allowed.join(", ")
            ));
        }
    };
    enum_prop(&mut out, "budgetType", &["HOURS", "COST"]);
    for key in ["budgetHours", "budgetAmount", "billableRate"] {
        if let Some(v) = defined(m.get(key)) {
            if !v.as_f64().is_some_and(|n| n >= 0.0) {
                out.push(format!("{key} must not be less than 0"));
            }
            if !v.is_number() {
                out.push(format!("{key} {NUMBER_MSG}"));
            }
        }
    }
    if let Some(v) = defined(m.get("countNonBillable"))
        && !v.is_boolean()
    {
        out.push("countNonBillable must be a boolean value".into());
    }
    enum_prop(&mut out, "resetCycle", &["NEVER", "WEEKLY", "MONTHLY"]);
    if let Some(v) = defined(m.get("alertThreshold")) {
        let n = v.as_f64();
        if !n.is_some_and(|n| n <= 100.0) {
            out.push("alertThreshold must not be greater than 100".into());
        }
        if !n.is_some_and(|n| n >= 1.0) {
            out.push("alertThreshold must not be less than 1".into());
        }
        if !n.is_some_and(|n| n.fract() == 0.0) {
            out.push("alertThreshold must be an integer number".into());
        }
    }
    enum_prop(&mut out, "enforcement", &["ALERT_ONLY", "STOP_TRACKING"]);
    out
}

/// `UpsertProjectBudgetDto` setelah lolos validasi.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BudgetInput {
    pub is_enabled: bool,
    pub budget_type: Option<String>,
    pub budget_hours: Option<f64>,
    pub budget_amount: Option<f64>,
    pub billable_rate: Option<f64>,
    pub count_non_billable: Option<bool>,
    pub reset_cycle: Option<String>,
    pub alert_threshold: Option<i32>,
    pub enforcement: Option<String>,
}

impl BudgetInput {
    pub fn from_map(m: &Map<String, Value>) -> Self {
        let s = |k: &str| defined(m.get(k)).and_then(Value::as_str).map(str::to_owned);
        let f = |k: &str| defined(m.get(k)).and_then(Value::as_f64);
        Self {
            is_enabled: m.get("isEnabled").and_then(Value::as_bool).unwrap_or(false),
            budget_type: s("budgetType"),
            budget_hours: f("budgetHours"),
            budget_amount: f("budgetAmount"),
            billable_rate: f("billableRate"),
            count_non_billable: defined(m.get("countNonBillable")).and_then(Value::as_bool),
            reset_cycle: s("resetCycle"),
            alert_threshold: f("alertThreshold").and_then(jsfmt::as_i32),
            enforcement: s("enforcement"),
        }
    }
}

/// `CreateProjectDto` / `UpdateProjectDto` setelah lolos validasi.
#[derive(Debug, Clone, Default)]
pub struct ProjectInput {
    pub code: Option<String>,
    pub name: Option<String>,
    pub screenshot_per_hour: Option<f64>,
    pub project_type_code: Option<String>,
    pub start_date: Option<String>,
    pub users_user_name: Option<Vec<String>>,
    pub clients_code: Option<Vec<String>>,
    pub total_hour: Option<f64>,
    pub billable: Option<bool>,
    pub budget: Option<BudgetInput>,
    pub manual_time_approvers: Option<Vec<String>>,
}

impl ProjectInput {
    pub fn from_map(m: &Map<String, Value>) -> Self {
        let s = |k: &str| defined(m.get(k)).and_then(Value::as_str).map(str::to_owned);
        let f = |k: &str| defined(m.get(k)).and_then(Value::as_f64);
        let list = |k: &str| {
            defined(m.get(k)).and_then(Value::as_array).map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_owned))
                    .collect::<Vec<_>>()
            })
        };
        Self {
            code: s("code"),
            name: s("name"),
            screenshot_per_hour: f("screenshotPerHour"),
            project_type_code: s("projectTypeCode"),
            start_date: s("startDate"),
            users_user_name: list("usersUserName"),
            clients_code: list("clientsCode"),
            total_hour: f("totalHour"),
            billable: defined(m.get("billable")).and_then(Value::as_bool),
            budget: defined(m.get("budget"))
                .and_then(Value::as_object)
                .map(BudgetInput::from_map),
            manual_time_approvers: list("manualTimeApprovers"),
        }
    }

    /// `new Date(startDate)` untuk kolom `@db.Date`.
    pub fn start_date_value(&self) -> Option<chrono::NaiveDate> {
        self.start_date
            .as_deref()
            .and_then(parse_js_date)
            .map(|d| d.date())
    }
}

/// Body `POST /projects`.
#[derive(Debug, Clone, Deserialize)]
#[serde(transparent)]
pub struct CreateProjectBody(pub Map<String, Value>);

impl Validate for CreateProjectBody {
    fn validate(&self) -> Vec<String> {
        validate_project(&self.0, false)
    }
}

/// Body `PATCH /projects/:uuid` (`PartialType(CreateProjectDto)`).
#[derive(Debug, Clone, Deserialize)]
#[serde(transparent)]
pub struct UpdateProjectBody(pub Map<String, Value>);

impl Validate for UpdateProjectBody {
    fn validate(&self) -> Vec<String> {
        validate_project(&self.0, true)
    }
}

/// `UpdateProjectHourDto`.
#[derive(Debug, Clone, Deserialize)]
#[serde(transparent)]
pub struct ProjectHourBody(pub Map<String, Value>);

impl ProjectHourBody {
    pub fn total_hour(&self) -> Option<f64> {
        self.0.get("totalHour").and_then(Value::as_f64)
    }
}

impl Validate for ProjectHourBody {
    fn validate(&self) -> Vec<String> {
        let mut out = Vec::new();
        number_prop(&mut out, "totalHour", self.0.get("totalHour"));
        out
    }
}

/// `PaginatorDto` (query string).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PaginatorQuery {
    pub page_number: Option<String>,
    pub items_per_page: Option<String>,
    pub order_by: Option<String>,
    pub order_column: Option<String>,
    pub search: Option<String>,
}

impl PaginatorQuery {
    pub fn page_number(&self) -> Option<i64> {
        self.page_number.as_deref().and_then(jsfmt::parse_int)
    }
    pub fn items_per_page(&self) -> Option<i64> {
        self.items_per_page.as_deref().and_then(jsfmt::parse_int)
    }
}

impl Validate for PaginatorQuery {
    fn validate(&self) -> Vec<String> {
        let mut out = Vec::new();
        // `@Transform(parseInt)` lalu `@IsNumber()`: string tak numerik → NaN → gagal.
        if self.page_number.is_some() && self.page_number().is_none() {
            out.push(format!("pageNumber {NUMBER_MSG}"));
        }
        if self.items_per_page.is_some() && self.items_per_page().is_none() {
            out.push(format!("itemsPerPage {NUMBER_MSG}"));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap_or_default()
    }

    fn valid() -> Value {
        json!({
            "code": "ABC", "name": "Proj", "screenshotPerHour": 3, "projectTypeCode": "ROLLING_HOURS",
            "startDate": "2026-11-16", "usersUserName": ["a@b.co"], "clientsCode": [], "totalHour": 40
        })
    }

    #[test]
    fn valid_body_has_no_errors() {
        assert!(validate_project(&obj(valid()), false).is_empty());
    }

    #[test]
    fn empty_body_lists_messages_in_class_validator_order() {
        let errs = validate_project(&Map::new(), false);
        assert_eq!(
            errs[0],
            "code must be shorter than or equal to 6 characters"
        );
        assert_eq!(errs[1], "code should not be empty");
        assert_eq!(errs[2], "code must be a string");
        assert!(errs.contains(&"usersUserName must be an array".to_string()));
        assert!(errs.contains(&"clientsCode must be an array".to_string()));
        assert!(!errs.iter().any(|e| e.starts_with("billable")));
    }

    #[test]
    fn partial_skips_missing_but_checks_present() {
        assert!(validate_project(&Map::new(), true).is_empty());
        let errs = validate_project(&obj(json!({ "code": "TOOLONG1" })), true);
        assert_eq!(
            errs,
            vec!["code must be shorter than or equal to 6 characters"]
        );
    }

    #[test]
    fn nested_budget_is_prefixed() {
        let mut v = valid();
        v["budget"] = json!({ "isEnabled": "yes", "alertThreshold": 150, "budgetType": "X" });
        let errs = validate_project(&obj(v), false);
        assert!(errs.contains(&"budget.isEnabled must be a boolean value".to_string()));
        assert!(errs.contains(&"budget.alertThreshold must not be greater than 100".to_string()));
        assert!(errs.contains(
            &"budget.budgetType must be one of the following values: HOURS, COST".to_string()
        ));
    }

    #[test]
    fn approvers_must_be_uuids() {
        let mut v = valid();
        v["manualTimeApprovers"] = json!(["nope"]);
        assert_eq!(
            validate_project(&obj(v), false),
            vec!["each value in manualTimeApprovers must be a UUID"]
        );
        assert!(is_uuid("ccae960e-ec50-4d1a-929a-d6d68285efb5"));
        assert!(!is_uuid("ccae960eec504d1a929ad6d68285efb5"));
        // `@IsUUID('all')`: bentuk benar tapi digit versi 0 ditolak (beda dengan ParseUUIDPipe).
        assert!(!is_uuid("ccae960e-ec50-0d1a-929a-d6d68285efb5"));
    }

    #[test]
    fn date_strings() {
        assert!(is_iso8601("2026-11-16"));
        assert!(is_iso8601("2026-11-16T08:00:00.000Z"));
        assert!(is_iso8601("2026-11-16T08:00:00+07:00"));
        assert!(!is_iso8601("16/11/2026"));
        assert!(!is_iso8601("2026-13-01"));
    }

    #[test]
    fn hour_body_and_paginator() {
        assert!(
            ProjectHourBody(obj(json!({ "totalHour": 5 })))
                .validate()
                .is_empty()
        );
        assert_eq!(
            ProjectHourBody(Map::new()).validate(),
            vec![
                "totalHour should not be empty",
                "totalHour must be a number conforming to the specified constraints"
            ]
        );
        let q = PaginatorQuery {
            page_number: Some("x".into()),
            ..Default::default()
        };
        assert_eq!(q.validate().len(), 1);
        let q = PaginatorQuery {
            page_number: Some("2".into()),
            ..Default::default()
        };
        assert!(q.validate().is_empty());
    }

    #[test]
    fn input_extraction() {
        let p = ProjectInput::from_map(&obj(valid()));
        assert_eq!(p.code.as_deref(), Some("ABC"));
        assert_eq!(p.total_hour, Some(40.0));
        assert_eq!(
            p.start_date_value().map(|d| d.to_string()).as_deref(),
            Some("2026-11-16")
        );
        assert!(p.budget.is_none());
    }
}
