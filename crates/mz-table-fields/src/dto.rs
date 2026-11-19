//! DTO `table-fields/dto/*`, divalidasi manual dari JSON mentah. Pesan & urutannya diverifikasi
//! terhadap `ValidationPipe({ transform: true })` + class-validator 0.14 sungguhan
//! (dekorator dihitung dari bawah ke atas; galat anak `ValidateNested` disusun seperti
//! `flattenValidationErrors` Nest).

use mz_core::extract::Validate;
use mz_core::uuid_rules::is_uuid;
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::registry::{FIELD_VISIBILITY_MODES, table_keys};

fn is_in_tables(v: Option<&Value>) -> bool {
    v.and_then(Value::as_str)
        .is_some_and(|s| table_keys().contains(&s))
}

/// `@IsString() @IsIn(TABLE_KEYS)` (wajib): IsIn dulu, lalu IsString.
fn table_key_required(m: &Map<String, Value>, out: &mut Vec<String>) {
    let v = m.get("tableKey");
    if !is_in_tables(v) {
        out.push(format!(
            "tableKey must be one of the following values: {}",
            table_keys().join(", ")
        ));
    }
    if !v.is_some_and(Value::is_string) {
        out.push("tableKey must be a string".into());
    }
}

fn defined(v: Option<&Value>) -> Option<&Value> {
    v.filter(|v| !v.is_null())
}

