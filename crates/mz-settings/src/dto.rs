//! DTO `settings/dto/*` + `PaginatorDto`, divalidasi manual dari objek JSON mentah supaya
//! pesan class-validator sama (urutan pesan per properti = dekorator dari bawah ke atas).

use mz_core::extract::Validate;
use mz_core::uuid_rules::is_uuid;
use serde::Deserialize;
use serde_json::{Map, Value};

/// `@IsString() @IsNotEmpty()` wajib: `should not be empty` dulu, lalu `must be a string`.
fn required_string(m: &Map<String, Value>, field: &str, out: &mut Vec<String>) {
    match m.get(field) {
        None | Some(Value::Null) => {
            out.push(format!("{field} should not be empty"));
            out.push(format!("{field} must be a string"));
        }
        Some(Value::String(s)) => {
            if s.is_empty() {
                out.push(format!("{field} should not be empty"));
            }
        }
        Some(_) => out.push(format!("{field} must be a string")),
    }
}

/// `@IsOptional()` + `@IsString() @IsNotEmpty()` (hasil `PartialType`).
fn optional_string(m: &Map<String, Value>, field: &str, out: &mut Vec<String>) {
    match m.get(field) {
        None | Some(Value::Null) => {}
        Some(Value::String(s)) => {
            if s.is_empty() {
                out.push(format!("{field} should not be empty"));
            }
        }
        Some(_) => out.push(format!("{field} must be a string")),
    }
}

/// `CreateSettingDto`.
#[derive(Debug, Deserialize)]
#[serde(transparent)]
pub struct CreateSetting(pub Map<String, Value>);

impl Validate for CreateSetting {
    fn validate(&self) -> Vec<String> {
        let mut out = Vec::new();
        for f in ["code", "value", "label", "description"] {
            required_string(&self.0, f, &mut out);
        }
        out
    }
}

impl CreateSetting {
    pub fn str(&self, f: &str) -> String {
        self.0
            .get(f)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    }
}

/// `UpdateSettingDto` (`PartialType(CreateSettingDto)`).
#[derive(Debug, Deserialize)]
#[serde(transparent)]
pub struct UpdateSetting(pub Map<String, Value>);

impl Validate for UpdateSetting {
    fn validate(&self) -> Vec<String> {
        let mut out = Vec::new();
        for f in ["code", "value", "label", "description"] {
            optional_string(&self.0, f, &mut out);
        }
        out
    }
}

/// `SaveClickupTokenDto`.
#[derive(Debug, Deserialize)]
#[serde(transparent)]
pub struct ClickupToken(pub Map<String, Value>);

impl Validate for ClickupToken {
    fn validate(&self) -> Vec<String> {
        let mut out = Vec::new();
        required_string(&self.0, "apiToken", &mut out);
        out
    }
}

/// `SaveClickupWorkspaceDto`.
#[derive(Debug, Deserialize)]
#[serde(transparent)]
pub struct ClickupWorkspace(pub Map<String, Value>);

impl Validate for ClickupWorkspace {
    fn validate(&self) -> Vec<String> {
        let mut out = Vec::new();
        required_string(&self.0, "workspaceId", &mut out);
        out
    }
}

/// `CreateDefaultManualTimeApproverDto`: `@IsUUID() @IsNotEmpty()`.
#[derive(Debug, Deserialize)]
#[serde(transparent)]
pub struct ApproverBody(pub Map<String, Value>);

impl Validate for ApproverBody {
    fn validate(&self) -> Vec<String> {
        let mut out = Vec::new();
        let v = self.0.get("userUuid");
        if v.is_none_or(|v| v.is_null() || v.as_str() == Some("")) {
            out.push("userUuid should not be empty".into());
        }
        if !v.and_then(Value::as_str).is_some_and(is_uuid) {
            out.push("userUuid must be a UUID".into());
        }
        out
    }
}

impl ApproverBody {
    pub fn user_uuid(&self) -> String {
        self.0
            .get("userUuid")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    }
}

/// `parseInt(string, 10)`; `None` = NaN.
pub fn parse_int(s: &str) -> Option<i64> {
    let t = s.trim_start();
    let end = t
        .char_indices()
        .find(|(i, c)| !(c.is_ascii_digit() || (*i == 0 && (*c == '-' || *c == '+'))))
        .map_or(t.len(), |(i, _)| i);
    t.get(..end).and_then(|p| p.parse::<i64>().ok())
}

/// `PaginatorDto` dari query string. Nilai angka: `Some(None)` = NaN (gagal `@IsNumber`).
#[derive(Debug, Deserialize)]
#[serde(transparent)]
pub struct Paginator(pub Map<String, Value>);

