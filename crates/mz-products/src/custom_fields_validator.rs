//! Port `src/custom-fields/custom-fields.validator.ts` (generik: `product`/`order`/`orderItem`)
//! + `src/catalog-query/catalog-query.value-cast.ts`. Fungsi murni.

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;
use serde::Serialize;
use serde_json::{Map, Value};

use crate::dto::re;

/// `CustomFieldType` (Prisma) — dipakai bersama `orderCustomFieldDefinition`.
pub const CUSTOM_FIELD_TYPES: &[&str] =
    &["text", "number", "date", "boolean", "select", "multiselect"];

/// `CustomFieldValidationEntity`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CfEntity {
    Product,
    Order,
    OrderItem,
}

impl CfEntity {
    /// `MAX_CUSTOM_FIELD_KEYS_BY_ENTITY`.
    pub fn max_keys(self) -> usize {
        match self {
            Self::Product | Self::Order => 100,
            Self::OrderItem => 20,
        }
    }
}

/// `MAX_CUSTOM_FIELD_STRING_LENGTH` (panjang JS = unit UTF-16).
pub const MAX_CUSTOM_FIELD_STRING_LENGTH: usize = 500;

/// Satu opsi select/multiselect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct CfOption {
    pub value: String,
    pub label: String,
}

/// `CustomFieldDefinitionForValidation`.
#[derive(Debug, Clone, PartialEq)]
pub struct CfDefinition {
    pub key: String,
    pub field_type: String,
    pub is_required: bool,
    pub is_active: bool,
    pub options: Option<Vec<CfOption>>,
}

impl CfDefinition {
    /// `options` dari kolom `Json?` (bentuk lain = tanpa opsi).
    pub fn options_from_json(v: Option<&Value>) -> Option<Vec<CfOption>> {
        let arr = v?.as_array()?;
        Some(
            arr.iter()
                .filter_map(|o| {
                    let value = o.get("value").map(js_string)?;
                    let label = o.get("label").map(js_string).unwrap_or_default();
                    Some(CfOption { value, label })
                })
                .collect(),
        )
    }
}

fn js_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// `CustomFieldValueIssue` — `key` kosong untuk aturan tanpa key (jumlah key).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CfIssue {
    pub key: String,
    pub code: String,
    pub message: String,
}

fn issue(key: &str, code: &str, message: String) -> CfIssue {
    CfIssue {
        key: key.to_owned(),
        code: code.to_owned(),
        message,
    }
}

/// `Object.keys(obj)`: kunci indeks array (bilangan bulat kanonik < 2^32-1) dulu, menaik,
/// lalu sisanya menurut urutan penyisipan.
pub fn js_keys(m: &Map<String, Value>) -> Vec<&String> {
    let is_index = |k: &str| {
        k.parse::<u32>()
            .ok()
            .is_some_and(|n| n != u32::MAX && n.to_string() == k)
    };
    let mut idx: Vec<&String> = m.keys().filter(|k| is_index(k)).collect();
    idx.sort_by_key(|k| k.parse::<u32>().unwrap_or(0));
    idx.extend(m.keys().filter(|k| !is_index(k)));
    idx
}

fn is_empty_value(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) => s.is_empty(),
        Some(Value::Array(a)) => a.is_empty(),
        _ => false,
    }
}

fn is_nested(v: &Value) -> bool {
    match v {
        Value::Object(_) => true,
        Value::Array(a) => a.iter().any(|e| e.is_object() || e.is_array()),
        _ => false,
    }
}

fn js_len(s: &str) -> usize {
    s.encode_utf16().count()
}

static DATE: LazyLock<Regex> = LazyLock::new(|| re(r"^\d{4}-\d{2}-\d{2}$"));

fn type_mismatch(key: &str, expected: &str) -> CfIssue {
    issue(
        key,
        "CUSTOM_FIELD_TYPE_MISMATCH",
        format!("{key} must be {expected}"),
    )
}

fn too_long(key: &str) -> CfIssue {
    issue(
        key,
        "CUSTOM_FIELD_TOO_LONG",
        format!("{key} must be at most {MAX_CUSTOM_FIELD_STRING_LENGTH} characters"),
    )
}

fn check_option(def: &CfDefinition, key: &str, value: &str) -> Option<CfIssue> {
    let allowed = def
        .options
        .as_deref()
        .unwrap_or_default()
        .iter()
        .any(|o| o.value == value);
    (!allowed).then(|| {
        issue(
            key,
            "CUSTOM_FIELD_INVALID_OPTION",
            format!("{key} value \"{value}\" is not one of the configured options"),
        )
    })
}