fn str_of(m: &Map<String, Value>, k: &str) -> String {
    m.get(k)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// `@IsOptional() @IsUUID()`.
fn optional_uuid(m: &Map<String, Value>, k: &str, out: &mut Vec<String>) {
    if let Some(v) = defined(m.get(k))
        && !v.as_str().is_some_and(is_uuid)
    {
        out.push(format!("{k} must be a UUID"));
    }
}

/// `parseInt(value, 10)` atas nilai query apa pun (`String(value)` dulu). `None` = NaN.
fn js_parse_int(v: &Value) -> Option<i64> {
    let s = match v {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .map(|i| match i {
                Value::String(s) => s.clone(),
                Value::Null => String::new(),
                other => other.to_string(),
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    let t = s.trim_start();
    let end = t
        .char_indices()
        .find(|(i, c)| !(c.is_ascii_digit() || (*i == 0 && (*c == '-' || *c == '+'))))
        .map_or(t.len(), |(i, _)| i);
    t.get(..end).and_then(|p| p.parse::<i64>().ok())
}

// ───────────────────────── query ─────────────────────────

/// `TableFieldMatrixQueryDto`.
#[derive(Debug, Deserialize)]
#[serde(transparent)]
pub struct MatrixQuery(pub Map<String, Value>);

impl Validate for MatrixQuery {
    fn validate(&self) -> Vec<String> {
        let mut out = Vec::new();
        table_key_required(&self.0, &mut out);
        out
    }
}

impl MatrixQuery {
    pub fn table_key(&self) -> String {
        str_of(&self.0, "tableKey")
    }
}

/// `ResetTableFieldMatrixQueryDto`.
#[derive(Debug, Deserialize)]
#[serde(transparent)]
pub struct ResetQuery(pub Map<String, Value>);

impl Validate for ResetQuery {
    fn validate(&self) -> Vec<String> {
        let mut out = Vec::new();
        table_key_required(&self.0, &mut out);
        optional_uuid(&self.0, "accessLevelId", &mut out);
        out
    }
}

impl ResetQuery {
    pub fn table_key(&self) -> String {
        str_of(&self.0, "tableKey")
    }
    /// `query.accessLevelId` truthy.
    pub fn access_level_id(&self) -> Option<String> {
        Some(str_of(&self.0, "accessLevelId")).filter(|s| !s.is_empty())
    }
}

/// `ListRevealLogsQueryDto`.
#[derive(Debug, Deserialize)]
#[serde(transparent)]
pub struct LogsQuery(pub Map<String, Value>);

fn is_day(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b.iter().enumerate().all(|(i, c)| match i {
            4 | 7 => *c == b'-',
            _ => c.is_ascii_digit(),
        })
}

impl LogsQuery {
    /// `page` sesudah `@Transform` (default 1). `None` = NaN.
    fn page_value(&self) -> Option<i64> {
        self.0.get("page").map_or(Some(1), js_parse_int)
    }
    fn limit_value(&self) -> Option<i64> {
        self.0.get("limit").map_or(Some(20), js_parse_int)
    }
    pub fn page(&self) -> i64 {
        self.page_value().unwrap_or(1)
    }
    pub fn limit(&self) -> i64 {
        self.limit_value().unwrap_or(20)
    }
    pub fn text(&self, k: &str) -> Option<String> {
        defined(self.0.get(k))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .filter(|s| !s.is_empty())
    }
}

impl Validate for LogsQuery {
    fn validate(&self) -> Vec<String> {
        let m = &self.0;
        let mut out = Vec::new();
        if let Some(v) = defined(m.get("tableKey")) {
            if !is_in_tables(Some(v)) {
                out.push(format!(
                    "tableKey must be one of the following values: {}",
                    table_keys().join(", ")
                ));
            }
            if !v.is_string() {
                out.push("tableKey must be a string".into());
            }
        }
        if let Some(v) = defined(m.get("columnKey")) {
            if !v.as_str().is_some_and(|s| utf16_len(s) <= 48) {
                out.push("columnKey must be shorter than or equal to 48 characters".into());
            }
            if !v.is_string() {
                out.push("columnKey must be a string".into());
            }
        }
        optional_uuid(m, "actorId", &mut out);
        for k in ["startDate", "endDate"] {
            if let Some(v) = defined(m.get(k))
                && !v.as_str().is_some_and(is_day)
            {
                out.push(format!("{k} must be YYYY-MM-DD"));
            }
        }
        // `@IsInt() @Min(1) @IsOptional()`: Min, IsInt.
        let page = self.page_value();
        if !page.is_some_and(|p| p >= 1) {
            out.push("page must not be less than 1".into());
        }
        if page.is_none() {
            out.push("page must be an integer number".into());
        }
        // `@Max(100) @Min(1) @IsInt()`.
        let limit = self.limit_value();
        if !limit.is_some_and(|l| l <= 100) {
            out.push("limit must not be greater than 100".into());
        }
        if !limit.is_some_and(|l| l >= 1) {
            out.push("limit must not be less than 1".into());
        }
        if limit.is_none() {
            out.push("limit must be an integer number".into());
        }
        out
    }
}

// ───────────────────────── body ─────────────────────────

/// `RevealFieldDto`.
#[derive(Debug, Deserialize)]
#[serde(transparent)]
pub struct RevealBody(pub Map<String, Value>);

impl Validate for RevealBody {
    fn validate(&self) -> Vec<String> {
        let m = &self.0;
        let mut out = Vec::new();
        table_key_required(m, &mut out);
        if !m
            .get("recordId")
            .and_then(Value::as_str)
            .is_some_and(is_uuid)
        {
            out.push("recordId must be a UUID".into());
        }
        let field = m.get("field");
        if !field
            .and_then(Value::as_str)
            .is_some_and(|s| utf16_len(s) <= 48)
        {
            out.push("field must be shorter than or equal to 48 characters".into());
        }
        if !field.is_some_and(Value::is_string) {
            out.push("field must be a string".into());
        }
        if let Some(v) = defined(m.get("reason")) {
            if !v.as_str().is_some_and(|s| utf16_len(s) <= 255) {
                out.push("reason must be shorter than or equal to 255 characters".into());
            }
            if !v.is_string() {
                out.push("reason must be a string".into());
            }
        }
        out
    }
}

impl RevealBody {
    pub fn table_key(&self) -> String {
        str_of(&self.0, "tableKey")
    }
    pub fn record_id(&self) -> String {
        str_of(&self.0, "recordId")
    }
    pub fn field(&self) -> String {
        str_of(&self.0, "field")
    }
    pub fn reason(&self) -> Option<String> {
        defined(self.0.get("reason"))
            .and_then(Value::as_str)
            .map(str::to_owned)
    }
}

/// Satu entri `{ columnKey, mode }` yang valid.
#[derive(Debug, Clone)]
pub struct Entry {
    pub column_key: String,
    pub mode: String,
}

/// Satu baris matriks yang valid.
#[derive(Debug, Clone)]
pub struct MatrixRow {
    pub access_level_id: Option<String>,
    pub entries: Vec<Entry>,
}

/// `PutTableFieldMatrixDto`.
#[derive(Debug, Deserialize)]
#[serde(transparent)]
pub struct PutBody(pub Map<String, Value>);

const NESTED: &str = "must be either object or array";

fn is_object_like(v: &Value) -> bool {
    v.is_object() || v.is_array()
}

/// Galat satu entri (`TableFieldEntryInputDto`) dengan awalan jalur `path` (mis. `rows.0.entries.1`).
fn entry_errors(path: &str, v: &Value) -> Vec<String> {
    let m = v.as_object();
    let get = |k: &str| m.and_then(|m| m.get(k));
    let mut out = Vec::new();
    if !get("columnKey").is_some_and(Value::is_string) {
        out.push(format!("{path}.columnKey must be a string"));
    }
    let mode = get("mode");
    if !mode
        .and_then(Value::as_str)
        .is_some_and(|s| FIELD_VISIBILITY_MODES.contains(&s))
    {
        out.push(format!(
            "{path}.mode must be one of the following values: {}",
            FIELD_VISIBILITY_MODES.join(", ")
        ));
    }
    if !mode.is_some_and(Value::is_string) {
        out.push(format!("{path}.mode must be a string"));
    }
    out
}

/// Properti array bersarang `@IsArray() @ArrayNotEmpty() @ValidateNested({each})`.
/// `x` = awalan jalur induk (`""` atau `"rows.0."`), `prop` = nama properti,
/// `each(path, element)` = galat satu elemen objek. `top` = galat tingkat-atas (pesan sendiri
/// dibuang bila punya anak, seperti `flattenValidationErrors` Nest).
fn nested_array(
    x: &str,
    prop: &str,
    value: Option<&Value>,
    top: bool,
    each: &dyn Fn(&str, &Value) -> Vec<String>,
) -> Vec<String> {
    let mut own = Vec::new();
    let is_array = value.is_some_and(Value::is_array);
    if !value
        .and_then(Value::as_array)
        .is_some_and(|a| !a.is_empty())
    {
        own.push(format!("{x}{prop} should not be empty"));
    }
    if !is_array {
        own.push(format!("{x}{prop} must be an array"));
    }
    let mut children = Vec::new();
    let mut nested_own = Vec::new();
    match value {
        None => {}
        Some(Value::Array(items)) => {
            for (i, el) in items.iter().enumerate() {
                if is_object_like(el) {
                    children.extend(each(&format!("{x}{prop}.{i}"), el));
                } else {
                    children.push(format!(
                        "{x}{prop}.each value in nested property {prop} {NESTED}"
                    ));
                }
            }
        }
        Some(obj @ Value::Object(_)) => {
            children.extend(each(&format!("{x}{prop}"), obj));
        }
        Some(_) => nested_own.push(format!("{x}each value in nested property {prop} {NESTED}")),
    }
    if children.is_empty() {
        own.extend(nested_own);
        return own;
    }
    if !top {
        children.extend(own);
    }
    children
}

/// Galat satu baris (`TableFieldRowInputDto`) di jalur `path` (mis. `rows.0`).
fn row_errors(path: &str, v: &Value) -> Vec<String> {
    let m = v.as_object();
    let mut out = Vec::new();
    if let Some(a) = defined(m.and_then(|m| m.get("accessLevelId")))
        && !a.as_str().is_some_and(is_uuid)
    {
        out.push(format!("{path}.accessLevelId must be a UUID"));
    }
    out.extend(nested_array(
        &format!("{path}."),
        "entries",
        m.and_then(|m| m.get("entries")),
        false,
        &entry_errors,
    ));
    out
}

impl Validate for PutBody {
    fn validate(&self) -> Vec<String> {
        let mut out = Vec::new();
        table_key_required(&self.0, &mut out);
        out.extend(nested_array(
            "",
            "rows",
            self.0.get("rows"),
            true,
            &row_errors,
        ));
        out
    }
}

impl PutBody {
    pub fn table_key(&self) -> String {
        str_of(&self.0, "tableKey")
    }

    /// Baris yang valid (hanya dipanggil sesudah `validate` lolos).
    pub fn rows(&self) -> Vec<MatrixRow> {
        self.0
            .get("rows")
            .and_then(Value::as_array)
            .map(|rows| {
                rows.iter()
                    .map(|r| MatrixRow {
                        access_level_id: defined(r.get("accessLevelId"))
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                            .map(str::to_owned),
                        entries: r
                            .get("entries")
                            .and_then(Value::as_array)
                            .map(|es| {
                                es.iter()
                                    .map(|e| Entry {
                                        column_key: str_of_value(e, "columnKey"),
                                        mode: str_of_value(e, "mode"),
                                    })
                                    .collect()
                            })
                            .unwrap_or_default(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

fn str_of_value(v: &Value, k: &str) -> String {
    v.get(k)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn put(v: Value) -> Vec<String> {
        PutBody(v.as_object().cloned().unwrap_or_default()).validate()
    }

    const KEYS: &str = "stores, customers, products, orders, order_items";

    /// Nilai yang sama dengan keluaran ValidationPipe Nest nyata (lihat catatan modul).
    #[test]
    fn put_matches_nest() {
        assert_eq!(
            put(json!({})),
            vec![
                format!("tableKey must be one of the following values: {KEYS}"),
                "tableKey must be a string".into(),
                "rows should not be empty".into(),
                "rows must be an array".into()
            ]
        );
        assert_eq!(
            put(json!({"tableKey": "stores", "rows": [5, "a", null]})),
            vec!["rows.each value in nested property rows must be either object or array"; 3]
        );
        assert_eq!(
            put(json!({"tableKey": "stores", "rows": [
                {"accessLevelId": "zz", "entries": []},
                {"entries": [7, {"columnKey": 1, "mode": "q"}]},
                {}
            ]})),
            vec![
                "rows.0.accessLevelId must be a UUID".to_owned(),
                "rows.0.entries should not be empty".into(),
                "rows.1.entries.each value in nested property entries must be either object or array".into(),
                "rows.1.entries.1.columnKey must be a string".into(),
                "rows.1.entries.1.mode must be one of the following values: visible, revealable, masked, hidden".into(),
                "rows.2.entries should not be empty".into(),
                "rows.2.entries must be an array".into(),
            ]
        );
        assert_eq!(
            put(json!({"tableKey": "stores", "rows": "abc"})),
            vec![
                "rows should not be empty",
                "rows must be an array",
                "each value in nested property rows must be either object or array"
            ]
        );
        assert_eq!(
            put(json!({"tableKey": "stores", "rows": {}})),
            vec![
                "rows.entries should not be empty",
                "rows.entries must be an array"
            ]
        );
        assert!(
            put(json!({"tableKey": "stores", "rows": [{"entries": [{"columnKey": "phone", "mode": "masked"}]}]}))
                .is_empty()
        );
    }

    #[test]
    fn logs_query_matches_nest() {
        let q = |v: Value| LogsQuery(v.as_object().cloned().unwrap_or_default()).validate();
        assert!(q(json!({})).is_empty());
        assert_eq!(
            q(json!({"page": "abc", "limit": "x"})),
            vec![
                "page must not be less than 1",
                "page must be an integer number",
                "limit must not be greater than 100",
                "limit must not be less than 1",
                "limit must be an integer number"
            ]
        );
        assert!(q(json!({"page": ["2"]})).is_empty());
        assert_eq!(
            q(json!({"tableKey": "", "actorId": ""})),
            vec![
                format!("tableKey must be one of the following values: {KEYS}"),
                "actorId must be a UUID".into()
            ]
        );
    }

    #[test]
    fn reveal_body_matches_nest() {
        let r = RevealBody(
            json!({"tableKey": "stores", "recordId": "x", "field": "p".repeat(49), "reason": 5})
                .as_object()
                .cloned()
                .unwrap_or_default(),
        );
        assert_eq!(
            r.validate(),
            vec![
                "recordId must be a UUID",
                "field must be shorter than or equal to 48 characters",
                "reason must be shorter than or equal to 255 characters",
                "reason must be a string"
            ]
        );
    }
}