pub struct Page {
    pub page_number: Option<i64>,
    pub items_per_page: Option<i64>,
    pub order_by: Option<String>,
    pub order_column: Option<String>,
    pub search: Option<String>,
}

impl Paginator {
    fn num(&self, f: &str) -> Option<Option<i64>> {
        match self.0.get(f)? {
            Value::String(s) => Some(parse_int(s)),
            _ => Some(None),
        }
    }
    fn string(&self, f: &str) -> Option<String> {
        self.0.get(f).and_then(Value::as_str).map(str::to_owned)
    }
    pub fn page(&self) -> Page {
        Page {
            page_number: self.num("pageNumber").flatten(),
            items_per_page: self.num("itemsPerPage").flatten(),
            order_by: self.string("orderBy"),
            order_column: self.string("orderColumn"),
            search: self.string("search"),
        }
    }
}

impl Validate for Paginator {
    fn validate(&self) -> Vec<String> {
        let mut out = Vec::new();
        for f in ["pageNumber", "itemsPerPage"] {
            if matches!(self.num(f), Some(None)) {
                out.push(format!(
                    "{f} must be a number conforming to the specified constraints"
                ));
            }
        }
        for f in ["orderBy", "orderColumn", "search"] {
            if self.0.get(f).is_some_and(|v| !v.is_string()) {
                out.push(format!("{f} must be a string"));
            }
        }
        out
    }
}

/// Query `GetLatestVersionMobileDto`.
#[derive(Debug, Deserialize)]
#[serde(transparent)]
pub struct MobileQuery(pub Map<String, Value>);

impl Validate for MobileQuery {
    fn validate(&self) -> Vec<String> {
        let mut out = Vec::new();
        let v = self.0.get("type");
        if !matches!(v.and_then(Value::as_str), Some("ANDROID" | "IOS")) {
            out.push("type must be one of the following values: ANDROID, IOS".into());
        }
        if v.is_none_or(|v| v.is_null() || v.as_str() == Some("")) {
            out.push("type should not be empty".into());
        }
        out
    }
}

impl MobileQuery {
    pub fn kind(&self) -> String {
        self.0
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    }
}

/// Query `GetPricingListsDto` (`product` opsional enum).
#[derive(Debug, Deserialize)]
#[serde(transparent)]
pub struct PricingQuery(pub Map<String, Value>);

impl Validate for PricingQuery {
    fn validate(&self) -> Vec<String> {
        match self.0.get("product") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::String(s)) if matches!(s.as_str(), "SALES" | "TIME" | "FULL") => Vec::new(),
            Some(_) => {
                vec!["product must be one of the following values: SALES, TIME, FULL".into()]
            }
        }
    }
}

impl PricingQuery {
    pub fn product(&self) -> Option<String> {
        self.0.get("product")?.as_str().map(str::to_owned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn create_requires_all_strings() {
        let b = CreateSetting(obj(json!({"code": "", "value": 5})));
        assert_eq!(
            b.validate(),
            vec![
                "code should not be empty",
                "value must be a string",
                "label should not be empty",
                "label must be a string",
                "description should not be empty",
                "description must be a string",
            ]
        );
    }

    #[test]
    fn update_is_partial() {
        assert!(UpdateSetting(obj(json!({}))).validate().is_empty());
        assert_eq!(
            UpdateSetting(obj(json!({"code": ""}))).validate(),
            vec!["code should not be empty"]
        );
    }

    #[test]
    fn paginator_nan_fails() {
        let p = Paginator(obj(json!({"pageNumber": "abc", "orderBy": ["x"]})));
        assert_eq!(
            p.validate(),
            vec![
                "pageNumber must be a number conforming to the specified constraints",
                "orderBy must be a string"
            ]
        );
        assert_eq!(parse_int("12abc"), Some(12));
        assert_eq!(parse_int(""), None);
    }

    #[test]
    fn mobile_type_messages() {
        assert_eq!(
            MobileQuery(obj(json!({}))).validate(),
            vec![
                "type must be one of the following values: ANDROID, IOS",
                "type should not be empty"
            ]
        );
        assert!(
            MobileQuery(obj(json!({"type": "IOS"})))
                .validate()
                .is_empty()
        );
    }

    #[test]
    fn approver_uuid() {
        assert_eq!(
            ApproverBody(obj(json!({"userUuid": "x"}))).validate(),
            vec!["userUuid must be a UUID"]
        );
        assert!(
            ApproverBody(obj(
                json!({"userUuid": "79cf2254-0124-48c2-8173-adf710f6140a"})
            ))
            .validate()
            .is_empty()
        );
    }
}
