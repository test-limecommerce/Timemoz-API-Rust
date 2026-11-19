//! Port `form-schema.validator.ts`: validator schema form (§2.8). Murni — tidak
//! menyentuh database, aman dijalankan di dalam transaksi publish.
//!
//! Input admin tidak tepercaya, jadi semuanya dibaca dari `serde_json::Value` dengan
//! semantik JS (`typeof`, `Number.isInteger`, `Array.isArray`, `hasOwn`). Pesan & `path`
//! identik dengan Node, begitu pula urutan issue.

use std::collections::HashMap;

use serde_json::{Map, Value};

use crate::consts::{
    FORM_SCHEMA_VERSION, FormContainerContext, FormFieldFormat, FormNodeCategory, FormNodeType,
    GRID_COLUMNS, GROUP_DIRECTIONS, GROUP_JUSTIFY, LAYOUT_SECTIONS, MAX_DESCRIPTION_LENGTH,
    MAX_FILE_BYTES, MAX_LABEL_LENGTH, MAX_NODES_PER_TEMPLATE, MAX_OPTION_LABEL_LENGTH,
    MAX_OPTIONS_PER_FIELD, MAX_PLACEHOLDER_LENGTH, MAX_REPEATER_ROWS, OPTIONS_SOURCES,
    REPEATER_RENDER_AS, VisibleIfOp, is_form_key,
};
use crate::layout_validator::{is_hex_color, validate_layout_sections};
use crate::leaf_validator::{LeafValueOptions, validate_leaf_value};
use crate::types::{FormNode, FormSchemaIssue};
use crate::value_util::{
    is_clock_time, is_integer, js_trim, resolve_date_bound, resolve_instant_bound, text_length,
};

/// Batas jumlah kondisi per blok `visibleIf` (`form-schema.validator.ts:54`).
pub const MAX_VISIBLE_IF_CONDITIONS: usize = 20;

const COMMON_NODE_PROPERTIES: &[&str] = &[
    "key",
    "type",
    "label",
    "description",
    "placeholder",
    "required",
    "defaultValue",
    "layout",
    "visibleIf",
    "validation",
    "options",
    "config",
];

/// `ValidateFormSchemaOptions` (`form-schema.validator.ts:126`).
#[derive(Debug, Clone, Copy, Default)]
pub struct ValidateFormSchemaOptions<'a> {
    /// Versi terpublish terakhir (diff `key`↔`type`); `None` = belum ada.
    pub previous_schema: Option<&'a Value>,
    /// Timezone company (bawaan `UTC`).
    pub time_zone: Option<&'a str>,
}

struct Collected<'a> {
    raw: &'a Map<String, Value>,
    key: String,
    ty: FormNodeType,
    path: String,
    data_path: Option<String>,
    scope: usize,
    container_path: Option<String>,
}

struct Ctx<'a> {
    issues: Vec<FormSchemaIssue>,
    collected: Vec<Collected<'a>>,
    /// Scope data → key → indeks `collected`.
    scopes: Vec<HashMap<String, usize>>,
    node_count: usize,
    time_zone: String,
}

impl Ctx<'_> {
    fn push(&mut self, path: impl Into<String>, message: impl Into<String>) {
        self.issues.push(FormSchemaIssue::new(path, message));
    }
}

// ── Pembantu semantik JS ─────────────────────────────────────────────────────

/// `isInteger(value)` → nilainya.
fn int_of(v: Option<&Value>) -> Option<f64> {
    v.and_then(Value::as_f64).filter(|x| is_integer(*x))
}

fn is_non_empty_string(v: Option<&Value>) -> bool {
    matches!(v, Some(Value::String(s)) if !js_trim(s).is_empty())
}

/// `String(value)`.
fn js_string(v: Option<&Value>) -> String {
    match v {
        None => "undefined".into(),
        Some(Value::Null) => "null".into(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => n
            .as_f64()
            .map_or_else(|| n.to_string(), mz_core::jsfmt::num_string),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) => a
            .iter()
            .map(|x| match x {
                Value::Null => String::new(),
                other => js_string(Some(other)),
            })
            .collect::<Vec<_>>()
            .join(","),
        Some(Value::Object(_)) => "[object Object]".into(),
    }
}

/// `list.includes(value)` untuk daftar string (kesetaraan ketat).
fn str_in(list: &[&str], v: Option<&Value>) -> bool {
    matches!(v, Some(Value::String(s)) if list.contains(&s.as_str()))
}

/// Truthiness JS.
fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|x| x != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

/// `x?.prop` untuk nilai JSON apa pun.
fn prop<'a>(v: Option<&'a Value>, key: &str) -> Option<&'a Value> {
    v.and_then(Value::as_object).and_then(|o| o.get(key))
}

/// `ISO_MIME_PATTERN` `/^[a-z0-9!#$&^_.+-]+\/(?:\*|[a-z0-9!#$&^_.+-]+)$/i`.
fn is_iso_mime(s: &str) -> bool {
    let tok = |t: &str| {
        !t.is_empty()
            && t.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$&^_.+-".contains(&b))
    };
    match s.split_once('/') {
        Some((a, b)) => tok(a) && (b == "*" || tok(b)),
        None => false,
    }
}

/// `CURRENCY_CODE_PATTERN` `^[A-Z]{3}$`.
fn is_currency_code(s: &str) -> bool {
    s.len() == 3 && s.bytes().all(|b| b.is_ascii_uppercase())
}

/// `ASPECT_RATIO_PATTERN` `^\d{1,2}:\d{1,2}$`.
fn is_aspect_ratio(s: &str) -> bool {
    let part = |p: &str| (1..=2).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_digit());
    matches!(s.split_once(':'), Some((a, b)) if part(a) && part(b))
}

// ── Blok `validation` ────────────────────────────────────────────────────────

fn validate_date_bound_shape(
    ctx: &mut Ctx,
    path: &str,
    ty: FormNodeType,
    key: &str,
    bound: &Value,
) {
    let Some(bound) = bound.as_str() else {
        ctx.push(
            format!("{path}.validation.{key}"),
            format!("{key} must be a string"),
        );
        return;
    };
    if ty == FormNodeType::Time {
        if !is_clock_time(bound) {
            ctx.push(
                format!("{path}.validation.{key}"),
                format!("{key} must be an HH:mm time"),
            );
        }
        return;
    }
    if ty == FormNodeType::Date {
        if resolve_date_bound(bound, &ctx.time_zone).is_none() {
            ctx.push(
                format!("{path}.validation.{key}"),
                format!("{key} must be a YYYY-MM-DD date or a today/today±Nd token"),
            );
        }
        return;
    }
    if resolve_instant_bound(bound, &ctx.time_zone, false).is_none() {
        ctx.push(
            format!("{path}.validation.{key}"),
            format!(
                "{key} must be an ISO 8601 instant, a YYYY-MM-DD date, or a today/today±Nd token"
            ),
        );
    }
}