fn check_value(def: &CfDefinition, key: &str, value: &Value) -> Option<CfIssue> {
    match def.field_type.as_str() {
        "text" => match value {
            Value::String(s) if js_len(s) > MAX_CUSTOM_FIELD_STRING_LENGTH => Some(too_long(key)),
            Value::String(_) => None,
            _ => Some(type_mismatch(key, "a string")),
        },
        "number" => (!value.is_number()).then(|| type_mismatch(key, "a number")),
        "boolean" => (!value.is_boolean()).then(|| type_mismatch(key, "a boolean")),
        "date" => match value {
            Value::String(s) if DATE.is_match(s) => None,
            _ => Some(type_mismatch(key, "a \"YYYY-MM-DD\" date string")),
        },
        "select" => match value {
            Value::String(s) if js_len(s) > MAX_CUSTOM_FIELD_STRING_LENGTH => Some(too_long(key)),
            Value::String(s) => check_option(def, key, s),
            _ => Some(type_mismatch(key, "a string")),
        },
        "multiselect" => {
            let Some(items) = value.as_array().filter(|a| a.iter().all(Value::is_string)) else {
                return Some(type_mismatch(key, "an array of strings"));
            };
            for el in items.iter().filter_map(Value::as_str) {
                if js_len(el) > MAX_CUSTOM_FIELD_STRING_LENGTH {
                    return Some(too_long(key));
                }
                if let Some(i) = check_option(def, key, el) {
                    return Some(i);
                }
            }
            None
        }
        _ => None,
    }
}

/// `validateCustomFieldValues(entity, definitions, values)` — 8 aturan §4.1. `definitions` boleh
/// memuat baris terarsip (dianggap tidak terdaftar untuk penulisan).
pub fn validate_custom_field_values(
    entity: CfEntity,
    definitions: &[CfDefinition],
    values: &Map<String, Value>,
) -> Vec<CfIssue> {
    let mut issues = Vec::new();
    let keys = js_keys(values);
    let max = entity.max_keys();
    if keys.len() > max {
        issues.push(issue(
            "",
            "CUSTOM_FIELD_TOO_MANY",
            format!("at most {max} custom fields are allowed"),
        ));
    }
    let mut active: HashMap<&str, &CfDefinition> = HashMap::new();
    for d in definitions.iter().filter(|d| d.is_active) {
        active.insert(d.key.as_str(), d);
    }
    for key in keys {
        let value = values.get(key.as_str());
        let Some(def) = active.get(key.as_str()) else {
            issues.push(issue(
                key,
                "CUSTOM_FIELD_UNKNOWN_KEY",
                format!("unknown custom field \"{key}\""),
            ));
            continue;
        };
        if is_empty_value(value) {
            continue;
        }
        let Some(value) = value else { continue };
        if is_nested(value) {
            issues.push(issue(
                key,
                "CUSTOM_FIELD_NESTED_VALUE",
                format!("{key} must not be a nested object"),
            ));
            continue;
        }
        issues.extend(check_value(def, key, value));
    }
    for def in definitions {
        if def.is_active && def.is_required && is_empty_value(values.get(&def.key)) {
            issues.push(issue(
                &def.key,
                "CUSTOM_FIELD_REQUIRED",
                format!("{} is required", def.key),
            ));
        }
    }
    issues
}

// ───────────────────────── value cast (catalog-query) ─────────────────────────

/// `castCustomFieldFilterValue(key, type, raw)`: `Err(pesan)` = `InvalidCustomFieldQueryValueError`.
pub fn cast_filter_value(key: &str, field_type: &str, raw: &str) -> Result<Value, String> {
    match field_type {
        "number" => {
            let n = mz_core::jsfmt::number(raw);
            if n.is_finite() {
                Ok(mz_core::jsfmt::num(n))
            } else {
                Err(format!("{key} must be a number"))
            }
        }
        "boolean" => match raw {
            "true" => Ok(Value::Bool(true)),
            "false" => Ok(Value::Bool(false)),
            _ => Err(format!("{key} must be \"true\" or \"false\"")),
        },
        "date" => {
            if DATE.is_match(raw) {
                Ok(Value::String(raw.to_owned()))
            } else {
                Err(format!("{key} must be a \"YYYY-MM-DD\" date string"))
            }
        }
        "multiselect" => Ok(Value::Array(vec![Value::String(raw.to_owned())])),
        _ => Ok(Value::String(raw.to_owned())),
    }
}

