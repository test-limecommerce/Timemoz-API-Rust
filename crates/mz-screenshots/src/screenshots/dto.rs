//! DTO `screenshots/dto/*.ts` dengan pesan class-validator.

use mz_core::extract::Validate;
use mz_core::iso8601::is_iso8601;
use serde::Deserialize;
use serde_json::Value;

use crate::util::{is_empty, is_nullish, is_uuid_value, present};

fn check_date(errs: &mut Vec<String>, v: Option<&Value>) {
    if is_empty(v) {
        errs.push("takenAt should not be empty".into());
    }
    if !matches!(v, Some(Value::String(s)) if is_iso8601(s)) {
        errs.push("takenAt must be a valid ISO 8601 date string".into());
    }
}

fn check_bool(errs: &mut Vec<String>, v: Option<&Value>) {
    if is_empty(v) {
        errs.push("isBlur should not be empty".into());
    }
    if !matches!(v, Some(Value::Bool(_))) {
        errs.push("isBlur must be a boolean value".into());
    }
}

fn check_uuid(errs: &mut Vec<String>, name: &str, v: Option<&Value>) {
    if is_empty(v) {
        errs.push(format!("{name} should not be empty"));
    }
    if !is_uuid_value(v) {
        errs.push(format!("{name} must be a UUID"));
    }
}

/// `CreateScreenshotDto`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateScreenshotDto {
    #[serde(default, deserialize_with = "present")]
    pub taken_at: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub is_blur: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub project_uuid: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub tracking_timeline_uuid: Option<Value>,
}

impl Validate for CreateScreenshotDto {
    fn validate(&self) -> Vec<String> {
        let mut errs = Vec::new();
        check_date(&mut errs, self.taken_at.as_ref());
        check_bool(&mut errs, self.is_blur.as_ref());
        check_uuid(&mut errs, "projectUuid", self.project_uuid.as_ref());
        check_uuid(
            &mut errs,
            "trackingTimelineUuid",
            self.tracking_timeline_uuid.as_ref(),
        );
        errs
    }
}

/// `UpdateScreenshotDto` = `PartialType(CreateScreenshotDto)` + `fileFullpath`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateScreenshotDto {
    #[serde(default, deserialize_with = "present")]
    pub taken_at: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub is_blur: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub project_uuid: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub tracking_timeline_uuid: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub file_fullpath: Option<Value>,
}

impl UpdateScreenshotDto {
    pub fn file_fullpath_str(&self) -> Option<&str> {
        match &self.file_fullpath {
            Some(Value::String(s)) => Some(s),
            _ => None,
        }
    }
}

impl Validate for UpdateScreenshotDto {
    fn validate(&self) -> Vec<String> {
        let mut errs = Vec::new();
        if !is_nullish(self.taken_at.as_ref()) {
            check_date(&mut errs, self.taken_at.as_ref());
        }
        if !is_nullish(self.is_blur.as_ref()) {
            check_bool(&mut errs, self.is_blur.as_ref());
        }
        if !is_nullish(self.project_uuid.as_ref()) {
            check_uuid(&mut errs, "projectUuid", self.project_uuid.as_ref());
        }
        if !is_nullish(self.tracking_timeline_uuid.as_ref()) {
            check_uuid(
                &mut errs,
                "trackingTimelineUuid",
                self.tracking_timeline_uuid.as_ref(),
            );
        }
        // Dekorator bawah dievaluasi lebih dulu: MaxLength, IsNotEmpty, IsString.
        let f = self.file_fullpath.as_ref();
        let is_str = matches!(f, Some(Value::String(_)));
        if !matches!(f, Some(Value::String(s)) if s.chars().count() <= 2048) {
            errs.push("fileFullpath must be shorter than or equal to 2048 characters".into());
        }
        if is_empty(f) {
            errs.push("fileFullpath should not be empty".into());
        }
        if !is_str {
            errs.push("fileFullpath must be a string".into());
        }
        errs
    }
}

/// `ScreenshotImageQueryDto`.
#[derive(Debug, Default, Deserialize)]
pub struct ScreenshotImageQueryDto {
    #[serde(default)]
    pub variant: Option<String>,
}

impl Validate for ScreenshotImageQueryDto {
    fn validate(&self) -> Vec<String> {
        match self.variant.as_deref() {
            None | Some("display" | "original") => Vec::new(),
            Some(_) => {
                vec!["variant must be one of the following values: display, original".into()]
            }
        }
    }
}

impl ScreenshotImageQueryDto {
    /// `query.variant ?? 'display'`.
    pub fn variant(&self) -> &str {
        self.variant.as_deref().unwrap_or("display")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn create(v: Value) -> Vec<String> {
        serde_json::from_value::<CreateScreenshotDto>(v)
            .map(|d| d.validate())
            .unwrap_or_default()
    }

    #[test]
    fn create_valid() {
        let ok = json!({"takenAt":"2026-11-16T08:00:00.000Z","isBlur":true,
            "projectUuid":"79cf2254-0124-48c2-8173-adf710f6140a","trackingTimelineUuid":"ccae960e-ec50-4d1a-929a-d6d68285efb5"});
        assert!(create(ok).is_empty());
    }

    #[test]
    fn create_messages() {
        let errs = create(json!({}));
        assert_eq!(
            errs,
            vec![
                "takenAt should not be empty",
                "takenAt must be a valid ISO 8601 date string",
                "isBlur should not be empty",
                "isBlur must be a boolean value",
                "projectUuid should not be empty",
                "projectUuid must be a UUID",
                "trackingTimelineUuid should not be empty",
                "trackingTimelineUuid must be a UUID",
            ]
        );
        assert_eq!(
            create(
                json!({"takenAt":"x","isBlur":"yes","projectUuid":"1","trackingTimelineUuid":"2"})
            )
            .len(),
            4
        );
    }

    #[test]
    fn update_requires_file_fullpath_only() {
        let d: UpdateScreenshotDto =
            serde_json::from_value(json!({"fileFullpath":"a/b.jpg"})).unwrap_or_default();
        assert!(d.validate().is_empty());
        let d: UpdateScreenshotDto = serde_json::from_value(json!({})).unwrap_or_default();
        assert_eq!(d.validate().len(), 3);
        let d: UpdateScreenshotDto =
            serde_json::from_value(json!({"fileFullpath":"x".repeat(2049)})).unwrap_or_default();
        assert_eq!(
            d.validate(),
            vec!["fileFullpath must be shorter than or equal to 2048 characters"]
        );
        let d: UpdateScreenshotDto =
            serde_json::from_value(json!({"fileFullpath":"a","isBlur":null})).unwrap_or_default();
        assert!(d.validate().is_empty());
    }

    #[test]
    fn variant_check() {
        assert!(
            ScreenshotImageQueryDto {
                variant: Some("original".into())
            }
            .validate()
            .is_empty()
        );
        assert_eq!(
            ScreenshotImageQueryDto {
                variant: Some("x".into())
            }
            .validate()
            .len(),
            1
        );
        assert_eq!(ScreenshotImageQueryDto::default().variant(), "display");
    }
}
