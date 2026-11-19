//! Port `form-answers.validator.ts`: validasi `answers` terhadap schema yang di-pin di
//! submission (§7.1): pangkas field tersembunyi (final), tolak key asing, tegakkan
//! `required`/batas, kumpulkan rujukan file & product/order. Pesan identik dengan Node.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use serde_json::{Map, Value};

use crate::consts::{FormNodeType, MAX_REPEATER_ROWS};
use crate::leaf_validator::{LeafValueOptions, js_num, validate_leaf_value_at};
use crate::types::{
    AnswerFileReference, AnswerProductOrderReference, FormAnswerIssue, FormDocument, FormNode,
    OptionsSource,
};
use crate::value_util::{is_empty_value, js_normalize};
use crate::visibility::{is_node_visible, prune_to_fixed_point};

/// `AnswerValidationMode` (`form-answers.validator.ts:31`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnswerValidationMode {
    Draft,
    Final,
}

/// `ValidateAnswersOptions` (`form-answers.validator.ts:33`).
#[derive(Debug, Clone, Copy)]
pub struct ValidateAnswersOptions<'a> {
    pub mode: AnswerValidationMode,
    /// Timezone company.
    pub time_zone: &'a str,
}

/// `ValidateAnswersResult` (`form-answers.validator.ts:45`).
#[derive(Debug, Clone, Default)]
pub struct ValidateAnswersResult {
    /// Jawaban ternormalisasi (`final`: field tersembunyi sudah dibuang).
    pub answers: Map<String, Value>,
    pub issues: Vec<FormAnswerIssue>,
    /// Setiap `fileUuid` yang bertahan, dengan jalur field-nya.
    pub files: Vec<AnswerFileReference>,
    /// Setiap rujukan product/order yang bertahan.
    pub product_order_refs: Vec<AnswerProductOrderReference>,
}

/// `answersByteSize(answers)` (`form-answers.validator.ts:56`) —
/// `Buffer.byteLength(JSON.stringify(answers ?? {}))`.
pub fn answers_byte_size(answers: Option<&Value>) -> usize {
    match answers {
        None | Some(Value::Null) => 2,
        Some(v) => crate::value_util::js_stringify(v).len(),
    }
}

/// Indeks array kanonik (`"0"`, `"12"`, < 2^32-1) — `Object.keys` menaruhnya lebih dulu.
fn array_index(k: &str) -> Option<u64> {
    if k.is_empty() || (k.len() > 1 && k.starts_with('0')) || !k.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    k.parse::<u64>().ok().filter(|n| *n < u64::from(u32::MAX))
}

/// `Object.keys(o)` dengan urutan JS: kunci indeks-array naik, lalu sisanya sesuai urutan sisip.
pub(crate) fn js_object_keys(o: &Map<String, Value>) -> Vec<&str> {
    let mut idx: Vec<(u64, &str)> = o
        .keys()
        .filter_map(|k| array_index(k).map(|n| (n, k.as_str())))
        .collect();
    idx.sort_by_key(|(n, _)| *n);
    let mut out: Vec<&str> = idx.into_iter().map(|(_, k)| k).collect();
    out.extend(
        o.keys()
            .map(String::as_str)
            .filter(|k| array_index(k).is_none()),
    );
    out
}

struct CheckContext<'a> {
    issues: Vec<FormAnswerIssue>,
    seen: HashSet<String>,
    files: Vec<AnswerFileReference>,
    product_order_refs: Vec<AnswerProductOrderReference>,
    mode: AnswerValidationMode,
    time_zone: &'a str,
    now: DateTime<Utc>,
}

impl CheckContext<'_> {
    fn report(&mut self, key: &str, message: &str) {
        let token = format!("{key} {message}");
        if !self.seen.insert(token) {
            return;
        }
        self.issues.push(FormAnswerIssue::new(key, message));
    }
}

/// Semua node data di satu scope, menembus `section`.
fn scope_data_nodes(fields: &[FormNode]) -> Vec<&FormNode> {
    fn walk<'a>(list: &'a [FormNode], out: &mut Vec<&'a FormNode>) {
        for node in list {
            if node.ty == FormNodeType::Section {
                walk(node.children(), out);
                continue;
            }
            if node.ty.is_display() {
                continue;
            }
            out.push(node);
        }
    }
    let mut out = Vec::new();
    walk(fields, &mut out);
    out
}

fn check_scope(
    ctx: &mut CheckContext<'_>,
    fields: &[FormNode],
    values: &Map<String, Value>,
    prefix: &str,
    parent_visible: bool,
) {
    let known: HashSet<&str> = scope_data_nodes(fields)
        .iter()
        .map(|n| n.key.as_str())
        .collect();
    for key in js_object_keys(values) {
        if !known.contains(key) {
            ctx.report(
                &format!("{prefix}{key}"),
                &format!("unknown field \"{key}\""),
            );
        }
    }
    for node in fields {
        visit(ctx, node, values, prefix, parent_visible);
    }
}