fn validate_validation_block(ctx: &mut Ctx, path: &str, ty: FormNodeType, raw: Option<&Value>) {
    let spec = ty.spec();
    let Some(raw) = raw else {
        if ty == FormNodeType::Repeater {
            ctx.push(
                format!("{path}.validation"),
                "repeater requires validation.minRows and validation.maxRows",
            );
        }
        return;
    };
    let Some(v) = raw.as_object() else {
        ctx.push(format!("{path}.validation"), "validation must be an object");
        return;
    };

    for key in v.keys() {
        if !spec.allowed_validation.contains(&key.as_str()) {
            let message = if ty == FormNodeType::Rating && (key == "min" || key == "max") {
                "rating uses config.max; validation.min/max are not allowed".to_owned()
            } else {
                format!(
                    "validation.{key} is not allowed on type \"{}\"",
                    ty.as_str()
                )
            };
            ctx.push(format!("{path}.validation.{key}"), message);
        }
    }

    let num = |ctx: &mut Ctx, key: &str| -> Option<f64> {
        let value = v.get(key)?;
        let n = int_of(Some(value));
        if n.is_none() {
            ctx.push(
                format!("{path}.validation.{key}"),
                format!("{key} must be an integer"),
            );
        }
        n
    };

    let min_length = num(ctx, "minLength");
    let max_length = num(ctx, "maxLength");
    if min_length.is_some_and(|x| x < 0.0) {
        ctx.push(
            format!("{path}.validation.minLength"),
            "minLength must be 0 or more",
        );
    }
    if max_length.is_some_and(|x| x < 1.0) {
        ctx.push(
            format!("{path}.validation.maxLength"),
            "maxLength must be 1 or more",
        );
    }
    if let (Some(a), Some(b)) = (min_length, max_length)
        && a > b
    {
        ctx.push(
            format!("{path}.validation"),
            "minLength must not be greater than maxLength",
        );
    }

    if let Some(format) = v.get("format")
        && !format
            .as_str()
            .is_some_and(|f| FormFieldFormat::parse(f).is_some())
    {
        let all: Vec<&str> = FormFieldFormat::ALL.iter().map(|f| f.as_str()).collect();
        ctx.push(
            format!("{path}.validation.format"),
            format!("format must be one of: {}", all.join(", ")),
        );
    }

    let min = v.get("min");
    let max = v.get("max");
    for (key, value) in [("min", min), ("max", max)] {
        if value.is_some() && value.and_then(Value::as_f64).is_none_or(|x| !x.is_finite()) {
            ctx.push(
                format!("{path}.validation.{key}"),
                format!("{key} must be a finite number"),
            );
        }
    }
    if let (Some(a), Some(b)) = (min.and_then(Value::as_f64), max.and_then(Value::as_f64))
        && a.is_finite()
        && b.is_finite()
        && a > b
    {
        ctx.push(
            format!("{path}.validation"),
            "min must not be greater than max",
        );
    }

    let decimals = num(ctx, "decimals");
    if decimals.is_some_and(|d| !(0.0..=6.0).contains(&d)) {
        ctx.push(
            format!("{path}.validation.decimals"),
            "decimals must be between 0 and 6",
        );
    }

    for key in ["minDate", "maxDate"] {
        if let Some(bound) = v.get(key) {
            validate_date_bound_shape(ctx, path, ty, key, bound);
        }
    }

    let min_items = num(ctx, "minItems");
    let max_items = num(ctx, "maxItems");
    if min_items.is_some_and(|x| x < 0.0) {
        ctx.push(
            format!("{path}.validation.minItems"),
            "minItems must be 0 or more",
        );
    }
    if max_items.is_some_and(|x| x < 1.0) {
        ctx.push(
            format!("{path}.validation.maxItems"),
            "maxItems must be 1 or more",
        );
    }
    if let (Some(a), Some(b)) = (min_items, max_items)
        && a > b
    {
        ctx.push(
            format!("{path}.validation"),
            "minItems must not be greater than maxItems",
        );
    }

    if ty == FormNodeType::Repeater {
        let min_rows = num(ctx, "minRows");
        let max_rows = num(ctx, "maxRows");
        if !v.contains_key("minRows") {
            ctx.push(
                format!("{path}.validation.minRows"),
                "repeater requires minRows",
            );
        }
        if !v.contains_key("maxRows") {
            ctx.push(
                format!("{path}.validation.maxRows"),
                "repeater requires maxRows",
            );
        }
        if min_rows.is_some_and(|x| x < 0.0) {
            ctx.push(
                format!("{path}.validation.minRows"),
                "minRows must be 0 or more",
            );
        }
        if max_rows.is_some_and(|x| x < 1.0 || x > MAX_REPEATER_ROWS as f64) {
            ctx.push(
                format!("{path}.validation.maxRows"),
                format!("maxRows must be between 1 and {MAX_REPEATER_ROWS}"),
            );
        }
        if let (Some(a), Some(b)) = (min_rows, max_rows)
            && a > b
        {
            ctx.push(
                format!("{path}.validation"),
                "minRows must not be greater than maxRows",
            );
        }
    }

    let max_size = num(ctx, "maxSizeBytes");
    if max_size.is_some_and(|x| x < 1.0 || x > MAX_FILE_BYTES as f64) {
        ctx.push(
            format!("{path}.validation.maxSizeBytes"),
            format!("maxSizeBytes must be between 1 and {MAX_FILE_BYTES}"),
        );
    }

    if let Some(accept) = v.get("accept") {
        match accept.as_array() {
            Some(list) if !list.is_empty() && list.len() <= 20 => {
                for entry in list {
                    if !entry.as_str().is_some_and(is_iso_mime) {
                        ctx.push(
                            format!("{path}.validation.accept"),
                            format!("\"{}\" is not a valid media type", js_string(Some(entry))),
                        );
                    }
                }
            }
            _ => ctx.push(
                format!("{path}.validation.accept"),
                "accept must be an array of 1 to 20 media types",
            ),
        }
    }
}

// ── Blok `config` ────────────────────────────────────────────────────────────

fn validate_config_block(ctx: &mut Ctx, path: &str, ty: FormNodeType, raw: Option<&Value>) {
    let Some(raw) = raw else {
        return;
    };
    let Some(c) = raw.as_object() else {
        ctx.push(format!("{path}.config"), "config must be an object");
        return;
    };
    let allowed = ty.spec().allowed_config;
    for key in c.keys() {
        if !allowed.contains(&key.as_str()) {
            ctx.push(
                format!("{path}.config.{key}"),
                format!("config.{key} is not allowed on type \"{}\"", ty.as_str()),
            );
        }
    }

    if c.contains_key("rows") && int_of(c.get("rows")).is_none_or(|r| !(1.0..=50.0).contains(&r)) {
        ctx.push(
            format!("{path}.config.rows"),
            "rows must be an integer between 1 and 50",
        );
    }
    if let Some(code) = c.get("currencyCode")
        && !code.as_str().is_some_and(is_currency_code)
    {
        ctx.push(
            format!("{path}.config.currencyCode"),
            "currencyCode must be a 3-letter uppercase code",
        );
    }
    if c.contains_key("max") && int_of(c.get("max")).is_none_or(|m| !(2.0..=10.0).contains(&m)) {
        ctx.push(
            format!("{path}.config.max"),
            "max must be an integer between 2 and 10",
        );
    }
    if let Some(pen) = c.get("penColor")
        && !pen.as_str().is_some_and(is_hex_color)
    {
        ctx.push(
            format!("{path}.config.penColor"),
            "penColor must be a #rrggbb colour",
        );
    }
    if let Some(ratio) = c.get("aspectRatio")
        && !ratio.as_str().is_some_and(is_aspect_ratio)
    {
        ctx.push(
            format!("{path}.config.aspectRatio"),
            "aspectRatio must look like \"3:1\"",
        );
    }
    let render_as = c.get("renderAs");
    if render_as.is_some() && !str_in(REPEATER_RENDER_AS, render_as) {
        ctx.push(
            format!("{path}.config.renderAs"),
            format!("renderAs must be one of: {}", REPEATER_RENDER_AS.join(", ")),
        );
    }
    let direction = c.get("direction");
    if direction.is_some() && !str_in(GROUP_DIRECTIONS, direction) {
        ctx.push(
            format!("{path}.config.direction"),
            format!("direction must be one of: {}", GROUP_DIRECTIONS.join(", ")),
        );
    }
    let justify = c.get("justify");
    if justify.is_some() {
        if !str_in(GROUP_JUSTIFY, justify) {
            ctx.push(
                format!("{path}.config.justify"),
                format!("justify must be one of: {}", GROUP_JUSTIFY.join(", ")),
            );
        } else if direction.and_then(Value::as_str) != Some("row") {
            ctx.push(
                format!("{path}.config.justify"),
                "justify only applies when direction is \"row\"",
            );
        }
    }
}

// ── Blok `options` ───────────────────────────────────────────────────────────

fn validate_options_block(ctx: &mut Ctx, path: &str, ty: FormNodeType, raw: Option<&Value>) {
    let spec = ty.spec();
    let Some(raw) = raw else {
        if spec.requires_options {
            ctx.push(
                format!("{path}.options"),
                format!("type \"{}\" requires options", ty.as_str()),
            );
        }
        return;
    };
    if !spec.requires_options {
        ctx.push(
            format!("{path}.options"),
            format!("type \"{}\" does not accept options", ty.as_str()),
        );
        return;
    }
    let Some(o) = raw.as_object() else {
        ctx.push(format!("{path}.options"), "options must be an object");
        return;
    };
    for key in o.keys() {
        if key != "source" && key != "items" {
            ctx.push(
                format!("{path}.options.{key}"),
                format!("unknown options property \"{key}\""),
            );
        }
    }
    let source = o.get("source");
    if !str_in(OPTIONS_SOURCES, source) {
        ctx.push(
            format!("{path}.options.source"),
            format!("source must be one of: {}", OPTIONS_SOURCES.join(", ")),
        );
    }
    if matches!(source.and_then(Value::as_str), Some("product" | "order")) {
        if o.contains_key("items") {
            ctx.push(
                format!("{path}.options.items"),
                "options.items must be absent when source is \"product\" or \"order\"",
            );
        }
        return;
    }
    let Some(items) = o.get("items").and_then(Value::as_array) else {
        ctx.push(format!("{path}.options.items"), "items must be an array");
        return;
    };
    if items.is_empty() {
        ctx.push(format!("{path}.options.items"), "items must not be empty");
    }
    if items.len() > MAX_OPTIONS_PER_FIELD {
        ctx.push(
            format!("{path}.options.items"),
            format!("items must not exceed {MAX_OPTIONS_PER_FIELD} entries"),
        );
    }
    let mut seen: Vec<&str> = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let item_path = format!("{path}.options.items[{index}]");
        let Some(it) = item.as_object() else {
            ctx.push(item_path, "option must be an object");
            continue;
        };
        for key in it.keys() {
            if key != "value" && key != "label" {
                ctx.push(
                    format!("{item_path}.{key}"),
                    format!("unknown option property \"{key}\""),
                );
            }
        }
        match it.get("value").and_then(Value::as_str) {
            Some(value) if is_form_key(value) => {
                if seen.contains(&value) {
                    ctx.push(
                        format!("{item_path}.value"),
                        format!("duplicate option value \"{value}\""),
                    );
                }
                seen.push(value);
            }
            _ => ctx.push(
                format!("{item_path}.value"),
                "value must match ^[a-z][a-z0-9_]{0,39}$",
            ),
        }
        let label = it.get("label");
        if !is_non_empty_string(label) {
            ctx.push(format!("{item_path}.label"), "label is required");
        } else if label
            .and_then(Value::as_str)
            .is_some_and(|l| text_length(l) > MAX_OPTION_LABEL_LENGTH)
        {
            ctx.push(
                format!("{item_path}.label"),
                format!("label must be at most {MAX_OPTION_LABEL_LENGTH} characters"),
            );
        }
    }
}

