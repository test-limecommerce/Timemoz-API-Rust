//! DTO `reports/dto/*.ts` dengan pesan class-validator.

use mz_core::extract::Validate;
use mz_core::iso8601::is_iso8601;
use serde::Deserialize;
use serde_json::Value;

use crate::util::{is_empty, is_nullish, present};

/// `QueryReportDto` + parameter `user` / `project` (daftar id dipisah spasi dari frontend).
#[derive(Debug, Default, Deserialize)]
pub struct QueryReportDto {
    #[serde(default, rename = "userUuid")]
    pub user_uuid: Option<String>,
    #[serde(default, rename = "projectUuid")]
    pub project_uuid: Option<String>,
    #[serde(default, rename = "startAt")]
    start_at: Option<String>,
    #[serde(default, rename = "endAt")]
    end_at: Option<String>,
    #[serde(default, rename = "billableFilter")]
    pub billable_filter: Option<String>,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
}

impl QueryReportDto {
    pub fn start_at(&self) -> &str {
        self.start_at.as_deref().unwrap_or("")
    }
    pub fn end_at(&self) -> &str {
        self.end_at.as_deref().unwrap_or("")
    }
}

impl Validate for QueryReportDto {
    fn validate(&self) -> Vec<String> {
        let mut errs = Vec::new();
        for (name, v) in [
            ("userUuid", &self.user_uuid),
            ("projectUuid", &self.project_uuid),
        ] {
            if v.as_deref()
                .is_some_and(|s| !mz_core::uuid_rules::is_uuid(s))
            {
                errs.push(format!("{name} must be a UUID"));
            }
        }
        for (name, v) in [("startAt", &self.start_at), ("endAt", &self.end_at)] {
            let s = v.as_deref();
            if s.is_none_or(str::is_empty) {
                errs.push(format!("{name} should not be empty"));
            }
            if !s.is_some_and(is_iso8601) {
                errs.push(format!("{name} must be a valid ISO 8601 date string"));
            }
        }
        if let Some(b) = self.billable_filter.as_deref()
            && !matches!(b, "all" | "billable" | "non-billable")
        {
            errs.push(
                "billableFilter must be one of the following values: all, billable, non-billable"
                    .into(),
            );
        }
        errs
    }
}

/// `Number(x)` JS untuk nilai JSON (`NaN` -> `None`).
fn js_number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => {
            let t = s.trim();
            if t.is_empty() {
                Some(0.0)
            } else {
                t.parse::<f64>().ok()
            }
        }
        Value::Bool(b) => Some(f64::from(u8::from(*b))),
        Value::Null => Some(0.0),
        _ => None,
    }
}

const MAX_IDS: usize = 1000;

/// `@IsArray() @ArrayMaxSize(1000) @Type(() => Number) @IsInt({each}) @Min(1, {each})`.
fn check_id_array(errs: &mut Vec<String>, name: &str, v: Option<&Value>) {
    let arr = match v {
        Some(Value::Array(a)) => Some(a),
        _ => None,
    };
    let nums: Option<Vec<Option<f64>>> = arr.map(|a| a.iter().map(js_number).collect());
    let min_ok = nums
        .as_ref()
        .is_some_and(|n| n.iter().all(|x| x.is_some_and(|f| f >= 1.0)));
    let int_ok = nums
        .as_ref()
        .is_some_and(|n| n.iter().all(|x| x.is_some_and(|f| f.fract() == 0.0)));
    // Dekorator bawah dievaluasi lebih dulu: Min, IsInt, ArrayMaxSize, IsArray.
    if !min_ok {
        errs.push(format!("each value in {name} must not be less than 1"));
    }
    if !int_ok {
        errs.push(format!("each value in {name} must be an integer number"));
    }
    if !arr.is_some_and(|a| a.len() <= MAX_IDS) {
        errs.push(format!(
            "{name} must contain no more than {MAX_IDS} elements"
        ));
    }
    if arr.is_none() {
        errs.push(format!("{name} must be an array"));
    }
}

/// `CreateShareReportDto`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateShareReportDto {
    #[serde(default, deserialize_with = "present")]
    name: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    start_at: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    end_at: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    projects: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    employees: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    expires_in_days: Option<Value>,
}

fn ids_of(v: &Option<Value>) -> Vec<i32> {
    match v {
        Some(Value::Array(a)) => {
            let nums: Vec<f64> = a.iter().filter_map(js_number).collect();
            let mut out: Vec<i32> = Vec::new();
            for n in nums {
                #[allow(clippy::cast_possible_truncation)]
                let id = n as i32;
                if !out.contains(&id) {
                    out.push(id);
                }
            }
            out
        }
        _ => Vec::new(),
    }
}

