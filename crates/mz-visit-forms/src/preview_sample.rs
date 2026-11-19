//! Port `form-pdf-preview.sample.ts`: jawaban contoh untuk pratinjau PDF di builder.
//! Murni & deterministik (kecuali "hari ini"/`Date.now()`).

use chrono::{DateTime, Utc};
use serde_json::{Map, Value, json};

use crate::consts::FormNodeType;
use crate::leaf_validator::rating_max;
use crate::types::{FormDocument, FormNode, OptionsSource};
use crate::value_util::today_in;
use crate::visibility::AnswerScope;

/// Kunci peta media untuk byte gambar contoh (`form-pdf-preview.sample.ts:20`).
pub const PREVIEW_SIGNATURE_KEY: &str = "preview-signature";
/// `form-pdf-preview.sample.ts:21`.
pub const PREVIEW_PHOTO_KEY: &str = "preview-photo";
const PREVIEW_FILE_KEY: &str = "preview-file";

const REPEATER_SAMPLE_ROWS: f64 = 2.0;

fn sample_number(node: &FormNode, fallback: f64) -> Value {
    let rules = node.validation.as_ref();
    let mut value = fallback;
    if let Some(min) = rules.and_then(|r| r.min)
        && value < min
    {
        value = min;
    }
    if let Some(max) = rules.and_then(|r| r.max)
        && value > max
    {
        value = max;
    }
    mz_core::jsfmt::num(value)
}

fn static_option_values(node: &FormNode) -> Vec<String> {
    match &node.options {
        Some(o) if o.source == OptionsSource::Static => o
            .items
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .map(|i| i.value.clone())
            .collect(),
        _ => Vec::new(),
    }
}

fn sample_leaf(node: &FormNode, time_zone: &str, row: usize, now: DateTime<Utc>) -> Option<Value> {
    let suffix = if row > 0 {
        format!(" {}", row + 1)
    } else {
        String::new()
    };
    let r = row as f64;
    Some(match node.ty {
        FormNodeType::Text => json!(format!(
            "Contoh {}{suffix}",
            node.label.as_deref().unwrap_or("jawaban")
        )),
        FormNodeType::Textarea => json!(
            "Contoh jawaban panjang: kondisi toko baik, display rapi, dan stok tersedia sesuai rencana kunjungan."
        ),
        FormNodeType::Email => json!("contoh@perusahaan.co.id"),
        FormNodeType::Phone => json!("081234567890"),
        FormNodeType::Url => json!("https://contoh.co.id"),
        FormNodeType::Barcode => json!("8991234567890"),
        FormNodeType::Number => sample_number(node, 10.0 + r),
        FormNodeType::Currency => sample_number(node, 150_000.0 * (r + 1.0)),
        FormNodeType::Percent => sample_number(node, 25.0),
        FormNodeType::Date => json!(today_in(time_zone, now).format("%Y-%m-%d").to_string()),
        FormNodeType::Time => json!("09:30"),
        FormNodeType::Datetime => json!(now.timestamp_millis()),
        FormNodeType::Select | FormNodeType::Radio => {
            let values = static_option_values(node);
            let idx = row % values.len().max(1);
            json!(
                values
                    .get(idx)
                    .cloned()
                    .unwrap_or_else(|| "Contoh pilihan".into())
            )
        }
        FormNodeType::Multiselect | FormNodeType::Checkbox => {
            let values = static_option_values(node);
            if values.is_empty() {
                json!(["Contoh pilihan"])
            } else {
                json!(values.into_iter().take(2).collect::<Vec<_>>())
            }
        }
        FormNodeType::Boolean => json!(true),
        // `Math.max(1, max - 1)` — NaN merambat (JSON: null).
        FormNodeType::Rating => {
            let max = rating_max(node);
            mz_core::jsfmt::num(if max.is_nan() {
                f64::NAN
            } else {
                (max - 1.0).max(1.0)
            })
        }
        FormNodeType::Signature => json!({ "fileUuid": PREVIEW_SIGNATURE_KEY }),
        FormNodeType::Photo => json!({ "fileUuid": PREVIEW_PHOTO_KEY }),
        FormNodeType::File => json!({
            "fileUuid": PREVIEW_FILE_KEY,
            "contentType": "application/pdf",
            "sizeBytes": 120 * 1024,
        }),
        FormNodeType::Location => json!({ "lat": -6.2, "lng": 106.816666, "accuracy": 8 }),
        _ => return None,
    })
}