// ── Pass 1: struktur & pengumpulan ───────────────────────────────────────────

fn validate_node_shell(ctx: &mut Ctx, path: &str, raw: &Map<String, Value>, ty: FormNodeType) {
    let category = ty.category();
    for key in raw.keys() {
        let allowed = COMMON_NODE_PROPERTIES.contains(&key.as_str())
            || (category == FormNodeCategory::Container && key == "fields");
        if !allowed {
            ctx.push(
                format!("{path}.{key}"),
                format!("unknown property \"{key}\""),
            );
        }
    }

    let label = raw.get("label");
    if ty == FormNodeType::Divider {
        if label.is_some() {
            ctx.push(format!("{path}.label"), "divider does not take a label");
        }
    } else if !is_non_empty_string(label) {
        ctx.push(format!("{path}.label"), "label is required");
    } else if label
        .and_then(Value::as_str)
        .is_some_and(|l| text_length(l) > MAX_LABEL_LENGTH)
    {
        ctx.push(
            format!("{path}.label"),
            format!("label must be at most {MAX_LABEL_LENGTH} characters"),
        );
    }

    if let Some(description) = raw.get("description") {
        match description.as_str() {
            None => ctx.push(
                format!("{path}.description"),
                "description must be a string",
            ),
            Some(d) if text_length(d) > MAX_DESCRIPTION_LENGTH => ctx.push(
                format!("{path}.description"),
                format!("description must be at most {MAX_DESCRIPTION_LENGTH} characters"),
            ),
            Some(_) => {}
        }
    }

    if let Some(placeholder) = raw.get("placeholder") {
        if !(ty.is_text_like() || ty.is_number_like()) {
            ctx.push(
                format!("{path}.placeholder"),
                "placeholder is only allowed on text and number fields",
            );
        } else {
            match placeholder.as_str() {
                None => ctx.push(
                    format!("{path}.placeholder"),
                    "placeholder must be a string",
                ),
                Some(p) if text_length(p) > MAX_PLACEHOLDER_LENGTH => ctx.push(
                    format!("{path}.placeholder"),
                    format!("placeholder must be at most {MAX_PLACEHOLDER_LENGTH} characters"),
                ),
                Some(_) => {}
            }
        }
    }

    if let Some(required) = raw.get("required") {
        let forbidden = category == FormNodeCategory::Display
            || ty == FormNodeType::Section
            || ty == FormNodeType::Group;
        if forbidden {
            ctx.push(
                format!("{path}.required"),
                format!("required is not allowed on type \"{}\"", ty.as_str()),
            );
        } else if !required.is_boolean() {
            ctx.push(format!("{path}.required"), "required must be a boolean");
        }
    }

    if let Some(layout) = raw.get("layout") {
        match layout.as_object() {
            None => ctx.push(format!("{path}.layout"), "layout must be an object"),
            Some(l) => {
                for key in l.keys() {
                    if key != "colSpan" {
                        ctx.push(
                            format!("{path}.layout.{key}"),
                            format!("unknown layout property \"{key}\""),
                        );
                    }
                }
                if l.contains_key("colSpan")
                    && int_of(l.get("colSpan")).is_none_or(|c| c < 1.0 || c > GRID_COLUMNS as f64)
                {
                    ctx.push(
                        format!("{path}.layout.colSpan"),
                        format!("colSpan must be an integer between 1 and {GRID_COLUMNS}"),
                    );
                }
            }
        }
    }
}

fn validate_default_value(ctx: &mut Ctx, path: &str, raw: &Map<String, Value>, ty: FormNodeType) {
    let Some(default) = raw.get("defaultValue") else {
        return;
    };
    if ty.category() != FormNodeCategory::Leaf {
        ctx.push(
            format!("{path}.defaultValue"),
            format!("defaultValue is not allowed on type \"{}\"", ty.as_str()),
        );
        return;
    }
    let options = raw.get("options");
    if truthy(options) && prop(options, "source").and_then(Value::as_str) != Some("static") {
        ctx.push(
            format!("{path}.defaultValue"),
            "defaultValue is not allowed when options.source is \"product\" or \"order\"",
        );
        return;
    }
    // Batas tanggal sengaja tidak ikut diperiksa (`maxDate: "today"` bergerak).
    let mut stripped = raw.clone();
    if let Some(Value::Object(v)) = stripped.get_mut("validation") {
        v.remove("minDate");
        v.remove("maxDate");
    }
    let Some(node) = FormNode::from_value(&Value::Object(stripped)) else {
        return;
    };
    let opts = LeafValueOptions {
        time_zone: &ctx.time_zone,
        enforce_lower_bounds: true,
    };
    let messages = validate_leaf_value(&node, default, &opts);
    for message in messages {
        ctx.push(format!("{path}.defaultValue"), message);
    }
}

struct Container<'p> {
    context: FormContainerContext,
    scope: usize,
    data_prefix: &'p str,
    container_path: Option<&'p str>,
    path: &'p str,
}

fn walk_fields<'a>(ctx: &mut Ctx<'a>, fields: Option<&'a Value>, container: Container<'_>) {
    let Some(fields) = fields.and_then(Value::as_array) else {
        ctx.push(
            format!("{}.fields", container.path),
            "fields must be an array",
        );
        return;
    };
    if fields.is_empty() {
        ctx.push(
            format!("{}.fields", container.path),
            "fields must not be empty",
        );
        return;
    }

    for (index, raw) in fields.iter().enumerate() {
        let path = format!("{}.fields[{index}]", container.path);
        ctx.node_count += 1;
        if ctx.node_count > MAX_NODES_PER_TEMPLATE {
            if ctx.node_count == MAX_NODES_PER_TEMPLATE + 1 {
                ctx.push(
                    "",
                    format!("schema must not exceed {MAX_NODES_PER_TEMPLATE} nodes in total"),
                );
            }
            continue;
        }
        let Some(obj) = raw.as_object() else {
            ctx.push(path, "node must be an object");
            continue;
        };
        let type_value = obj.get("type");
        let Some(ty) = type_value
            .and_then(Value::as_str)
            .and_then(FormNodeType::parse)
        else {
            ctx.push(
                format!("{path}.type"),
                format!("unknown node type \"{}\"", js_string(type_value)),
            );
            continue;
        };
        if !container.context.allows(ty) {
            ctx.push(
                format!("{path}.type"),
                format!(
                    "type \"{}\" is not allowed inside {}",
                    ty.as_str(),
                    container.context.as_str()
                ),
            );
            continue;
        }
        let key = match obj.get("key").and_then(Value::as_str) {
            Some(k) if is_form_key(k) => k.to_owned(),
            _ => {
                ctx.push(
                    format!("{path}.key"),
                    "key must match ^[a-z][a-z0-9_]{0,39}$",
                );
                continue;
            }
        };
        if ctx
            .scopes
            .get(container.scope)
            .is_some_and(|s| s.contains_key(&key))
        {
            ctx.push(
                format!("{path}.key"),
                format!("duplicate key \"{key}\" in this scope"),
            );
            continue;
        }

        validate_node_shell(ctx, &path, obj, ty);
        validate_validation_block(ctx, &path, ty, obj.get("validation"));
        validate_config_block(ctx, &path, ty, obj.get("config"));
        validate_options_block(ctx, &path, ty, obj.get("options"));
        validate_default_value(ctx, &path, obj, ty);

        let is_section = ty == FormNodeType::Section;
        let data_path =
            (!ty.is_display() && !is_section).then(|| format!("{}{key}", container.data_prefix));

        let index_in_collected = ctx.collected.len();
        ctx.collected.push(Collected {
            raw: obj,
            key: key.clone(),
            ty,
            path: path.clone(),
            data_path: data_path.clone(),
            scope: container.scope,
            container_path: container.container_path.map(str::to_owned),
        });
        if let Some(scope) = ctx.scopes.get_mut(container.scope) {
            scope.insert(key, index_in_collected);
        }

        if !ty.is_container() {
            continue;
        }
        let children = obj.get("fields");
        if is_section {
            walk_fields(
                ctx,
                children,
                Container {
                    context: FormContainerContext::Section,
                    scope: container.scope,
                    data_prefix: container.data_prefix,
                    container_path: Some(&path),
                    path: &path,
                },
            );
            continue;
        }
        let dp = data_path.unwrap_or_default();
        let (context, prefix) = if ty == FormNodeType::Group {
            (FormContainerContext::Group, format!("{dp}."))
        } else {
            (FormContainerContext::Repeater, format!("{dp}[]."))
        };
        ctx.scopes.push(HashMap::new());
        let scope = ctx.scopes.len() - 1;
        walk_fields(
            ctx,
            children,
            Container {
                context,
                scope,
                data_prefix: &prefix,
                container_path: Some(&path),
                path: &path,
            },
        );
    }
}

// ── Pass 2: rujukan `visibleIf` ──────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    String,
    Number,
    Boolean,
    StringArray,
}

fn target_value_kind(ty: FormNodeType) -> Option<Kind> {
    if ty.is_single_choice() {
        return Some(Kind::String);
    }
    if ty.is_multi_choice() {
        return Some(Kind::StringArray);
    }
    if ty.is_text_like() {
        return Some(Kind::String);
    }
    if ty.is_number_like() {
        return Some(Kind::Number);
    }
    match ty {
        FormNodeType::Date | FormNodeType::Time => Some(Kind::String),
        FormNodeType::Datetime | FormNodeType::Rating => Some(Kind::Number),
        FormNodeType::Boolean => Some(Kind::Boolean),
        _ => None,
    }
}

