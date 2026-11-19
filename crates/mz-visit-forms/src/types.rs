//! Port `form-schema.types.ts`: bentuk schema SESUDAH lolos `validateFormSchema`.
//!
//! Node membaca kolom `schema` (JSON) lewat cast tipe tanpa pemeriksaan; Rust membacanya
//! lewat [`FormDocument::from_value`] yang longgar: properti bertipe salah dianggap absen,
//! node ber-`type` tak dikenal dibuang (schema tervalidasi tidak pernah memilikinya).
//! Yang datang dari admin (belum tervalidasi) tetap `serde_json::Value` — hanya validator
//! yang boleh menyimpulkan bentuknya.

use serde_json::{Map, Value};

use crate::consts::{FormNodeType, VisibleIfOp};

#[derive(Debug, Clone, PartialEq)]
pub struct FormOptionItem {
    pub value: String,
    pub label: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptionsSource {
    Static,
    Product,
    Order,
}

impl OptionsSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Static => "static",
            Self::Product => "product",
            Self::Order => "order",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "static" => Some(Self::Static),
            "product" => Some(Self::Product),
            "order" => Some(Self::Order),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FormOptions {
    pub source: OptionsSource,
    /// Hanya `static`.
    pub items: Option<Vec<FormOptionItem>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FormVisibleIfCondition {
    pub field: String,
    pub op: VisibleIfOp,
    /// `None` = properti `value` absen (`undefined`).
    pub value: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct FormVisibleIf {
    pub all: Option<Vec<FormVisibleIfCondition>>,
    pub any: Option<Vec<FormVisibleIfCondition>>,
}

/// `FormValidation` — angka tetap `f64` (JS number).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FormValidation {
    pub min_length: Option<f64>,
    pub max_length: Option<f64>,
    pub format: Option<String>,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub decimals: Option<f64>,
    pub min_date: Option<String>,
    pub max_date: Option<String>,
    pub min_items: Option<f64>,
    pub max_items: Option<f64>,
    pub min_rows: Option<f64>,
    pub max_rows: Option<f64>,
    pub max_size_bytes: Option<f64>,
    pub accept: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FormNode {
    pub key: String,
    pub ty: FormNodeType,
    pub label: Option<String>,
    pub description: Option<String>,
    pub placeholder: Option<String>,
    pub required: Option<bool>,
    /// `None` = absen.
    pub default_value: Option<Value>,
    pub col_span: Option<f64>,
    pub visible_if: Option<FormVisibleIf>,
    pub validation: Option<FormValidation>,
    pub options: Option<FormOptions>,
    pub config: Option<Map<String, Value>>,
    /// Hanya `section`, `group`, `repeater`.
    pub fields: Option<Vec<FormNode>>,
}

impl FormNode {
    /// `node.fields ?? []`.
    pub fn children(&self) -> &[FormNode] {
        self.fields.as_deref().unwrap_or(&[])
    }

    pub fn is_required(&self) -> bool {
        self.required == Some(true)
    }

    /// `node.config?.[key]`.
    pub fn config_value(&self, key: &str) -> Option<&Value> {
        self.config.as_ref().and_then(|c| c.get(key))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FormLayoutBlock {
    pub key: String,
    /// `text` | `image` | `divider` | `spacer` | `row`.
    pub ty: String,
    pub text: Option<String>,
    pub source: Option<String>,
    pub asset_uuid: Option<String>,
    pub col_span: Option<f64>,
    pub config: Option<Map<String, Value>>,
    pub blocks: Option<Vec<FormLayoutBlock>>,
}

impl FormLayoutBlock {
    pub fn config_value(&self, key: &str) -> Option<&Value> {
        self.config.as_ref().and_then(|c| c.get(key))
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct FormDocument {
    pub schema_version: f64,
    pub fields: Vec<FormNode>,
    /// `None` = absen (kop bawaan).
    pub headers: Option<Vec<FormLayoutBlock>>,
    /// `None` = absen (kaki bawaan).
    pub footers: Option<Vec<FormLayoutBlock>>,
}

fn s(o: &Map<String, Value>, k: &str) -> Option<String> {
    o.get(k).and_then(Value::as_str).map(str::to_owned)
}

fn n(o: &Map<String, Value>, k: &str) -> Option<f64> {
    o.get(k).and_then(Value::as_f64)
}

fn arr<'a>(o: &'a Map<String, Value>, k: &str) -> Option<&'a Vec<Value>> {
    o.get(k).and_then(Value::as_array)
}

fn obj<'a>(o: &'a Map<String, Value>, k: &str) -> Option<&'a Map<String, Value>> {
    o.get(k).and_then(Value::as_object)
}

fn col_span(o: &Map<String, Value>) -> Option<f64> {
    obj(o, "layout").and_then(|l| n(l, "colSpan"))
}

fn conditions(v: Option<&Vec<Value>>) -> Option<Vec<FormVisibleIfCondition>> {
    v.map(|items| {
        items
            .iter()
            .filter_map(Value::as_object)
            .filter_map(|c| {
                Some(FormVisibleIfCondition {
                    field: s(c, "field")?,
                    op: VisibleIfOp::parse(c.get("op")?.as_str()?)?,
                    value: c.get("value").cloned(),
                })
            })
            .collect()
    })
}

impl FormValidation {
    fn from_map(o: &Map<String, Value>) -> Self {
        Self {
            min_length: n(o, "minLength"),
            max_length: n(o, "maxLength"),
            format: s(o, "format"),
            min: n(o, "min"),
            max: n(o, "max"),
            decimals: n(o, "decimals"),
            min_date: s(o, "minDate"),
            max_date: s(o, "maxDate"),
            min_items: n(o, "minItems"),
            max_items: n(o, "maxItems"),
            min_rows: n(o, "minRows"),
            max_rows: n(o, "maxRows"),
            max_size_bytes: n(o, "maxSizeBytes"),
            accept: arr(o, "accept").map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            }),
        }
    }
}

impl FormNode {
    /// Baca satu node; `None` kalau bukan object atau `type` tak dikenal.
    pub fn from_value(v: &Value) -> Option<Self> {
        let o = v.as_object()?;
        let ty = FormNodeType::parse(o.get("type")?.as_str()?)?;
        let visible_if = obj(o, "visibleIf").map(|r| FormVisibleIf {
            all: conditions(arr(r, "all")),
            any: conditions(arr(r, "any")),
        });
        let options = obj(o, "options").and_then(|opt| {
            Some(FormOptions {
                source: OptionsSource::parse(opt.get("source")?.as_str()?)?,
                items: arr(opt, "items").map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_object)
                        .filter_map(|i| {
                            Some(FormOptionItem {
                                value: s(i, "value")?,
                                label: s(i, "label").unwrap_or_default(),
                            })
                        })
                        .collect()
                }),
            })
        });
        Some(Self {
            key: s(o, "key").unwrap_or_default(),
            ty,
            label: s(o, "label"),
            description: s(o, "description"),
            placeholder: s(o, "placeholder"),
            required: o.get("required").and_then(Value::as_bool),
            default_value: o.get("defaultValue").cloned(),
            col_span: col_span(o),
            visible_if,
            validation: obj(o, "validation").map(FormValidation::from_map),
            options,
            config: obj(o, "config").cloned(),
            fields: arr(o, "fields").map(|f| f.iter().filter_map(Self::from_value).collect()),
        })
    }
}