fn fill_scope(fields: &[FormNode], time_zone: &str, row: usize, now: DateTime<Utc>) -> AnswerScope {
    let mut scope = Map::new();
    for node in fields {
        match node.ty {
            FormNodeType::Heading | FormNodeType::Paragraph | FormNodeType::Divider => {}
            FormNodeType::Section => {
                for (k, v) in fill_scope(node.children(), time_zone, row, now) {
                    scope.insert(k, v);
                }
            }
            FormNodeType::Group => {
                scope.insert(
                    node.key.clone(),
                    Value::Object(fill_scope(node.children(), time_zone, row, now)),
                );
            }
            FormNodeType::Repeater => {
                let count = match node.validation.as_ref().and_then(|v| v.max_rows) {
                    Some(max_rows) => REPEATER_SAMPLE_ROWS.min(max_rows.max(0.0)),
                    None => REPEATER_SAMPLE_ROWS,
                };
                // `Array.from({ length })` memotong pecahan.
                let count = if count.is_finite() {
                    count.trunc() as usize
                } else {
                    0
                };
                let rows = (0..count)
                    .map(|i| Value::Object(fill_scope(node.children(), time_zone, i, now)))
                    .collect();
                scope.insert(node.key.clone(), Value::Array(rows));
            }
            _ => {
                if let Some(v) = sample_leaf(node, time_zone, row, now) {
                    scope.insert(node.key.clone(), v);
                }
            }
        }
    }
    scope
}

/// `buildSampleAnswers(schema, timeZone)` (`form-pdf-preview.sample.ts:144`).
pub fn build_sample_answers(schema: &FormDocument, time_zone: &str) -> AnswerScope {
    build_sample_answers_at(schema, time_zone, Utc::now())
}

/// [`build_sample_answers`] dengan jam tetap (uji).
pub fn build_sample_answers_at(
    schema: &FormDocument,
    time_zone: &str,
    now: DateTime<Utc>,
) -> AnswerScope {
    fill_scope(&schema.fields, time_zone, 0, now)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema() -> FormDocument {
        FormDocument::from_value(&json!({
            "schemaVersion": 1,
            "fields": [
                {"key": "judul", "type": "heading", "label": "Judul"},
                {"key": "info", "type": "section", "label": "Info", "fields": [
                    {"key": "qty", "type": "number", "label": "Qty", "validation": {"min": 50, "max": 99}},
                    {"key": "status", "type": "select", "label": "Status", "options": {"source": "static",
                        "items": [{"value": "ok", "label": "OK"}, {"value": "gagal", "label": "Gagal"}]}}
                ]},
                {"key": "ttd", "type": "group", "label": "TTD", "fields": [{"key": "sales", "type": "signature", "label": "Sales"}]},
                {"key": "stok", "type": "repeater", "label": "Stok", "validation": {"minRows": 0, "maxRows": 1},
                 "fields": [{"key": "produk", "type": "text", "label": "Produk"}]}
            ]
        }))
    }

    #[test]
    fn fills_every_leaf() {
        let a = build_sample_answers(&schema(), "Asia/Jakarta");
        assert!(!a.contains_key("judul"));
        assert!(!a.contains_key("info"));
        assert_eq!(a["qty"], json!(50));
        assert_eq!(a["status"], json!("ok"));
        assert_eq!(
            a["ttd"],
            json!({"sales": {"fileUuid": PREVIEW_SIGNATURE_KEY}})
        );
        assert_eq!(a["stok"], json!([{"produk": "Contoh Produk"}]));
    }

    #[test]
    fn other_leaves() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-08-11T18:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let doc = FormDocument::from_value(&json!({"schemaVersion": 1, "fields": [
            {"key": "d", "type": "date"},
            {"key": "dt", "type": "datetime"},
            {"key": "r", "type": "rating", "config": {"max": 10}},
            {"key": "p", "type": "select", "options": {"source": "product"}},
            {"key": "m", "type": "multiselect", "options": {"source": "order"}},
            {"key": "rep", "type": "repeater", "fields": [
                {"key": "t", "type": "text", "label": "Unit"},
                {"key": "c", "type": "currency"}
            ]}
        ]}));
        let a = build_sample_answers_at(&doc, "Asia/Jakarta", now);
        assert_eq!(a["d"], json!("2026-08-12"));
        assert_eq!(a["dt"], json!(now.timestamp_millis()));
        assert_eq!(a["r"], json!(9));
        assert_eq!(a["p"], json!("Contoh pilihan"));
        assert_eq!(a["m"], json!(["Contoh pilihan"]));
        assert_eq!(
            a["rep"],
            json!([{"t": "Contoh Unit", "c": 150000}, {"t": "Contoh Unit 2", "c": 300000}])
        );
    }
}