fn is_list_op(op: VisibleIfOp) -> bool {
    matches!(op, VisibleIfOp::In | VisibleIfOp::Nin)
}

fn is_presence_op(op: VisibleIfOp) -> bool {
    matches!(op, VisibleIfOp::Empty | VisibleIfOp::NotEmpty)
}

fn is_ordered_op(op: VisibleIfOp) -> bool {
    matches!(
        op,
        VisibleIfOp::Gt | VisibleIfOp::Gte | VisibleIfOp::Lt | VisibleIfOp::Lte
    )
}

fn check_condition_value_kind(
    ctx: &mut Ctx,
    path: &str,
    target: usize,
    op: VisibleIfOp,
    value: &Value,
) {
    let Some((t_key, t_ty, t_raw)) = ctx
        .collected
        .get(target)
        .map(|t| (t.key.clone(), t.ty, t.raw))
    else {
        return;
    };
    let Some(kind) = target_value_kind(t_ty) else {
        return;
    };
    let matches = |c: &Value| match kind {
        Kind::StringArray | Kind::String => c.is_string(),
        Kind::Number => c.as_f64().is_some_and(f64::is_finite),
        Kind::Boolean => c.is_boolean(),
    };
    let single = [value.clone()];
    let candidates: &[Value] = if is_list_op(op) {
        value.as_array().map_or(&[][..], Vec::as_slice)
    } else if kind == Kind::StringArray {
        value.as_array().map_or(&single[..], Vec::as_slice)
    } else {
        &single[..]
    };

    if candidates.iter().any(|c| !matches(c)) {
        let expected = match kind {
            Kind::StringArray => "a string or array of strings",
            Kind::String => "a string",
            Kind::Number => "a number",
            Kind::Boolean => "a boolean",
        };
        ctx.push(path, format!("value for \"{t_key}\" must be {expected}"));
        return;
    }

    let options = t_raw.get("options");
    let source = prop(options, "source");
    if (t_ty.is_single_choice() || t_ty.is_multi_choice())
        && (!truthy(source) || source.and_then(Value::as_str) == Some("static"))
    {
        let allowed: Vec<&Value> = prop(options, "items")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(|i| i.get("value")).collect())
            .unwrap_or_default();
        for candidate in candidates {
            if let Some(s) = candidate.as_str()
                && !allowed.iter().any(|a| a.as_str() == Some(s))
            {
                ctx.push(path, format!("\"{s}\" is not an option of \"{t_key}\""));
            }
        }
    }
}

fn validate_condition(
    ctx: &mut Ctx,
    owner: usize,
    path: &str,
    raw: &Value,
    edges: &mut Vec<String>,
) {
    let Some(c) = raw.as_object() else {
        ctx.push(path, "condition must be an object");
        return;
    };
    for key in c.keys() {
        if key != "field" && key != "op" && key != "value" {
            ctx.push(
                format!("{path}.{key}"),
                format!("unknown condition property \"{key}\""),
            );
        }
    }
    let Some(field) = c.get("field").and_then(Value::as_str) else {
        ctx.push(format!("{path}.field"), "field must be a string");
        return;
    };
    let op_value = c.get("op");
    let Some(op) = op_value
        .and_then(Value::as_str)
        .and_then(VisibleIfOp::parse)
    else {
        ctx.push(
            format!("{path}.op"),
            format!("unknown operator \"{}\"", js_string(op_value)),
        );
        return;
    };
    let Some((owner_scope, owner_path)) =
        ctx.collected.get(owner).map(|o| (o.scope, o.path.clone()))
    else {
        return;
    };
    let Some(target) = ctx
        .scopes
        .get(owner_scope)
        .and_then(|s| s.get(field))
        .copied()
    else {
        ctx.push(
            format!("{path}.field"),
            format!("\"{field}\" is not a field in the same data scope"),
        );
        return;
    };
    let Some((t_path, t_ty)) = ctx.collected.get(target).map(|t| (t.path.clone(), t.ty)) else {
        return;
    };
    if t_path == owner_path {
        ctx.push(format!("{path}.field"), "a field cannot depend on itself");
        return;
    }
    if t_ty == FormNodeType::Section {
        ctx.push(
            format!("{path}.field"),
            "a section has no value and cannot be referenced",
        );
        return;
    }
    if t_ty.is_display() {
        ctx.push(
            format!("{path}.field"),
            format!("\"{field}\" is a display node and has no value"),
        );
        return;
    }
    let op_str = op.as_str();
    if t_ty.is_container() && !is_presence_op(op) {
        ctx.push(
            format!("{path}.op"),
            format!(
                "\"{field}\" is a {} and only supports empty/notEmpty",
                t_ty.as_str()
            ),
        );
        return;
    }
    if is_ordered_op(op) && !t_ty.spec().ordered {
        ctx.push(
            format!("{path}.op"),
            format!("\"{op_str}\" cannot be used on type \"{}\"", t_ty.as_str()),
        );
        return;
    }
    if !is_presence_op(op) && (t_ty.is_media() || t_ty == FormNodeType::Location) {
        ctx.push(
            format!("{path}.op"),
            format!(
                "\"{field}\" is a {} and only supports empty/notEmpty",
                t_ty.as_str()
            ),
        );
        return;
    }

    let value = c.get("value");
    if is_presence_op(op) {
        if value.is_some() {
            ctx.push(
                format!("{path}.value"),
                format!("\"{op_str}\" does not take a value"),
            );
        }
    } else if let Some(value) = value {
        if is_list_op(op) {
            if value.as_array().is_none_or(Vec::is_empty) {
                ctx.push(
                    format!("{path}.value"),
                    format!("\"{op_str}\" requires a non-empty array"),
                );
            } else {
                check_condition_value_kind(ctx, &format!("{path}.value"), target, op, value);
            }
        } else {
            check_condition_value_kind(ctx, &format!("{path}.value"), target, op, value);
        }
    } else {
        ctx.push(
            format!("{path}.value"),
            format!("\"{op_str}\" requires a value"),
        );
    }

    edges.push(t_path);
}

fn validate_visible_if(ctx: &mut Ctx, owner: usize) -> Vec<String> {
    let mut edges = Vec::new();
    let Some((raw, owner_path)) = ctx
        .collected
        .get(owner)
        .map(|o| (o.raw.get("visibleIf"), o.path.clone()))
    else {
        return edges;
    };
    let Some(raw) = raw else {
        return edges;
    };
    let path = format!("{owner_path}.visibleIf");
    let Some(rule) = raw.as_object() else {
        ctx.push(path, "visibleIf must be an object");
        return edges;
    };
    for key in rule.keys() {
        if key != "all" && key != "any" {
            ctx.push(
                format!("{path}.{key}"),
                format!("unknown visibleIf property \"{key}\""),
            );
        }
    }
    let mut total = 0;
    let mut present = false;
    for bucket in ["all", "any"] {
        let Some(list) = rule.get(bucket) else {
            continue;
        };
        let Some(list) = list.as_array() else {
            ctx.push(
                format!("{path}.{bucket}"),
                format!("{bucket} must be an array"),
            );
            continue;
        };
        if list.is_empty() {
            ctx.push(
                format!("{path}.{bucket}"),
                format!("{bucket} must not be empty"),
            );
            continue;
        }
        present = true;
        total += list.len();
        for (index, condition) in list.iter().enumerate() {
            validate_condition(
                ctx,
                owner,
                &format!("{path}.{bucket}[{index}]"),
                condition,
                &mut edges,
            );
        }
    }
    if !present {
        ctx.push(
            path.clone(),
            "visibleIf requires at least one of \"all\" or \"any\"",
        );
    }
    if total > MAX_VISIBLE_IF_CONDITIONS {
        ctx.push(
            path,
            format!("visibleIf must not exceed {MAX_VISIBLE_IF_CONDITIONS} conditions"),
        );
    }
    edges
}

// ── Pass 3: siklus ───────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum Colour {
    Grey,
    Black,
}

struct CycleSearch<'e> {
    edges: &'e HashMap<String, Vec<String>>,
    key_by_path: HashMap<String, String>,
    colour: HashMap<String, Colour>,
    trail: Vec<String>,
    reported: Option<String>,
}

impl CycleSearch<'_> {
    fn label(&self, p: &str) -> String {
        self.key_by_path
            .get(p)
            .cloned()
            .unwrap_or_else(|| p.to_owned())
    }

    fn visit(&mut self, path: &str) {
        if self.reported.is_some() {
            return;
        }
        match self.colour.get(path) {
            Some(Colour::Black) => return,
            Some(Colour::Grey) => {
                let start = self.trail.iter().position(|p| p == path).unwrap_or(0);
                let mut lp: Vec<String> = self
                    .trail
                    .get(start..)
                    .unwrap_or(&[])
                    .iter()
                    .map(|p| self.label(p))
                    .collect();
                lp.push(self.label(path));
                self.reported = Some(format!("visibleIf forms a cycle: {}", lp.join(" → ")));
                return;
            }
            None => {}
        }
        self.colour.insert(path.to_owned(), Colour::Grey);
        self.trail.push(path.to_owned());
        let next: Vec<String> = self.edges.get(path).cloned().unwrap_or_default();
        for n in next {
            self.visit(&n);
            if self.reported.is_some() {
                return;
            }
        }
        self.trail.pop();
        self.colour.insert(path.to_owned(), Colour::Black);
    }
}