impl FormLayoutBlock {
    pub fn from_value(v: &Value) -> Option<Self> {
        let o = v.as_object()?;
        Some(Self {
            key: s(o, "key").unwrap_or_default(),
            ty: s(o, "type")?,
            text: s(o, "text"),
            source: s(o, "source"),
            asset_uuid: s(o, "assetUuid"),
            col_span: col_span(o),
            config: obj(o, "config").cloned(),
            blocks: arr(o, "blocks").map(|b| b.iter().filter_map(Self::from_value).collect()),
        })
    }
}

impl FormDocument {
    /// Baca kolom `schema` (atau `dto.schema` yang sudah lolos validator).
    pub fn from_value(v: &Value) -> Self {
        let Some(o) = v.as_object() else {
            return Self::default();
        };
        let blocks = |k: &str| {
            arr(o, k).map(|b| {
                b.iter()
                    .filter_map(FormLayoutBlock::from_value)
                    .collect::<Vec<_>>()
            })
        };
        Self {
            schema_version: n(o, "schemaVersion").unwrap_or(0.0),
            fields: arr(o, "fields")
                .map(|f| f.iter().filter_map(FormNode::from_value).collect())
                .unwrap_or_default(),
            headers: blocks("headers"),
            footers: blocks("footers"),
        }
    }
}

/// `FormSchemaIssue` — `path` menunjuk posisi di dokumen.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct FormSchemaIssue {
    pub path: String,
    pub message: String,
}

impl FormSchemaIssue {
    pub fn new(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            message: message.into(),
        }
    }
}

/// `FormAnswerIssue` — `key` adalah jalur data, mis. `unit_list.0.qty`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct FormAnswerIssue {
    pub key: String,
    pub message: String,
}

impl FormAnswerIssue {
    pub fn new(key: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            message: message.into(),
        }
    }
}

/// `AnswerFileReference`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnswerFileReference {
    pub file_uuid: String,
    pub field_path: String,
}

/// `AnswerProductOrderReference`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnswerProductOrderReference {
    /// `Product` | `Order`.
    pub source: OptionsSource,
    pub field_path: String,
    pub ids: Vec<String>,
}

/// `PdfMedia` (files service) = `SubmissionPdfMedia` (generator PDF): byte gambar siap tanam.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PdfMedia {
    pub buffer: Vec<u8>,
    pub content_type: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_lenient() {
        let doc = FormDocument::from_value(&json!({
            "schemaVersion": 1,
            "fields": [
                {"key": "a", "type": "text", "required": true, "layout": {"colSpan": 6}},
                {"key": "b", "type": "nope"},
                {"key": "r", "type": "repeater", "fields": [{"key": "q", "type": "number"}],
                 "visibleIf": {"all": [{"field": "a", "op": "notEmpty"}]}},
                {"key": "s", "type": "select", "options": {"source": "static", "items": [{"value": "x", "label": "X"}]}}
            ],
            "headers": [{"key": "h", "type": "text", "text": "{{storeName}}"}]
        }));
        assert_eq!(doc.fields.len(), 3);
        assert_eq!(doc.fields[0].col_span, Some(6.0));
        assert!(doc.fields[0].is_required());
        assert_eq!(doc.fields[1].children().len(), 1);
        let vis = doc.fields[1].visible_if.as_ref().unwrap();
        assert_eq!(vis.all.as_ref().unwrap()[0].op, VisibleIfOp::NotEmpty);
        assert!(vis.all.as_ref().unwrap()[0].value.is_none());
        assert_eq!(
            doc.fields[2]
                .options
                .as_ref()
                .unwrap()
                .items
                .as_ref()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(doc.headers.as_ref().unwrap().len(), 1);
        assert!(doc.footers.is_none());
    }
}
