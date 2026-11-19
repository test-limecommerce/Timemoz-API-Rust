//! Port `form-leaf-value.validator.ts`: periksa satu nilai daun terhadap type &
//! `validation` node-nya. Pesan identik dengan Node.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::consts::{FormFieldFormat, FormNodeType, is_uuid_pattern};
use crate::types::{FormNode, OptionsSource};
use crate::value_util::{
    decimal_places, is_calendar_date, is_clock_time, is_integer, js_str_cmp, resolve_date_bound_at,
    resolve_instant_bound_at, text_length,
};

/// `LeafValueOptions` (`form-leaf-value.validator.ts:23`).
#[derive(Debug, Clone, Copy)]
pub struct LeafValueOptions<'a> {
    /// Timezone company.
    pub time_zone: &'a str,
    /// Batas bawah hanya ditegakkan saat submit.
    pub enforce_lower_bounds: bool,
}

const DEFAULT_RATING_MAX: f64 = 5.0;

/// `String(number)` JS untuk pesan galat.
pub(crate) fn js_num(x: f64) -> String {
    if x.is_nan() {
        return "NaN".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    mz_core::jsfmt::num_string(x)
}

/// `Number(v)` JS untuk nilai JSON.
pub(crate) fn js_to_number(v: &Value) -> f64 {
    match v {
        Value::Null => 0.0,
        Value::Bool(b) => f64::from(u8::from(*b)),
        Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
        Value::String(s) => mz_core::jsfmt::number(s),
        Value::Array(a) => match a.as_slice() {
            [] => 0.0,
            [one] if !one.is_object() => js_to_number(one),
            _ => f64::NAN,
        },
        Value::Object(_) => f64::NAN,
    }
}

/// `Number(node.config?.max ?? DEFAULT_RATING_MAX)`.
pub(crate) fn rating_max(node: &FormNode) -> f64 {
    match node.config_value("max") {
        None | Some(Value::Null) => DEFAULT_RATING_MAX,
        Some(v) => js_to_number(v),
    }
}

/// `IMPLICIT_FORMAT_BY_TYPE`.
fn implicit_format(t: FormNodeType) -> Option<&'static str> {
    match t {
        FormNodeType::Email => Some("email"),
        FormNodeType::Url => Some("url"),
        FormNodeType::Phone => Some("phone_id"),
        _ => None,
    }
}

fn option_values(node: &FormNode) -> HashSet<&str> {
    node.options
        .as_ref()
        .and_then(|o| o.items.as_ref())
        .map(|items| items.iter().map(|i| i.value.as_str()).collect())
        .unwrap_or_default()
}

fn check_format(node: &FormNode, value: &str, issues: &mut Vec<String>) {
    let named = node
        .validation
        .as_ref()
        .and_then(|v| v.format.as_deref())
        .or_else(|| implicit_format(node.ty));
    let Some(named) = named else {
        return;
    };
    if let Some(f) = FormFieldFormat::parse(named)
        && !f.matches(value)
    {
        issues.push(format!("value does not match the {named} format"));
    }
}

fn validate_text(
    node: &FormNode,
    value: &Value,
    opts: &LeafValueOptions,
    issues: &mut Vec<String>,
) {
    let Some(s) = value.as_str() else {
        issues.push("value must be a string".into());
        return;
    };
    let length = text_length(s) as f64;
    let rules = node.validation.as_ref();
    if let Some(max) = rules.and_then(|r| r.max_length)
        && length > max
    {
        issues.push(format!("value must be at most {} characters", js_num(max)));
    }
    if opts.enforce_lower_bounds
        && let Some(min) = rules.and_then(|r| r.min_length)
        && length < min
    {
        issues.push(format!("value must be at least {} characters", js_num(min)));
    }
    check_format(node, s, issues);
}

fn validate_number(
    node: &FormNode,
    value: &Value,
    opts: &LeafValueOptions,
    issues: &mut Vec<String>,
) {
    let Some(x) = value.as_f64().filter(|x| x.is_finite()) else {
        issues.push("value must be a finite number".into());
        return;
    };
    let rules = node.validation.as_ref();
    if let Some(max) = rules.and_then(|r| r.max)
        && x > max
    {
        issues.push(format!("value must be at most {}", js_num(max)));
    }
    if opts.enforce_lower_bounds
        && let Some(min) = rules.and_then(|r| r.min)
        && x < min
    {
        issues.push(format!("value must be at least {}", js_num(min)));
    }
    if let Some(decimals) = rules.and_then(|r| r.decimals)
        && (decimal_places(x) as f64) > decimals
    {
        issues.push(if decimals == 0.0 {
            "value must be a whole number".into()
        } else {
            format!(
                "value must have at most {} decimal places",
                js_num(decimals)
            )
        });
    }
}