fn detect_cycles(ctx: &mut Ctx, edges: &HashMap<String, Vec<String>>) {
    let mut search = CycleSearch {
        edges,
        key_by_path: ctx
            .collected
            .iter()
            .map(|n| (n.path.clone(), n.key.clone()))
            .collect(),
        colour: HashMap::new(),
        trail: Vec::new(),
        reported: None,
    };
    let paths: Vec<String> = ctx.collected.iter().map(|n| n.path.clone()).collect();
    for p in paths {
        search.trail.clear();
        search.visit(&p);
        if let Some(message) = search.reported.take() {
            ctx.push("", message);
            return;
        }
    }
}

// ── Pass 4: diff terhadap versi terpublish terakhir ──────────────────────────

/// `collectDataTypes(schema)` (`form-schema.validator.ts:1182`) — jalur data → `type`
/// dari schema tersimpan (urutan sisip `Map` JS). Defensif terhadap bentuk apa pun.
pub fn collect_data_types(schema: &Value) -> Vec<(String, String)> {
    fn set(out: &mut Vec<(String, String)>, k: String, v: String) {
        if let Some(slot) = out.iter_mut().find(|(ek, _)| *ek == k) {
            slot.1 = v;
        } else {
            out.push((k, v));
        }
    }
    fn walk(fields: Option<&Value>, prefix: &str, out: &mut Vec<(String, String)>) {
        let Some(fields) = fields.and_then(Value::as_array) else {
            return;
        };
        for raw in fields {
            let Some(o) = raw.as_object() else {
                continue;
            };
            let (Some(key), Some(ty)) = (
                o.get("key").and_then(Value::as_str),
                o.get("type").and_then(Value::as_str),
            ) else {
                continue;
            };
            if ty == "section" {
                walk(o.get("fields"), prefix, out);
                continue;
            }
            if FormNodeType::parse(ty).is_some_and(FormNodeType::is_display) {
                continue;
            }
            let data_path = format!("{prefix}{key}");
            set(out, data_path.clone(), ty.to_owned());
            if ty == "group" {
                walk(o.get("fields"), &format!("{data_path}."), out);
            } else if ty == "repeater" {
                walk(o.get("fields"), &format!("{data_path}[]."), out);
            }
        }
    }
    let mut out = Vec::new();
    if let Some(o) = schema.as_object() {
        walk(o.get("fields"), "", &mut out);
    }
    out
}

fn diff_against_previous(ctx: &mut Ctx, previous_schema: &Value) {
    let previous = collect_data_types(previous_schema);
    if previous.is_empty() {
        return;
    }
    let mut found = Vec::new();
    for node in &ctx.collected {
        let Some(dp) = &node.data_path else {
            continue;
        };
        if let Some((_, prev_ty)) = previous.iter().find(|(k, _)| k == dp)
            && prev_ty != node.ty.as_str()
        {
            found.push(FormSchemaIssue::new(
                node.path.clone(),
                format!(
                    "key \"{dp}\" was \"{prev_ty}\" in the last published version and cannot change to \"{}\"; use a new key instead",
                    node.ty.as_str()
                ),
            ));
        }
    }
    ctx.issues.extend(found);
}

// ── Entry point ──────────────────────────────────────────────────────────────