impl CreateShareReportDto {
    pub fn name(&self) -> &str {
        match &self.name {
            Some(Value::String(s)) => s,
            _ => "",
        }
    }
    pub fn start_at(&self) -> &str {
        match &self.start_at {
            Some(Value::String(s)) => s,
            _ => "",
        }
    }
    pub fn end_at(&self) -> &str {
        match &self.end_at {
            Some(Value::String(s)) => s,
            _ => "",
        }
    }
    /// `[...new Set(payload.projects)]`.
    pub fn project_ids(&self) -> Vec<i32> {
        ids_of(&self.projects)
    }
    pub fn employee_ids(&self) -> Vec<i32> {
        ids_of(&self.employees)
    }
    pub fn expires_in_days(&self) -> Option<i64> {
        match &self.expires_in_days {
            Some(v) => js_number(v).map(|f| {
                #[allow(clippy::cast_possible_truncation)]
                {
                    f as i64
                }
            }),
            None => None,
        }
    }
}

impl Validate for CreateShareReportDto {
    fn validate(&self) -> Vec<String> {
        let mut errs = Vec::new();
        // name: MaxLength, IsNotEmpty, IsString (bawah ke atas).
        let n = self.name.as_ref();
        if !matches!(n, Some(Value::String(s)) if s.chars().count() <= 200) {
            errs.push("name must be shorter than or equal to 200 characters".into());
        }
        if is_empty(n) {
            errs.push("name should not be empty".into());
        }
        if !matches!(n, Some(Value::String(_))) {
            errs.push("name must be a string".into());
        }
        for (name, v) in [("startAt", &self.start_at), ("endAt", &self.end_at)] {
            if !matches!(v, Some(Value::String(s)) if is_iso8601(s)) {
                errs.push(format!("{name} must be a valid ISO 8601 date string"));
            }
        }
        check_id_array(&mut errs, "projects", self.projects.as_ref());
        check_id_array(&mut errs, "employees", self.employees.as_ref());
        // ValidateIf(expiresInDays !== undefined): null tetap divalidasi (dan gagal).
        // Tanpa @Type: hanya `number` sungguhan yang lolos IsInt/Min/Max.
        if let Some(v) = &self.expires_in_days {
            let f = if let Value::Number(n) = v {
                n.as_f64()
            } else {
                None
            };
            if !f.is_some_and(|f| f <= 365.0) {
                errs.push("expiresInDays must not be greater than 365".into());
            }
            if !f.is_some_and(|f| f >= 1.0) {
                errs.push("expiresInDays must not be less than 1".into());
            }
            if !f.is_some_and(|f| f.fract() == 0.0) {
                errs.push("expiresInDays must be an integer number".into());
            }
        }
        errs
    }
}

/// `SendReportDto`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendReportDto {
    #[serde(default, deserialize_with = "present")]
    emails: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    subject: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    message: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    start_date: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    end_date: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    user_ids: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    project_ids: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    sections: Option<Value>,
}

pub const SEND_REPORT_MAX_RECIPIENTS: usize = 20;

fn str_of(v: &Option<Value>) -> &str {
    match v {
        Some(Value::String(s)) => s,
        _ => "",
    }
}

fn number_list(v: &Option<Value>) -> Vec<f64> {
    match v {
        Some(Value::Array(a)) => a.iter().map(|x| js_number(x).unwrap_or(f64::NAN)).collect(),
        _ => Vec::new(),
    }
}

impl SendReportDto {
    pub fn emails(&self) -> Vec<String> {
        match &self.emails {
            Some(Value::Array(a)) => a
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect(),
            _ => Vec::new(),
        }
    }
    pub fn subject(&self) -> &str {
        str_of(&self.subject)
    }
    pub fn message(&self) -> &str {
        str_of(&self.message)
    }
    pub fn start_date(&self) -> &str {
        str_of(&self.start_date)
    }
    pub fn end_date(&self) -> &str {
        str_of(&self.end_date)
    }
    /// `toIntIds(payload.userIds)`.
    pub fn user_ids(&self) -> Vec<i32> {
        crate::reports::support::to_int_ids(&number_list(&self.user_ids))
    }
    pub fn project_ids(&self) -> Vec<i32> {
        crate::reports::support::to_int_ids(&number_list(&self.project_ids))
    }
    /// `payload.sections || ['company','project','user']`.
    pub fn sections(&self) -> Vec<String> {
        match &self.sections {
            Some(Value::Array(a)) => a
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect(),
            _ => vec!["company".into(), "project".into(), "user".into()],
        }
    }
}