fn validate_date_bounds(
    node: &FormNode,
    value: &str,
    opts: &LeafValueOptions,
    now: DateTime<Utc>,
    issues: &mut Vec<String>,
) {
    let Some(rules) = node.validation.as_ref() else {
        return;
    };
    if let Some(min) = rules.min_date.as_deref()
        && let Some(bound) = resolve_date_bound_at(min, opts.time_zone, now)
        && js_str_cmp(value, &bound).is_lt()
    {
        issues.push(format!("date must be on or after {bound}"));
    }
    if let Some(max) = rules.max_date.as_deref()
        && let Some(bound) = resolve_date_bound_at(max, opts.time_zone, now)
        && js_str_cmp(value, &bound).is_gt()
    {
        issues.push(format!("date must be on or before {bound}"));
    }
}

fn finite(v: Option<&Value>) -> Option<f64> {
    v.and_then(Value::as_f64).filter(|x| x.is_finite())
}

fn validate_location(value: &Value, issues: &mut Vec<String>) {
    let Some(o) = value.as_object() else {
        issues.push("value must be an object with lat and lng".into());
        return;
    };
    for key in crate::answers_validator::js_object_keys(o) {
        if !matches!(key, "lat" | "lng" | "accuracy" | "capturedAt") {
            issues.push(format!("unknown location property \"{key}\""));
        }
    }
    if !finite(o.get("lat")).is_some_and(|x| (-90.0..=90.0).contains(&x)) {
        issues.push("lat must be a number between -90 and 90".into());
    }
    if !finite(o.get("lng")).is_some_and(|x| (-180.0..=180.0).contains(&x)) {
        issues.push("lng must be a number between -180 and 180".into());
    }
    if let Some(acc) = o.get("accuracy")
        && !finite(Some(acc)).is_some_and(|x| x >= 0.0)
    {
        issues.push("accuracy must be a non-negative number".into());
    }
    if let Some(at) = o.get("capturedAt")
        && !at.as_f64().is_some_and(is_integer)
    {
        issues.push("capturedAt must be an epoch-milliseconds integer".into());
    }
}

/// Bentuk media sebagaimana disimpan: hanya `fileUuid` (Keputusan §0.17).
fn validate_media(value: &Value, issues: &mut Vec<String>) {
    let Some(o) = value.as_object() else {
        issues.push("value must be an object with fileUuid".into());
        return;
    };
    for key in crate::answers_validator::js_object_keys(o) {
        if key != "fileUuid" {
            issues.push(if key == "objectKey" {
                "objectKey is resolved by the server and must not be sent".into()
            } else {
                format!("unknown media property \"{key}\"")
            });
        }
    }
    if !o
        .get("fileUuid")
        .and_then(Value::as_str)
        .is_some_and(is_uuid_pattern)
    {
        issues.push("fileUuid must be a uuid".into());
    }
}

fn validate_choice(
    node: &FormNode,
    value: &Value,
    opts: &LeafValueOptions,
    issues: &mut Vec<String>,
) {
    let allowed = option_values(node);
    let is_static = node
        .options
        .as_ref()
        .is_none_or(|o| o.source == OptionsSource::Static);
    if node.ty.is_single_choice() {
        let Some(s) = value.as_str() else {
            issues.push("value must be a string".into());
            return;
        };
        if is_static && !allowed.contains(s) {
            issues.push(format!("\"{s}\" is not an allowed option"));
        }
        return;
    }
    let Some(items) = value.as_array() else {
        issues.push("value must be an array of option values".into());
        return;
    };
    let mut seen: HashSet<&str> = HashSet::new();
    for item in items {
        let Some(s) = item.as_str() else {
            issues.push("every selected option must be a string".into());
            continue;
        };
        if is_static && !allowed.contains(s) {
            issues.push(format!("\"{s}\" is not an allowed option"));
        }
        if seen.contains(s) {
            issues.push(format!("\"{s}\" is selected more than once"));
        }
        seen.insert(s);
    }
    let rules = node.validation.as_ref();
    let len = items.len() as f64;
    if let Some(max) = rules.and_then(|r| r.max_items)
        && len > max
    {
        issues.push(format!("select at most {} options", js_num(max)));
    }
    if opts.enforce_lower_bounds
        && let Some(min) = rules.and_then(|r| r.min_items)
        && len < min
    {
        issues.push(format!("select at least {} options", js_num(min)));
    }
}

/// `validateLeafValue(node, value, opts)` (`form-leaf-value.validator.ts:264`) — nilai
/// dijamin tidak kosong; daftar pesan, kosong = lolos.
pub fn validate_leaf_value(
    node: &FormNode,
    value: &Value,
    opts: &LeafValueOptions<'_>,
) -> Vec<String> {
    validate_leaf_value_at(node, value, opts, Utc::now())
}