fn visit(
    ctx: &mut CheckContext<'_>,
    node: &FormNode,
    values: &Map<String, Value>,
    prefix: &str,
    visible_so_far: bool,
) {
    if node.ty.is_display() {
        return;
    }
    let is_final = ctx.mode == AnswerValidationMode::Final;
    let visible = if is_final {
        visible_so_far && is_node_visible(node, values)
    } else {
        true
    };

    if node.ty == FormNodeType::Section {
        for child in node.children() {
            visit(ctx, child, values, prefix, visible);
        }
        return;
    }

    let key = format!("{prefix}{}", node.key);
    let value = values.get(&node.key);

    if is_final && !visible {
        return;
    }

    match node.ty {
        FormNodeType::Group => {
            match value {
                None | Some(Value::Null) => {
                    if is_final {
                        check_scope(ctx, node.children(), &Map::new(), &format!("{key}."), true);
                    }
                }
                Some(Value::Object(child)) => {
                    check_scope(ctx, node.children(), child, &format!("{key}."), true);
                }
                Some(_) => ctx.report(&key, "value must be an object"),
            }
            return;
        }
        FormNodeType::Repeater => {
            let empty = Vec::new();
            let rows = match value {
                None | Some(Value::Null) => &empty,
                Some(Value::Array(rows)) => rows,
                Some(_) => {
                    ctx.report(&key, "value must be an array of rows");
                    return;
                }
            };
            let rules = node.validation.as_ref();
            let cap = MAX_REPEATER_ROWS as f64;
            let max_rows = rules.and_then(|r| r.max_rows).unwrap_or(cap).min(cap);
            let len = rows.len() as f64;
            if len > max_rows {
                ctx.report(
                    &key,
                    &format!("at most {} rows are allowed", js_num(max_rows)),
                );
            }
            if is_final {
                if node.is_required() && rows.is_empty() {
                    ctx.report(&key, "at least one row is required");
                }
                if let Some(min) = rules.and_then(|r| r.min_rows)
                    && len < min
                {
                    ctx.report(&key, &format!("at least {} rows are required", js_num(min)));
                }
            }
            for (index, row) in rows.iter().enumerate() {
                match row {
                    Value::Object(row) => {
                        check_scope(ctx, node.children(), row, &format!("{key}.{index}."), true);
                    }
                    _ => ctx.report(&format!("{key}.{index}"), "row must be an object"),
                }
            }
            return;
        }
        _ => {}
    }

    // Daun.
    let Some(value) = value.filter(|v| !is_empty_value(Some(v))) else {
        if is_final && node.is_required() {
            ctx.report(&key, "this field is required");
        }
        return;
    };

    let opts = LeafValueOptions {
        time_zone: ctx.time_zone,
        enforce_lower_bounds: is_final,
    };
    for message in validate_leaf_value_at(node, value, &opts, ctx.now) {
        ctx.report(&key, &message);
    }

    if node.ty.is_media()
        && let Some(file_uuid) = value.get("fileUuid").and_then(Value::as_str)
        && value.is_object()
    {
        ctx.files.push(AnswerFileReference {
            file_uuid: file_uuid.to_owned(),
            field_path: key.clone(),
        });
    }

    if (node.ty.is_single_choice() || node.ty.is_multi_choice())
        && let Some(options) = &node.options
        && options.source != OptionsSource::Static
    {
        let ids = match value {
            Value::Array(items) => items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
            Value::String(s) => vec![s.clone()],
            _ => Vec::new(),
        };
        ctx.product_order_refs.push(AnswerProductOrderReference {
            source: options.source,
            field_path: key,
            ids,
        });
    }
}

/// `validateAnswers(schema, input, options)` (`form-answers.validator.ts:223`).
/// `input = None` = `undefined`.
pub fn validate_answers(
    schema: &FormDocument,
    input: Option<&Value>,
    options: &ValidateAnswersOptions<'_>,
) -> ValidateAnswersResult {
    validate_answers_at(schema, input, options, Utc::now())
}

/// [`validate_answers`] dengan jam tetap (uji).
pub fn validate_answers_at(
    schema: &FormDocument,
    input: Option<&Value>,
    options: &ValidateAnswersOptions<'_>,
    now: DateTime<Utc>,
) -> ValidateAnswersResult {
    let mut answers = match input {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(o)) => match js_normalize(&Value::Object(o.clone())) {
            Value::Object(o) => o,
            _ => Map::new(),
        },
        Some(_) => {
            return ValidateAnswersResult {
                issues: vec![FormAnswerIssue::new("", "answers must be an object")],
                ..Default::default()
            };
        }
    };
    if options.mode == AnswerValidationMode::Final {
        prune_to_fixed_point(schema, &mut answers);
    }
    let mut ctx = CheckContext {
        issues: Vec::new(),
        seen: HashSet::new(),
        files: Vec::new(),
        product_order_refs: Vec::new(),
        mode: options.mode,
        time_zone: options.time_zone,
        now,
    };
    check_scope(&mut ctx, &schema.fields, &answers, "", true);
    ValidateAnswersResult {
        answers,
        issues: ctx.issues,
        files: ctx.files,
        product_order_refs: ctx.product_order_refs,
    }
}

#[cfg(test)]
#[path = "answers_validator_tests.rs"]
mod tests;
