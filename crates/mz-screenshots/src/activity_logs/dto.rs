//! DTO `activity-logs/dto/*.ts` dengan pesan class-validator.

use mz_core::extract::Validate;
use mz_core::iso8601::is_iso8601;
use serde::Deserialize;
use serde_json::Value;

use crate::util::{is_empty, is_nullish, present};

fn check_string_not_empty(errs: &mut Vec<String>, name: &str, v: Option<&Value>) {
    // `@IsString() @IsNotEmpty()`: dekorator bawah (IsNotEmpty) lebih dulu.
    if is_empty(v) {
        errs.push(format!("{name} should not be empty"));
    }
    if !matches!(v, Some(Value::String(_))) {
        errs.push(format!("{name} must be a string"));
    }
}

fn check_number(errs: &mut Vec<String>, name: &str, v: Option<&Value>) {
    if is_empty(v) {
        errs.push(format!("{name} should not be empty"));
    }
    if !matches!(v, Some(Value::Number(_))) {
        errs.push(format!(
            "{name} must be a number conforming to the specified constraints"
        ));
    }
}

fn check_optional_string(errs: &mut Vec<String>, name: &str, v: Option<&Value>) {
    if !is_nullish(v) && !matches!(v, Some(Value::String(_))) {
        errs.push(format!("{name} must be a string"));
    }
}

fn str_of(v: &Option<Value>) -> &str {
    match v {
        Some(Value::String(s)) => s,
        _ => "",
    }
}

fn ms_of(v: &Option<Value>) -> i64 {
    match v {
        // `new Date(seconds * 1000)`: dipotong ke bilangan bulat milidetik (TimeClip).
        #[allow(clippy::cast_possible_truncation)]
        Some(Value::Number(n)) => (n.as_f64().unwrap_or(0.0) * 1000.0).trunc() as i64,
        _ => 0,
    }
}

/// `CreateActivityLogDto`.
#[derive(Debug, Default, Deserialize)]
pub struct CreateActivityLogDto {
    #[serde(default, deserialize_with = "present")]
    process_name: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    window_title: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    full_url: Option<Value>,
    #[serde(default, rename = "startAt", deserialize_with = "present")]
    start_at: Option<Value>,
    #[serde(default, rename = "endAt", deserialize_with = "present")]
    end_at: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    timezone: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    tracking_timeline_uuid: Option<Value>,
}

impl CreateActivityLogDto {
    pub fn process_name(&self) -> &str {
        str_of(&self.process_name)
    }
    pub fn window_title(&self) -> &str {
        str_of(&self.window_title)
    }
    /// `dto.full_url` (kosong bila tidak dikirim).
    pub fn full_url(&self) -> &str {
        str_of(&self.full_url)
    }
    pub fn timezone(&self) -> &str {
        str_of(&self.timezone)
    }
    pub fn tracking_timeline_uuid(&self) -> &str {
        str_of(&self.tracking_timeline_uuid)
    }
    pub fn start_at_ms(&self) -> i64 {
        ms_of(&self.start_at)
    }
    pub fn end_at_ms(&self) -> i64 {
        ms_of(&self.end_at)
    }
}

impl Validate for CreateActivityLogDto {
    fn validate(&self) -> Vec<String> {
        let mut errs = Vec::new();
        check_string_not_empty(&mut errs, "process_name", self.process_name.as_ref());
        if !matches!(self.window_title, Some(Value::String(_))) {
            errs.push("window_title must be a string".into());
        }
        check_optional_string(&mut errs, "full_url", self.full_url.as_ref());
        check_number(&mut errs, "startAt", self.start_at.as_ref());
        check_number(&mut errs, "endAt", self.end_at.as_ref());
        check_string_not_empty(&mut errs, "timezone", self.timezone.as_ref());
        check_optional_string(
            &mut errs,
            "tracking_timeline_uuid",
            self.tracking_timeline_uuid.as_ref(),
        );
        errs
    }
}

/// `FindActivityLogsByDateDto` (query string).
#[derive(Debug, Default, Deserialize)]
pub struct FindActivityLogsByDateDto {
    #[serde(default, rename = "takenDate")]
    taken_date: Option<String>,
    #[serde(default)]
    pub tracking_timeline_uuid: Option<String>,
    #[serde(default)]
    pub search: Option<String>,
    #[serde(default)]
    pub user_uuid: Option<String>,
    #[serde(default)]
    pub project_uuid: Option<String>,
}

impl FindActivityLogsByDateDto {
    pub fn taken_date(&self) -> &str {
        self.taken_date.as_deref().unwrap_or("")
    }
}

impl Validate for FindActivityLogsByDateDto {
    fn validate(&self) -> Vec<String> {
        let mut errs = Vec::new();
        let d = self.taken_date.as_deref();
        if d.is_none_or(str::is_empty) {
            errs.push("takenDate should not be empty".into());
        }
        if !d.is_some_and(is_iso8601) {
            errs.push("takenDate must be a valid ISO 8601 date string".into());
        }
        errs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn create_valid_and_ms() {
        let d: CreateActivityLogDto = serde_json::from_value(json!({
            "process_name":"Electron","window_title":"","startAt":1772068771,"endAt":1772068820.5,"timezone":"Asia/Jakarta",
            "tracking_timeline_uuid":"x"
        }))
        .unwrap_or_default();
        assert!(d.validate().is_empty());
        assert_eq!(d.start_at_ms(), 1_772_068_771_000);
        assert_eq!(d.end_at_ms(), 1_772_068_820_500);
    }

    #[test]
    fn create_messages() {
        let d = CreateActivityLogDto::default();
        assert_eq!(
            d.validate(),
            vec![
                "process_name should not be empty",
                "process_name must be a string",
                "window_title must be a string",
                "startAt should not be empty",
                "startAt must be a number conforming to the specified constraints",
                "endAt should not be empty",
                "endAt must be a number conforming to the specified constraints",
                "timezone should not be empty",
                "timezone must be a string",
            ]
        );
    }

    #[test]
    fn find_requires_date() {
        assert_eq!(FindActivityLogsByDateDto::default().validate().len(), 2);
        let d = FindActivityLogsByDateDto {
            taken_date: Some("2026-01-01".into()),
            ..Default::default()
        };
        assert!(d.validate().is_empty());
    }
}