/// `customFieldSortCast(type)`: `None` = multiselect (tidak bisa diurutkan).
pub fn sort_cast(field_type: &str) -> Option<&'static str> {
    match field_type {
        "number" => Some("numeric"),
        "boolean" => Some("boolean"),
        "multiselect" => None,
        _ => Some("text"),
    }
}

static CF_SORT_BY: LazyLock<Regex> = LazyLock::new(|| re(r"^cf:([a-z][a-z0-9_]{0,39})$"));

/// `parseCfSortBy(sortBy)`.
pub fn parse_cf_sort_by(sort_by: &str) -> Option<String> {
    CF_SORT_BY
        .captures(sort_by)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_owned())
}

/// `castSummaryValue(type, raw)`.
pub fn cast_summary_value(field_type: &str, raw: Option<&str>) -> Value {
    let raw_s = raw.unwrap_or("null");
    match field_type {
        "number" => {
            let n = raw.map_or(0.0, mz_core::jsfmt::number);
            if n.is_finite() {
                mz_core::jsfmt::num(n)
            } else {
                Value::String(raw_s.to_owned())
            }
        }
        "boolean" => Value::Bool(raw == Some("true")),
        _ => raw.map_or(Value::Null, |s| Value::String(s.to_owned())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn def(key: &str, t: &str) -> CfDefinition {
        CfDefinition {
            key: key.into(),
            field_type: t.into(),
            is_required: false,
            is_active: true,
            options: None,
        }
    }

    fn defs() -> Vec<CfDefinition> {
        let opts = |vals: &[&str]| {
            Some(
                vals.iter()
                    .map(|v| CfOption {
                        value: (*v).into(),
                        label: v.to_uppercase(),
                    })
                    .collect(),
            )
        };
        let mut select = def("delivery_window", "select");
        select.options = opts(&["pagi", "siang"]);
        let mut multi = def("campaign_tags", "multiselect");
        multi.options = opts(&["display", "promo"]);
        let mut legacy = def("legacy", "text");
        legacy.is_active = false;
        vec![
            def("po_number", "text"),
            def("crate_count", "number"),
            def("is_urgent", "boolean"),
            def("expected_payment_date", "date"),
            select,
            multi,
            legacy,
        ]
    }

    fn codes(v: Value) -> Vec<(String, String)> {
        let m = v.as_object().cloned().unwrap();
        validate_custom_field_values(CfEntity::Order, &defs(), &m)
            .into_iter()
            .map(|i| (i.key, i.code))
            .collect()
    }

    fn one(key: &str, code: &str) -> Vec<(String, String)> {
        vec![(key.into(), code.into())]
    }

    #[test]
    fn aturan() {
        assert_eq!(
            codes(json!({"does_not_exist": "x"})),
            one("does_not_exist", "CUSTOM_FIELD_UNKNOWN_KEY")
        );
        assert_eq!(
            codes(json!({"legacy": "x"})),
            one("legacy", "CUSTOM_FIELD_UNKNOWN_KEY")
        );
        assert!(codes(json!({"po_number": "PO-1"})).is_empty());
        assert_eq!(
            codes(json!({"crate_count": "12"})),
            one("crate_count", "CUSTOM_FIELD_TYPE_MISMATCH")
        );
        assert!(codes(json!({"crate_count": 12})).is_empty());
        assert_eq!(
            codes(json!({"delivery_window": "malam"})),
            one("delivery_window", "CUSTOM_FIELD_INVALID_OPTION")
        );
        assert!(codes(json!({"delivery_window": "pagi"})).is_empty());
        assert_eq!(
            codes(json!({"campaign_tags": "display"})),
            one("campaign_tags", "CUSTOM_FIELD_TYPE_MISMATCH")
        );
        assert_eq!(
            codes(json!({"campaign_tags": ["display", 1]})),
            one("campaign_tags", "CUSTOM_FIELD_TYPE_MISMATCH")
        );
        assert!(codes(json!({"campaign_tags": ["display", "promo"]})).is_empty());
        assert_eq!(
            codes(json!({"po_number": {"a": 1}})),
            one("po_number", "CUSTOM_FIELD_NESTED_VALUE")
        );
        assert_eq!(
            codes(json!({"campaign_tags": [{"a": 1}]})),
            one("campaign_tags", "CUSTOM_FIELD_NESTED_VALUE")
        );
        assert_eq!(
            codes(json!({"po_number": "a".repeat(501)})),
            one("po_number", "CUSTOM_FIELD_TOO_LONG")
        );
        assert!(codes(json!({"po_number": "a".repeat(500)})).is_empty());
        assert!(
            codes(json!({"is_urgent": true, "expected_payment_date": "2026-09-20"})).is_empty()
        );
        assert_eq!(
            codes(json!({"expected_payment_date": "20/09/2026"})),
            one("expected_payment_date", "CUSTOM_FIELD_TYPE_MISMATCH")
        );
        assert!(codes(json!({"po_number": null, "campaign_tags": []})).is_empty());
    }

    #[test]
    fn required() {
        let mut d = def("required_field", "text");
        d.is_required = true;
        let issues = validate_custom_field_values(CfEntity::Product, &[d.clone()], &Map::new());
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].code, "CUSTOM_FIELD_REQUIRED");
        assert_eq!(issues[0].message, "required_field is required");
        let m = json!({"required_field": ""}).as_object().cloned().unwrap();
        assert_eq!(
            validate_custom_field_values(CfEntity::Product, &[d], &m)[0].code,
            "CUSTOM_FIELD_REQUIRED"
        );
    }

    fn many(n: usize) -> (Vec<CfDefinition>, Map<String, Value>) {
        let defs: Vec<CfDefinition> = (0..n).map(|i| def(&format!("f{i}"), "text")).collect();
        let m = defs.iter().map(|d| (d.key.clone(), json!("v"))).collect();
        (defs, m)
    }

    #[test]
    fn batas_per_entity() {
        let too_many = |e: CfEntity, n: usize| {
            let (d, m) = many(n);
            validate_custom_field_values(e, &d, &m)
                .iter()
                .any(|i| i.code == "CUSTOM_FIELD_TOO_MANY" && i.key.is_empty())
        };
        assert!(!too_many(CfEntity::Order, 100));
        assert!(too_many(CfEntity::Order, 101));
        assert!(!too_many(CfEntity::Product, 100));
        assert!(!too_many(CfEntity::OrderItem, 20));
        assert!(too_many(CfEntity::OrderItem, 21));
        assert!(!too_many(CfEntity::Order, 21));
    }

    #[test]
    fn urutan_kunci_js() {
        let m = json!({"b": 1, "10": 1, "a": 1, "2": 1})
            .as_object()
            .cloned()
            .unwrap();
        let k: Vec<&str> = js_keys(&m).into_iter().map(String::as_str).collect();
        assert_eq!(k, vec!["2", "10", "b", "a"]);
    }

    // catalog-query.value-cast.spec.ts
    #[test]
    fn value_cast() {
        assert_eq!(cast_filter_value("n", "number", "12"), Ok(json!(12)));
        assert_eq!(cast_filter_value("n", "number", "1.5"), Ok(json!(1.5)));
        assert!(cast_filter_value("n", "number", "abc").is_err());
        assert_eq!(cast_filter_value("b", "boolean", "true"), Ok(json!(true)));
        assert_eq!(
            cast_filter_value("b", "boolean", "yes"),
            Err("b must be \"true\" or \"false\"".into())
        );
        assert!(cast_filter_value("d", "date", "2026-01-01").is_ok());
        assert!(cast_filter_value("d", "date", "01-01-2026").is_err());
        assert_eq!(cast_filter_value("m", "multiselect", "x"), Ok(json!(["x"])));
        assert_eq!(cast_filter_value("t", "text", "x"), Ok(json!("x")));
        assert_eq!(sort_cast("number"), Some("numeric"));
        assert_eq!(sort_cast("multiselect"), None);
        assert_eq!(sort_cast("date"), Some("text"));
        assert_eq!(
            parse_cf_sort_by("cf:shelf_life").as_deref(),
            Some("shelf_life")
        );
        assert_eq!(parse_cf_sort_by("name"), None);
        assert_eq!(cast_summary_value("number", Some("365")), json!(365));
        assert_eq!(cast_summary_value("number", Some("x")), json!("x"));
        assert_eq!(cast_summary_value("boolean", Some("true")), json!(true));
        assert_eq!(cast_summary_value("text", Some("dry")), json!("dry"));
    }
}