impl Validate for SendReportDto {
    fn validate(&self) -> Vec<String> {
        let mut errs = Vec::new();
        // emails: IsEmail(each), ArrayMaxSize, ArrayMinSize, IsArray (bawah ke atas).
        let arr = match &self.emails {
            Some(Value::Array(a)) => Some(a),
            _ => None,
        };
        let emails_ok = arr.is_some_and(|a| {
            a.iter()
                .all(|v| v.as_str().is_some_and(mz_core::extract::is_email))
        });
        if !emails_ok {
            errs.push("each value in emails must be an email".into());
        }
        if !arr.is_some_and(|a| a.len() <= SEND_REPORT_MAX_RECIPIENTS) {
            errs.push(format!(
                "emails must contain no more than {SEND_REPORT_MAX_RECIPIENTS} elements"
            ));
        }
        if !arr.is_some_and(|a| !a.is_empty()) {
            errs.push("emails must contain at least 1 elements".into());
        }
        if arr.is_none() {
            errs.push("emails must be an array".into());
        }
        for (name, v, max) in [
            ("subject", &self.subject, 200usize),
            ("message", &self.message, 2000usize),
        ] {
            if !matches!(v, Some(Value::String(s)) if s.chars().count() <= max) {
                errs.push(format!(
                    "{name} must be shorter than or equal to {max} characters"
                ));
            }
            if !matches!(v, Some(Value::String(_))) {
                errs.push(format!("{name} must be a string"));
            }
        }
        for (name, v) in [("startDate", &self.start_date), ("endDate", &self.end_date)] {
            if !matches!(v, Some(Value::String(s)) if is_iso8601(s)) {
                errs.push(format!("{name} must be a valid ISO 8601 date string"));
            }
        }
        for (name, v) in [
            ("userIds", &self.user_ids),
            ("projectIds", &self.project_ids),
            ("sections", &self.sections),
        ] {
            if !is_nullish(v.as_ref()) && !matches!(v, Some(Value::Array(_))) {
                errs.push(format!("{name} must be an array"));
            }
        }
        errs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn share(v: Value) -> Vec<String> {
        serde_json::from_value::<CreateShareReportDto>(v)
            .map(|d| d.validate())
            .unwrap_or_default()
    }

    #[test]
    fn share_valid_and_ids() {
        let v = json!({"name":"Laporan","startAt":"2026-07-01","endAt":"2026-07-31","projects":["12", 12, 3],"employees":[],"expiresInDays":30});
        assert!(share(v.clone()).is_empty());
        let d: CreateShareReportDto = serde_json::from_value(v).unwrap_or_default();
        assert_eq!(d.project_ids(), vec![12, 3]);
        assert_eq!(d.expires_in_days(), Some(30));
        assert_eq!(d.name(), "Laporan");
    }

    #[test]
    fn share_messages() {
        let errs = share(json!({}));
        assert_eq!(
            errs[0],
            "name must be shorter than or equal to 200 characters"
        );
        assert!(errs.contains(&"projects must be an array".to_owned()));
        assert!(errs.contains(&"each value in employees must be an integer number".to_owned()));
        let errs = share(
            json!({"name":"a","startAt":"2026-01-01","endAt":"2026-01-02","projects":[0,"x",1.5],"employees":[],"expiresInDays":400}),
        );
        assert_eq!(
            errs,
            vec![
                "each value in projects must not be less than 1",
                "each value in projects must be an integer number",
                "expiresInDays must not be greater than 365",
            ]
        );
        let errs = share(
            json!({"name":"a","startAt":"2026-01-01","endAt":"2026-01-02","projects":[],"employees":[],"expiresInDays":null}),
        );
        assert_eq!(errs.len(), 3);
        let errs = share(
            json!({"name":"a","startAt":"2026-01-01","endAt":"2026-01-02","projects":[],"employees":[],"expiresInDays":1.5}),
        );
        assert_eq!(errs, vec!["expiresInDays must be an integer number"]);
    }

    #[test]
    fn send_report_validation() {
        let ok: SendReportDto = serde_json::from_value(json!({
            "emails":["a@b.co"],"subject":"s","message":"m","startDate":"2026-01-01","endDate":"2026-01-07"
        }))
        .unwrap_or_default();
        assert!(ok.validate().is_empty());
        assert_eq!(ok.sections(), vec!["company", "project", "user"]);
        let bad: SendReportDto =
            serde_json::from_value(json!({"emails":[],"subject":1})).unwrap_or_default();
        let errs = bad.validate();
        assert!(errs.contains(&"emails must contain at least 1 elements".to_owned()));
        assert!(errs.contains(&"subject must be a string".to_owned()));
        assert!(errs.contains(&"startDate must be a valid ISO 8601 date string".to_owned()));
    }

    #[test]
    fn query_validation() {
        let q = QueryReportDto {
            start_at: Some("2026-01-01".into()),
            end_at: Some("2026-01-02".into()),
            ..Default::default()
        };
        assert!(q.validate().is_empty());
        let bad = QueryReportDto {
            billable_filter: Some("x".into()),
            user_uuid: Some("1".into()),
            ..Default::default()
        };
        let errs = bad.validate();
        assert_eq!(errs[0], "userUuid must be a UUID");
        assert!(errs.contains(&"endAt should not be empty".to_owned()));
        assert!(errs.last().is_some_and(|e| e.starts_with("billableFilter")));
    }
}