/// `validateFormSchema(input, options)` (`form-schema.validator.ts:1247`). `input = None`
/// = `undefined`. Daftar kosong = lolos.
pub fn validate_form_schema(
    input: Option<&Value>,
    opts: &ValidateFormSchemaOptions<'_>,
) -> Vec<FormSchemaIssue> {
    let Some(doc) = input.and_then(Value::as_object) else {
        return vec![FormSchemaIssue::new("", "schema must be an object")];
    };
    let mut ctx = Ctx {
        issues: Vec::new(),
        collected: Vec::new(),
        scopes: vec![HashMap::new()],
        node_count: 0,
        time_zone: opts.time_zone.unwrap_or("UTC").to_owned(),
    };
    for key in doc.keys() {
        let known =
            key == "schemaVersion" || key == "fields" || LAYOUT_SECTIONS.contains(&key.as_str());
        if !known {
            ctx.push(key.clone(), format!("unknown property \"{key}\""));
        }
    }
    if doc.get("schemaVersion").and_then(Value::as_f64) != Some(FORM_SCHEMA_VERSION as f64) {
        ctx.push(
            "schemaVersion",
            format!("schemaVersion must be {FORM_SCHEMA_VERSION}"),
        );
    }

    walk_fields(
        &mut ctx,
        doc.get("fields"),
        Container {
            context: FormContainerContext::Root,
            scope: 0,
            data_prefix: "",
            container_path: None,
            path: "",
        },
    );

    if ctx.issues.is_empty() {
        let mut edges: HashMap<String, Vec<String>> = HashMap::new();
        for i in 0..ctx.collected.len() {
            let mut deps = validate_visible_if(&mut ctx, i);
            if let Some(n) = ctx.collected.get(i) {
                if let Some(cp) = &n.container_path {
                    deps.push(cp.clone());
                }
                edges.insert(n.path.clone(), deps);
            }
        }
        if ctx.issues.is_empty() {
            detect_cycles(&mut ctx, &edges);
        }
    }

    if ctx.issues.is_empty()
        && let Some(previous) = opts.previous_schema
    {
        diff_against_previous(&mut ctx, previous);
    }

    ctx.issues.extend(validate_layout_sections(doc));
    ctx.issues
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn doc(fields: Value) -> Value {
        json!({ "schemaVersion": 1, "fields": fields })
    }

    fn issues_of(v: &Value) -> Vec<FormSchemaIssue> {
        validate_form_schema(Some(v), &ValidateFormSchemaOptions::default())
    }

    fn messages_prev(fields: Value, previous: Option<&Value>) -> Vec<String> {
        let d = doc(fields);
        let opts = ValidateFormSchemaOptions {
            previous_schema: previous,
            time_zone: None,
        };
        validate_form_schema(Some(&d), &opts)
            .into_iter()
            .map(|i| i.message)
            .collect()
    }

    fn messages(fields: Value) -> Vec<String> {
        messages_prev(fields, None)
    }

    fn text(key: &str) -> Value {
        json!({ "key": key, "type": "text", "label": "Teks" })
    }

    fn text_with(key: &str, extra: Value) -> Value {
        let mut t = text(key);
        if let (Some(o), Some(e)) = (t.as_object_mut(), extra.as_object()) {
            for (k, v) in e {
                o.insert(k.clone(), v.clone());
            }
        }
        t
    }

    fn issue(path: &str, message: &str) -> FormSchemaIssue {
        FormSchemaIssue::new(path, message)
    }

    #[track_caller]
    fn contains(ms: &[String], m: &str) {
        assert!(ms.iter().any(|x| x == m), "missing {m:?} in {ms:?}");
    }

    fn empty(ms: Vec<String>) {
        assert!(ms.is_empty(), "expected no issues, got {ms:?}");
    }

    fn full_example() -> Value {
        json!({
            "schemaVersion": 1,
            "fields": [
                {"key": "tgl", "type": "date", "label": "Tanggal kunjungan", "required": true,
                 "layout": {"colSpan": 6}, "validation": {"minDate": "today-7d", "maxDate": "today"}},
                {"key": "items", "type": "select", "label": "Produk dibahas", "required": true,
                 "layout": {"colSpan": 6},
                 "options": {"source": "static", "items": [
                     {"value": "ac_split", "label": "AC Split"}, {"value": "kulkas", "label": "Kulkas"}]}},
                {"key": "alasan_gagal", "type": "textarea", "label": "Alasan tidak closing", "required": true,
                 "validation": {"maxLength": 500},
                 "visibleIf": {"all": [{"field": "items", "op": "eq", "value": "ac_split"}]}},
                {"key": "unit_list", "type": "repeater", "label": "Daftar unit", "config": {"renderAs": "table"},
                 "validation": {"minRows": 1, "maxRows": 50},
                 "fields": [
                     {"key": "unit", "type": "text", "label": "Unit", "required": true},
                     {"key": "qty", "type": "number", "label": "Qty", "validation": {"min": 1, "max": 999, "decimals": 0}}
                 ]},
                {"key": "sec_approval", "type": "section", "label": "Persetujuan", "fields": [
                    {"key": "diserahkan", "type": "group", "label": "Diserahkan oleh", "layout": {"colSpan": 6}, "fields": [
                        {"key": "ttd", "type": "signature", "label": "Tanda tangan", "required": true},
                        {"key": "nama", "type": "text", "label": "Nama terang", "required": true}
                    ]},
                    {"key": "diterima", "type": "group", "label": "Diterima oleh", "layout": {"colSpan": 6}, "fields": [
                        {"key": "ttd", "type": "signature", "label": "Tanda tangan", "required": true},
                        {"key": "nama", "type": "text", "label": "Nama terang", "required": true}
                    ]}
                ]}
            ]
        })
    }

    // ── dokumen ──
    #[test]
    fn full_example_passes() {
        assert_eq!(issues_of(&full_example()), vec![]);
    }

    #[test]
    fn rejects_non_object() {
        assert_eq!(
            issues_of(&json!("[]")),
            vec![issue("", "schema must be an object")]
        );
        assert_eq!(issues_of(&Value::Null).len(), 1);
        assert_eq!(issues_of(&json!([])).len(), 1);
        assert_eq!(
            validate_form_schema(None, &ValidateFormSchemaOptions::default()).len(),
            1
        );
    }

    #[test]
    fn rejects_schema_version() {
        assert_eq!(
            issues_of(&json!({"schemaVersion": 2, "fields": [text("a")]})),
            vec![issue("schemaVersion", "schemaVersion must be 1")]
        );
    }

    #[test]
    fn rejects_unknown_top_level() {
        assert_eq!(
            issues_of(&json!({"schemaVersion": 1, "fields": [text("a")], "title": "x"})),
            vec![issue("title", "unknown property \"title\"")]
        );
    }

    #[test]
    fn rejects_bad_fields() {
        assert_eq!(
            issues_of(&json!({"schemaVersion": 1, "fields": {}})),
            vec![issue(".fields", "fields must be an array")]
        );
        assert_eq!(
            issues_of(&json!({"schemaVersion": 1, "fields": []})),
            vec![issue(".fields", "fields must not be empty")]
        );
    }

    // ── key ──
    #[test]
    fn rejects_bad_keys() {
        let long = format!("a{}", "b".repeat(40));
        for key in ["Nama", "1nama", "_nama", "nama-toko", "", long.as_str()] {
            contains(
                &messages(json!([text(key)])),
                "key must match ^[a-z][a-z0-9_]{0,39}$",
            );
        }
    }

    #[test]
    fn accepts_40_char_key() {
        empty(messages(json!([text(&format!("a{}", "b".repeat(39)))])));
    }

    #[test]
    fn rejects_duplicate_keys() {
        contains(
            &messages(json!([text("nama"), text("nama")])),
            "duplicate key \"nama\" in this scope",
        );
    }

    #[test]
    fn section_shares_namespace() {
        contains(
            &messages(json!([
                {"key": "sec", "type": "section", "label": "S", "fields": [text("a")]},
                text("sec")
            ])),
            "duplicate key \"sec\" in this scope",
        );
    }

    #[test]
    fn same_key_in_two_groups() {
        empty(messages(json!([
            {"key": "g1", "type": "group", "label": "G1", "fields": [text("ttd")]},
            {"key": "g2", "type": "group", "label": "G2", "fields": [text("ttd")]}
        ])));
    }

    #[test]
    fn same_key_in_repeater_and_root() {
        empty(messages(json!([
            text("qty"),
            {"key": "rows", "type": "repeater", "label": "R",
             "validation": {"minRows": 0, "maxRows": 5}, "fields": [text("qty")]}
        ])));
    }

    // ── nesting whitelist ──
    fn container(key: &str, ty: &str, children: Value) -> Value {
        json!({"key": key, "type": ty, "label": "C", "fields": children})
    }

    fn repeater(key: &str, children: Value) -> Value {
        let mut c = container(key, "repeater", children);
        c["validation"] = json!({"minRows": 0, "maxRows": 5});
        c
    }

    #[test]
    fn nesting_rules() {
        contains(
            &messages(json!([container(
                "outer",
                "section",
                json!([container("inner", "section", json!([text("a")]))])
            )])),
            "type \"section\" is not allowed inside section",
        );
        contains(
            &messages(json!([repeater(
                "outer",
                json!([repeater("inner", json!([text("a")]))])
            )])),
            "type \"repeater\" is not allowed inside repeater",
        );
        contains(
            &messages(json!([container(
                "g",
                "group",
                json!([container("h", "group", json!([text("a")]))])
            )])),
            "type \"group\" is not allowed inside group",
        );
        contains(
            &messages(json!([repeater(
                "r",
                json!([container("h", "group", json!([text("a")]))])
            )])),
            "type \"group\" is not allowed inside repeater",
        );
        contains(
            &messages(json!([container(
                "g",
                "group",
                json!([container("s", "section", json!([text("a")]))])
            )])),
            "type \"section\" is not allowed inside group",
        );
        empty(messages(json!([container(
            "s",
            "section",
            json!([
                container("g", "group", json!([text("a")])),
                repeater("r", json!([text("b")]))
            ])
        )])));
        contains(
            &messages(json!([container("g", "group", json!([]))])),
            "fields must not be empty",
        );
    }

    // ── properti umum ──
    #[test]
    fn label_rules() {
        contains(
            &messages(json!([{"key": "a", "type": "text"}])),
            "label is required",
        );
        empty(messages(json!([{"key": "d", "type": "divider"}])));
        contains(
            &messages(json!([{"key": "d", "type": "divider", "label": "x"}])),
            "divider does not take a label",
        );
    }

    #[test]
    fn unknown_node_properties() {
        contains(
            &messages(json!([text_with("a", json!({"position": "left"}))])),
            "unknown property \"position\"",
        );
        contains(
            &messages(json!([text_with("a", json!({"fields": []}))])),
            "unknown property \"fields\"",
        );
    }

    #[test]
    fn required_rules() {
        contains(
            &messages(
                json!([{"key": "s", "type": "section", "label": "S", "required": true, "fields": [text("a")]}]),
            ),
            "required is not allowed on type \"section\"",
        );
        contains(
            &messages(
                json!([{"key": "g", "type": "group", "label": "G", "required": true, "fields": [text("a")]}]),
            ),
            "required is not allowed on type \"group\"",
        );
        contains(
            &messages(json!([{"key": "h", "type": "heading", "label": "H", "required": true}])),
            "required is not allowed on type \"heading\"",
        );
        empty(messages(
            json!([{"key": "r", "type": "repeater", "label": "R", "required": true,
            "validation": {"minRows": 0, "maxRows": 5}, "fields": [text("a")]}]),
        ));
    }

    #[test]
    fn col_span_rules() {
        contains(
            &messages(json!([text_with("a", json!({"layout": {"colSpan": 13}}))])),
            "colSpan must be an integer between 1 and 12",
        );
        assert_eq!(
            messages(json!([text_with("a", json!({"layout": {"colSpan": 0}}))])).len(),
            1
        );
        empty(messages(json!([text_with(
            "a",
            json!({"layout": {"colSpan": 12}})
        )])));
        contains(
            &messages(json!([text_with("a", json!({"layout": {"columns": 3}}))])),
            "unknown layout property \"columns\"",
        );
    }

    #[test]
    fn placeholder_rules() {
        empty(messages(json!([text_with(
            "a",
            json!({"placeholder": "isi"})
        )])));
        contains(
            &messages(json!([{"key": "b", "type": "boolean", "label": "B", "placeholder": "isi"}])),
            "placeholder is only allowed on text and number fields",
        );
    }

    // ── validation ──
    #[test]
    fn validation_keys() {
        contains(
            &messages(json!([text_with("a", json!({"validation": {"min": 1}}))])),
            "validation.min is not allowed on type \"text\"",
        );
        contains(
            &messages(
                json!([{"key": "r", "type": "rating", "label": "R", "validation": {"max": 10}}]),
            ),
            "rating uses config.max; validation.min/max are not allowed",
        );
    }

    #[test]
    fn repeater_requires_rows() {
        contains(
            &messages(
                json!([{"key": "r", "type": "repeater", "label": "R", "fields": [text("a")]}]),
            ),
            "repeater requires validation.minRows and validation.maxRows",
        );
        contains(
            &messages(json!([{"key": "r", "type": "repeater", "label": "R",
                "validation": {"minRows": 1}, "fields": [text("a")]}])),
            "repeater requires maxRows",
        );
        contains(
            &messages(json!([{"key": "r", "type": "repeater", "label": "R",
                "validation": {"minRows": 0, "maxRows": 201}, "fields": [text("a")]}])),
            "maxRows must be between 1 and 200",
        );
    }

    #[test]
    fn lower_above_upper() {
        contains(
            &messages(json!([text_with(
                "a",
                json!({"validation": {"minLength": 10, "maxLength": 5}})
            )])),
            "minLength must not be greater than maxLength",
        );
        contains(
            &messages(
                json!([{"key": "n", "type": "number", "label": "N", "validation": {"min": 10, "max": 5}}]),
            ),
            "min must not be greater than max",
        );
    }

    #[test]
    fn named_formats_only() {
        empty(messages(json!([text_with(
            "a",
            json!({"validation": {"format": "nik"}})
        )])));
        contains(
            &messages(json!([text_with(
                "a",
                json!({"validation": {"format": "^[0-9]+$"}})
            )])),
            "format must be one of: email, phone_id, url, numeric, alphanumeric, nik, npwp",
        );
    }

    #[test]
    fn file_validation() {
        contains(
            &messages(json!([{"key": "f", "type": "file", "label": "F",
                "validation": {"maxSizeBytes": 2 * 1024 * 1024 + 1}}])),
            "maxSizeBytes must be between 1 and 2097152",
        );
        empty(messages(json!([{"key": "f", "type": "file", "label": "F",
            "validation": {"accept": ["application/pdf", "image/*"]}}])));
        contains(
            &messages(
                json!([{"key": "f", "type": "file", "label": "F", "validation": {"accept": ["pdf"]}}]),
            ),
            "\"pdf\" is not a valid media type",
        );
    }

    fn date_field(validation: Value) -> Value {
        json!([{"key": "d", "type": "date", "label": "D", "validation": validation}])
    }

    #[test]
    fn date_bounds() {
        empty(messages(date_field(json!({"minDate": "today-7d"}))));
        empty(messages(date_field(json!({"maxDate": "today"}))));
        empty(messages(date_field(json!({"maxDate": "today+30d"}))));
        empty(messages(date_field(json!({"minDate": "2026-01-31"}))));
        assert_eq!(
            messages(date_field(json!({"minDate": "yesterday"}))).len(),
            1
        );
        assert_eq!(messages(date_field(json!({"minDate": "today-7"}))).len(), 1);
        assert_eq!(
            messages(date_field(json!({"minDate": "2026-02-30"}))).len(),
            1
        );
        contains(
            &messages(
                json!([{"key": "t", "type": "time", "label": "T", "validation": {"minDate": "today"}}]),
            ),
            "minDate must be an HH:mm time",
        );
        empty(messages(
            json!([{"key": "t", "type": "time", "label": "T", "validation": {"minDate": "09:30"}}]),
        ));
        empty(messages(
            json!([{"key": "dt", "type": "datetime", "label": "DT",
            "validation": {"minDate": "2026-08-11T09:00:00Z"}}]),
        ));
    }

    // ── config ──
    #[test]
    fn config_rules() {
        contains(
            &messages(json!([text_with("a", json!({"config": {"rows": 3}}))])),
            "config.rows is not allowed on type \"text\"",
        );
        contains(
            &messages(
                json!([{"key": "r", "type": "repeater", "label": "R", "config": {"renderAs": "grid"},
                "validation": {"minRows": 0, "maxRows": 5}, "fields": [text("a")]}]),
            ),
            "renderAs must be one of: table, card",
        );
        contains(
            &messages(json!([{"key": "g", "type": "group", "label": "G",
                "config": {"direction": "diagonal"}, "fields": [text("a")]}])),
            "direction must be one of: column, row",
        );
        contains(
            &messages(
                json!([{"key": "r", "type": "repeater", "label": "R", "config": {"direction": "row"},
                "validation": {"minRows": 0, "maxRows": 5}, "fields": [text("a")]}]),
            ),
            "config.direction is not allowed on type \"repeater\"",
        );
        empty(messages(json!([{"key": "g", "type": "group", "label": "G",
            "config": {"direction": "row", "justify": "between"}, "fields": [text("a"), text("b")]}])));
        contains(
            &messages(json!([{"key": "g", "type": "group", "label": "G",
                "config": {"direction": "row", "justify": "spread"}, "fields": [text("a")]}])),
            "justify must be one of: stretch, start, center, end, between, around",
        );
        contains(
            &messages(json!([{"key": "g", "type": "group", "label": "G",
                "config": {"justify": "center"}, "fields": [text("a")]}])),
            "justify only applies when direction is \"row\"",
        );
        empty(messages(json!([{"key": "g", "type": "group", "label": "G",
            "config": {"direction": "row"}, "fields": [text("a"), text("b")]}])));
        contains(
            &messages(
                json!([{"key": "c", "type": "currency", "label": "C", "config": {"currencyCode": "rupiah"}}]),
            ),
            "currencyCode must be a 3-letter uppercase code",
        );
        contains(
            &messages(
                json!([{"key": "s", "type": "signature", "label": "S", "config": {"penColor": "black"}}]),
            ),
            "penColor must be a #rrggbb colour",
        );
        contains(
            &messages(json!([{"key": "r", "type": "rating", "label": "R", "config": {"max": 99}}])),
            "max must be an integer between 2 and 10",
        );
    }

    // ── options ──
    fn select(options: Value) -> Value {
        json!([{"key": "s", "type": "select", "label": "S", "options": options}])
    }

    fn static_items(items: Value) -> Value {
        json!({"source": "static", "items": items})
    }

    #[test]
    fn options_required_and_forbidden() {
        contains(
            &messages(json!([{"key": "s", "type": "select", "label": "S"}])),
            "type \"select\" requires options",
        );
        contains(
            &messages(json!([text_with(
                "a",
                json!({"options": static_items(json!([{"value": "x", "label": "X"}]))})
            )])),
            "type \"text\" does not accept options",
        );
    }

    #[test]
    fn options_source_and_items() {
        contains(
            &messages(select(
                json!({"source": "products", "items": [{"value": "x", "label": "X"}]}),
            )),
            "source must be one of: static, product, order",
        );
        contains(
            &messages(select(static_items(json!([])))),
            "items must not be empty",
        );
        let many: Vec<Value> = (0..201)
            .map(|i| json!({"value": format!("v{i}"), "label": format!("L{i}")}))
            .collect();
        contains(
            &messages(select(static_items(Value::Array(many)))),
            "items must not exceed 200 entries",
        );
        contains(
            &messages(select(static_items(
                json!([{"value": "AC Split", "label": "X"}]),
            ))),
            "value must match ^[a-z][a-z0-9_]{0,39}$",
        );
        contains(
            &messages(select(static_items(
                json!([{"value": "a", "label": "A"}, {"value": "a", "label": "A lagi"}]),
            ))),
            "duplicate option value \"a\"",
        );
        contains(
            &messages(select(static_items(json!([{"value": "a"}])))),
            "label is required",
        );
        empty(messages(select(json!({"source": "product"}))));
        empty(messages(select(json!({"source": "order"}))));
        contains(
            &messages(select(
                json!({"source": "product", "items": [{"value": "x", "label": "X"}]}),
            )),
            "options.items must be absent when source is \"product\" or \"order\"",
        );
    }

    #[test]
    fn dynamic_source_visible_if() {
        let ms = messages(json!([
            {"key": "s", "type": "select", "label": "S", "options": {"source": "product"}},
            text_with("a", json!({"visibleIf": {"all": [{"field": "s", "op": "eq", "value": "apa-saja"}]}}))
        ]));
        assert!(
            !ms.iter()
                .any(|m| m == "\"apa-saja\" is not an option of \"s\"")
        );
        contains(
            &messages(json!([
                {"key": "s", "type": "select", "label": "S", "options": {"source": "product"}},
                text_with("a", json!({"visibleIf": {"all": [{"field": "s", "op": "eq", "value": 123}]}}))
            ])),
            "value for \"s\" must be a string",
        );
    }

    // ── defaultValue ──
    #[test]
    fn default_value_accepted() {
        empty(messages(json!([text_with(
            "a",
            json!({"defaultValue": "halo"})
        )])));
        empty(messages(
            json!([{"key": "s", "type": "select", "label": "S", "defaultValue": "a",
            "options": {"source": "static", "items": [{"value": "a", "label": "A"}]}}]),
        ));
    }

    #[test]
    fn default_value_rejected_by_leaf() {
        contains(
            &messages(json!([{"key": "n", "type": "number", "label": "N", "defaultValue": "dua"}])),
            "value must be a finite number",
        );
        contains(
            &messages(
                json!([{"key": "s", "type": "select", "label": "S", "defaultValue": "z",
                "options": {"source": "static", "items": [{"value": "a", "label": "A"}]}}]),
            ),
            "\"z\" is not an allowed option",
        );
    }

    #[test]
    fn default_value_forbidden() {
        contains(
            &messages(
                json!([{"key": "g", "type": "group", "label": "G", "defaultValue": {}, "fields": [text("a")]}]),
            ),
            "defaultValue is not allowed on type \"group\"",
        );
        contains(
            &messages(
                json!([{"key": "s", "type": "select", "label": "S", "defaultValue": "apa-saja",
                "options": {"source": "product"}}]),
            ),
            "defaultValue is not allowed when options.source is \"product\" or \"order\"",
        );
        empty(messages(
            json!([{"key": "d", "type": "date", "label": "D", "defaultValue": "2020-01-01",
            "validation": {"minDate": "today"}}]),
        ));
    }

    // ── visibleIf ──
    fn gate() -> Value {
        json!({"key": "items", "type": "select", "label": "Items", "options": {"source": "static", "items": [
            {"value": "ac_split", "label": "AC"}, {"value": "kulkas", "label": "Kulkas"}]}})
    }

    fn vis(key: &str, rule: Value) -> Value {
        text_with(key, json!({ "visibleIf": rule }))
    }

    #[test]
    fn visible_if_basics() {
        empty(messages(json!([
            gate(),
            vis(
                "a",
                json!({
                    "all": [{"field": "items", "op": "eq", "value": "ac_split"}],
                    "any": [{"field": "items", "op": "ne", "value": "kulkas"}]
                })
            )
        ])));
        contains(
            &messages(json!([vis("a", json!({}))])),
            "visibleIf requires at least one of \"all\" or \"any\"",
        );
        contains(
            &messages(json!([vis(
                "a",
                json!({"all": [{"field": "hantu", "op": "notEmpty"}]})
            )])),
            "\"hantu\" is not a field in the same data scope",
        );
        contains(
            &messages(
                json!([text("luar"), {"key": "g", "type": "group", "label": "G", "fields": [
                vis("dalam", json!({"all": [{"field": "luar", "op": "notEmpty"}]}))]}]),
            ),
            "\"luar\" is not a field in the same data scope",
        );
        empty(messages(
            json!([gate(), {"key": "s", "type": "section", "label": "S", "fields": [
            vis("a", json!({"all": [{"field": "items", "op": "eq", "value": "kulkas"}]}))]}]),
        ));
    }

    #[test]
    fn visible_if_targets() {
        contains(
            &messages(
                json!([{"key": "s", "type": "section", "label": "S", "fields": [text("x")]},
                vis("a", json!({"all": [{"field": "s", "op": "notEmpty"}]}))]),
            ),
            "a section has no value and cannot be referenced",
        );
        contains(
            &messages(json!([{"key": "h", "type": "heading", "label": "H"},
                vis("a", json!({"all": [{"field": "h", "op": "notEmpty"}]}))])),
            "\"h\" is a display node and has no value",
        );
        let group = json!({"key": "g", "type": "group", "label": "G", "fields": [text("x")]});
        empty(messages(json!([
            group.clone(),
            vis("a", json!({"all": [{"field": "g", "op": "notEmpty"}]}))
        ])));
        contains(
            &messages(json!([
                group,
                vis(
                    "a",
                    json!({"all": [{"field": "g", "op": "eq", "value": "x"}]})
                )
            ])),
            "\"g\" is a group and only supports empty/notEmpty",
        );
        contains(
            &messages(json!([{"key": "ttd", "type": "signature", "label": "TTD"},
                vis("a", json!({"all": [{"field": "ttd", "op": "eq", "value": "x"}]}))])),
            "\"ttd\" is a signature and only supports empty/notEmpty",
        );
        empty(messages(
            json!([{"key": "n", "type": "number", "label": "N"},
            vis("a", json!({"all": [{"field": "n", "op": "gte", "value": 5}]}))]),
        ));
        contains(
            &messages(json!([
                text("t"),
                vis(
                    "a",
                    json!({"all": [{"field": "t", "op": "gt", "value": "x"}]})
                )
            ])),
            "\"gt\" cannot be used on type \"text\"",
        );
    }

    #[test]
    fn visible_if_values() {
        contains(
            &messages(json!([
                text("t"),
                vis(
                    "a",
                    json!({"all": [{"field": "t", "op": "notEmpty", "value": 1}]})
                )
            ])),
            "\"notEmpty\" does not take a value",
        );
        contains(
            &messages(json!([
                text("t"),
                vis("a", json!({"all": [{"field": "t", "op": "eq"}]}))
            ])),
            "\"eq\" requires a value",
        );
        contains(
            &messages(json!([
                gate(),
                vis(
                    "a",
                    json!({"all": [{"field": "items", "op": "in", "value": "ac_split"}]})
                )
            ])),
            "\"in\" requires a non-empty array",
        );
        contains(
            &messages(json!([
                gate(),
                vis(
                    "a",
                    json!({"all": [{"field": "items", "op": "in", "value": []}]})
                )
            ])),
            "\"in\" requires a non-empty array",
        );
        contains(
            &messages(json!([
                gate(),
                vis(
                    "a",
                    json!({"all": [{"field": "items", "op": "eq", "value": "ac_spilt"}]})
                )
            ])),
            "\"ac_spilt\" is not an option of \"items\"",
        );
        contains(
            &messages(json!([{"key": "n", "type": "number", "label": "N"},
                vis("a", json!({"all": [{"field": "n", "op": "eq", "value": "5"}]}))])),
            "value for \"n\" must be a number",
        );
        contains(
            &messages(json!([
                text("t"),
                vis(
                    "a",
                    json!({"all": [{"field": "t", "op": "matches", "value": "x"}]})
                )
            ])),
            "unknown operator \"matches\"",
        );
        let conditions: Vec<Value> = (0..=MAX_VISIBLE_IF_CONDITIONS)
            .map(|_| json!({"field": "t", "op": "notEmpty"}))
            .collect();
        contains(
            &messages(json!([text("t"), vis("a", json!({ "all": conditions }))])),
            "visibleIf must not exceed 20 conditions",
        );
    }

    // ── siklus ──
    fn has_cycle(ms: &[String]) -> bool {
        ms.iter().any(|m| m.starts_with("visibleIf forms a cycle"))
    }

    #[test]
    fn cycles() {
        let ms = messages(json!([
            vis("a", json!({"all": [{"field": "b", "op": "notEmpty"}]})),
            vis("b", json!({"all": [{"field": "a", "op": "notEmpty"}]}))
        ]));
        assert!(has_cycle(&ms));
        assert_eq!(ms, vec!["visibleIf forms a cycle: a → b → a".to_owned()]);
        assert!(has_cycle(&messages(json!([
            vis("a", json!({"all": [{"field": "c", "op": "notEmpty"}]})),
            vis("b", json!({"all": [{"field": "a", "op": "notEmpty"}]})),
            vis("c", json!({"all": [{"field": "b", "op": "notEmpty"}]}))
        ]))));
        contains(
            &messages(json!([vis(
                "a",
                json!({"all": [{"field": "a", "op": "notEmpty"}]})
            )])),
            "a field cannot depend on itself",
        );
        assert!(has_cycle(&messages(
            json!([{"key": "sec", "type": "section", "label": "S",
            "visibleIf": {"all": [{"field": "a", "op": "notEmpty"}]}, "fields": [text("a")]}])
        )));
        empty(messages(json!([
            text("a"),
            vis("b", json!({"all": [{"field": "a", "op": "notEmpty"}]})),
            vis("c", json!({"all": [{"field": "b", "op": "notEmpty"}]})),
            vis("d", json!({"all": [{"field": "c", "op": "notEmpty"}]}))
        ])));
    }

    // ── batas node ──
    #[test]
    fn node_cap() {
        let f201: Vec<Value> = (0..201).map(|i| text(&format!("f{i}"))).collect();
        contains(
            &messages(Value::Array(f201)),
            "schema must not exceed 200 nodes in total",
        );
        let f200: Vec<Value> = (0..200).map(|i| text(&format!("f{i}"))).collect();
        empty(messages(Value::Array(f200.clone())));
        contains(
            &messages(json!([{"key": "g", "type": "group", "label": "G", "fields": f200}])),
            "schema must not exceed 200 nodes in total",
        );
    }

    // ── diff ──
    fn previous() -> Value {
        doc(json!([
            text("nama"),
            {"key": "qty", "type": "number", "label": "Qty"},
            {"key": "g", "type": "group", "label": "G", "fields": [{"key": "ttd", "type": "signature", "label": "TTD"}]}
        ]))
    }

    #[test]
    fn diff_against_published() {
        let prev = previous();
        empty(messages(
            json!([{"key": "nama", "type": "number", "label": "Nama"}]),
        ));
        contains(
            &messages_prev(
                json!([{"key": "qty", "type": "text", "label": "Qty"}]),
                Some(&prev),
            ),
            "key \"qty\" was \"number\" in the last published version and cannot change to \"text\"; use a new key instead",
        );
        empty(messages_prev(
            json!([text("nama"), text("baru")]),
            Some(&prev),
        ));
        empty(messages_prev(
            json!([{"key": "h", "type": "group", "label": "H", "fields": [text("ttd")]}]),
            Some(&prev),
        ));
        contains(
            &messages_prev(
                json!([{"key": "g", "type": "group", "label": "G", "fields": [text("ttd")]}]),
                Some(&prev),
            ),
            "key \"g.ttd\" was \"signature\" in the last published version and cannot change to \"text\"; use a new key instead",
        );
        contains(
            &messages_prev(
                json!([{"key": "g", "type": "repeater", "label": "G", "validation": {"minRows": 0, "maxRows": 5},
                    "fields": [{"key": "ttd", "type": "signature", "label": "TTD"}]}]),
                Some(&prev),
            ),
            "key \"g\" was \"group\" in the last published version and cannot change to \"repeater\"; use a new key instead",
        );
        let with_section =
            doc(json!([{"key": "nama", "type": "section", "label": "S", "fields": [text("x")]}]));
        empty(messages_prev(json!([text("nama")]), Some(&with_section)));
    }

    // ── collectDataTypes ──
    #[test]
    fn collect_data_types_maps_paths() {
        let expected: Vec<(String, String)> = [
            ("tgl", "date"),
            ("items", "select"),
            ("alasan_gagal", "textarea"),
            ("unit_list", "repeater"),
            ("unit_list[].unit", "text"),
            ("unit_list[].qty", "number"),
            ("diserahkan", "group"),
            ("diserahkan.ttd", "signature"),
            ("diserahkan.nama", "text"),
            ("diterima", "group"),
            ("diterima.ttd", "signature"),
            ("diterima.nama", "text"),
        ]
        .iter()
        .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
        .collect();
        assert_eq!(collect_data_types(&full_example()), expected);
    }

    #[test]
    fn collect_data_types_tolerates_garbage() {
        assert!(collect_data_types(&Value::Null).is_empty());
        assert!(collect_data_types(&json!({"fields": "bukan array"})).is_empty());
        assert!(collect_data_types(&json!({"fields": [null, {"key": 1}]})).is_empty());
    }

    // ── tambahan: semantik JS ──
    #[test]
    fn js_helpers() {
        assert_eq!(js_string(Some(&json!(5.0))), "5");
        assert_eq!(js_string(Some(&json!([1, null, "a"]))), "1,,a");
        assert_eq!(js_string(Some(&json!({}))), "[object Object]");
        assert_eq!(js_string(None), "undefined");
        assert!(is_iso_mime("image/*"));
        assert!(is_iso_mime("Application/PDF"));
        assert!(!is_iso_mime("*/*"));
        assert!(is_aspect_ratio("3:1"));
        assert!(!is_aspect_ratio("3:"));
        contains(
            &messages(json!([{"key": "x", "type": 5}])),
            "unknown node type \"5\"",
        );
        contains(
            &messages(
                json!([{"key": "f", "type": "file", "label": "F", "validation": {"accept": [1]}}]),
            ),
            "\"1\" is not a valid media type",
        );
    }
}