/// [`validate_leaf_value`] dengan jam tetap (uji, dan pemanggil yang sudah memegang `now`).
pub fn validate_leaf_value_at(
    node: &FormNode,
    value: &Value,
    opts: &LeafValueOptions<'_>,
    now: DateTime<Utc>,
) -> Vec<String> {
    let mut issues = Vec::new();
    let t = node.ty;
    if t.is_text_like() {
        validate_text(node, value, opts, &mut issues);
        return issues;
    }
    if t.is_number_like() {
        validate_number(node, value, opts, &mut issues);
        return issues;
    }
    if t.is_single_choice() || t.is_multi_choice() {
        validate_choice(node, value, opts, &mut issues);
        return issues;
    }
    match t {
        FormNodeType::Date => match value.as_str().filter(|s| is_calendar_date(s)) {
            None => issues.push("value must be a YYYY-MM-DD date".into()),
            Some(s) => validate_date_bounds(node, s, opts, now, &mut issues),
        },
        FormNodeType::Time => {
            let Some(s) = value.as_str().filter(|s| is_clock_time(s)) else {
                issues.push("value must be an HH:mm time".into());
                return issues;
            };
            let rules = node.validation.as_ref();
            // Truthy: string kosong tidak dihitung.
            if let Some(min) = rules
                .and_then(|r| r.min_date.as_deref())
                .filter(|m| !m.is_empty())
                && js_str_cmp(s, min).is_lt()
            {
                issues.push(format!("time must be at or after {min}"));
            }
            if let Some(max) = rules
                .and_then(|r| r.max_date.as_deref())
                .filter(|m| !m.is_empty())
                && js_str_cmp(s, max).is_gt()
            {
                issues.push(format!("time must be at or before {max}"));
            }
        }
        FormNodeType::Datetime => {
            let Some(x) = value.as_f64().filter(|x| is_integer(*x)) else {
                issues.push("value must be epoch milliseconds".into());
                return issues;
            };
            let rules = node.validation.as_ref();
            if let Some(min) = rules.and_then(|r| r.min_date.as_deref())
                && let Some(bound) = resolve_instant_bound_at(min, opts.time_zone, false, now)
                && x < bound as f64
            {
                issues.push(format!("datetime must be on or after {min}"));
            }
            if let Some(max) = rules.and_then(|r| r.max_date.as_deref())
                && let Some(bound) = resolve_instant_bound_at(max, opts.time_zone, true, now)
                && x > bound as f64
            {
                issues.push(format!("datetime must be on or before {max}"));
            }
        }
        FormNodeType::Boolean => {
            if !value.is_boolean() {
                issues.push("value must be a boolean".into());
            }
        }
        FormNodeType::Rating => {
            let max = rating_max(node);
            let Some(x) = value.as_f64().filter(|x| is_integer(*x)) else {
                issues.push("value must be a whole number".into());
                return issues;
            };
            if x < 0.0 || x > max {
                issues.push(format!("rating must be between 0 and {}", js_num(max)));
            }
        }
        FormNodeType::Signature | FormNodeType::Photo | FormNodeType::File => {
            validate_media(value, &mut issues);
        }
        FormNodeType::Location => validate_location(value, &mut issues),
        other => issues.push(format!("type \"{}\" does not hold a value", other.as_str())),
    }
    issues
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn node(v: Value) -> FormNode {
        FormNode::from_value(&v).unwrap()
    }

    const OPTS: LeafValueOptions<'static> = LeafValueOptions {
        time_zone: "Asia/Jakarta",
        enforce_lower_bounds: true,
    };

    #[test]
    fn rating_nan_max_message() {
        let n = node(json!({"key": "r", "type": "rating", "config": {"max": "x"}}));
        // `value > NaN` false → lolos, sama dengan Node.
        assert!(validate_leaf_value(&n, &json!(9), &OPTS).is_empty());
        assert_eq!(
            validate_leaf_value(&n, &json!(-1), &OPTS),
            vec!["rating must be between 0 and NaN"]
        );
    }

    #[test]
    fn display_type_reports() {
        let n = node(json!({"key": "h", "type": "heading"}));
        assert_eq!(
            validate_leaf_value(&n, &json!("x"), &OPTS),
            vec!["type \"heading\" does not hold a value"]
        );
    }

    #[test]
    fn time_bounds() {
        let n = node(
            json!({"key": "t", "type": "time", "validation": {"minDate": "08:00", "maxDate": "17:00"}}),
        );
        assert!(validate_leaf_value(&n, &json!("09:00"), &OPTS).is_empty());
        assert_eq!(
            validate_leaf_value(&n, &json!("07:59"), &OPTS),
            vec!["time must be at or after 08:00"]
        );
        assert_eq!(
            validate_leaf_value(&n, &json!("17:01"), &OPTS),
            vec!["time must be at or before 17:00"]
        );
    }

    #[test]
    fn location_extras() {
        let n = node(json!({"key": "l", "type": "location"}));
        assert_eq!(
            validate_leaf_value(
                &n,
                &json!({"lat": 0, "lng": 0, "accuracy": -1, "capturedAt": 1.5}),
                &OPTS
            ),
            vec![
                "accuracy must be a non-negative number",
                "capturedAt must be an epoch-milliseconds integer"
            ]
        );
        assert_eq!(
            validate_leaf_value(&n, &json!("x"), &OPTS),
            vec!["value must be an object with lat and lng"]
        );
    }
}
