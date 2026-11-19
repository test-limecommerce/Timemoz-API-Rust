//! Port `form-layout.validator.ts`: validator `headers` / `footers` (kop & kaki PDF).
//!
//! Murni. Keberadaan `assetUuid` di database diperiksa pemanggil lewat
//! [`collect_layout_asset_references`].

use std::collections::HashMap;

use serde_json::{Map, Value};

use crate::consts::{
    GRID_COLUMNS, GROUP_JUSTIFY, LAYOUT_ALIGNMENTS, LAYOUT_BLOCK_TYPES, LAYOUT_DIVIDER_STYLES,
    LAYOUT_DIVIDER_THICKNESS_RANGE, LAYOUT_IMAGE_HEIGHT_RANGE, LAYOUT_IMAGE_SOURCES,
    LAYOUT_ROW_CHILD_TYPES, LAYOUT_SECTIONS, LAYOUT_SPACER_HEIGHT_RANGE, LAYOUT_TEXT_SIZES,
    LAYOUT_TEXT_VARIABLES, LAYOUT_VARIABLE_PATTERN, MAX_LAYOUT_BLOCKS_PER_SECTION,
    MAX_LAYOUT_TEXT_LENGTH, Range, is_form_key, is_uuid_pattern, lookup_layout_block_config,
};
use crate::types::FormSchemaIssue;
use crate::value_util::{is_integer, js_trim, text_length};

const BLOCK_PROPERTIES: &[&str] = &[
    "key",
    "type",
    "text",
    "source",
    "assetUuid",
    "layout",
    "config",
    "blocks",
];

/// `HEX_COLOR_PATTERN` `^#[0-9a-fA-F]{6}$`.
pub(crate) fn is_hex_color(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 7 && b.first() == Some(&b'#') && b.iter().skip(1).all(u8::is_ascii_hexdigit)
}

/// `oneOf(allowed, value)`.
fn one_of(allowed: &[&str], v: Option<&Value>) -> bool {
    matches!(v, Some(Value::String(s)) if allowed.contains(&s.as_str()))
}

/// `inRange(value, range)`.
fn in_range(v: Option<&Value>, r: Range) -> bool {
    matches!(v.and_then(Value::as_f64), Some(x) if x.is_finite() && x >= r.min && x <= r.max)
}

/// `layoutTextVariables(text)` (`form-layout.validator.ts:66`) — nama variabel `{{…}}`
/// sesuai urutan kemunculan.
pub fn layout_text_variables(text: &str) -> Vec<String> {
    LAYOUT_VARIABLE_PATTERN
        .captures_iter(text)
        .filter_map(|c| c.get(1).map(|m| m.as_str().to_owned()))
        .collect()
}

struct Issues(Vec<FormSchemaIssue>);

impl Issues {
    fn push(&mut self, path: impl Into<String>, message: impl Into<String>) {
        self.0.push(FormSchemaIssue::new(path, message));
    }
}

fn validate_block_config(out: &mut Issues, path: &str, ty: &str, raw: Option<&Value>) {
    let Some(raw) = raw else {
        return;
    };
    let Some(cfg) = raw.as_object() else {
        out.push(format!("{path}.config"), "config must be an object");
        return;
    };
    let allowed = lookup_layout_block_config(ty).unwrap_or(&[]);
    for key in cfg.keys() {
        if !allowed.contains(&key.as_str()) {
            out.push(
                format!("{path}.config.{key}"),
                format!("config.{key} is not allowed on block \"{ty}\""),
            );
        }
    }

    let size = cfg.get("size");
    if size.is_some() && !one_of(LAYOUT_TEXT_SIZES, size) {
        out.push(
            format!("{path}.config.size"),
            format!("size must be one of: {}", LAYOUT_TEXT_SIZES.join(", ")),
        );
    }
    for flag in ["bold", "italic"] {
        if let Some(v) = cfg.get(flag)
            && !v.is_boolean()
        {
            out.push(
                format!("{path}.config.{flag}"),
                format!("{flag} must be a boolean"),
            );
        }
    }
    let align = cfg.get("align");
    if align.is_some() && !one_of(LAYOUT_ALIGNMENTS, align) {
        out.push(
            format!("{path}.config.align"),
            format!("align must be one of: {}", LAYOUT_ALIGNMENTS.join(", ")),
        );
    }
    if let Some(color) = cfg.get("color")
        && !color.as_str().is_some_and(is_hex_color)
    {
        out.push(
            format!("{path}.config.color"),
            "color must be a #rrggbb colour",
        );
    }
    let height = cfg.get("height");
    if height.is_some() {
        let range = if ty == "spacer" {
            LAYOUT_SPACER_HEIGHT_RANGE
        } else {
            LAYOUT_IMAGE_HEIGHT_RANGE
        };
        if !in_range(height, range) {
            out.push(
                format!("{path}.config.height"),
                format!(
                    "height must be a number between {} and {}",
                    range.min, range.max
                ),
            );
        }
    }
    let thickness = cfg.get("thickness");
    if thickness.is_some() && !in_range(thickness, LAYOUT_DIVIDER_THICKNESS_RANGE) {
        out.push(
            format!("{path}.config.thickness"),
            format!(
                "thickness must be a number between {} and {}",
                LAYOUT_DIVIDER_THICKNESS_RANGE.min, LAYOUT_DIVIDER_THICKNESS_RANGE.max
            ),
        );
    }
    let style = cfg.get("style");
    if style.is_some() && !one_of(LAYOUT_DIVIDER_STYLES, style) {
        out.push(
            format!("{path}.config.style"),
            format!("style must be one of: {}", LAYOUT_DIVIDER_STYLES.join(", ")),
        );
    }
    let justify = cfg.get("justify");
    if justify.is_some() && !one_of(GROUP_JUSTIFY, justify) {
        out.push(
            format!("{path}.config.justify"),
            format!("justify must be one of: {}", GROUP_JUSTIFY.join(", ")),
        );
    }
}

fn validate_block(
    out: &mut Issues,
    path: &str,
    raw: &Value,
    seen_keys: &mut HashMap<String, String>,
    inside_row: bool,
) {
    let Some(b) = raw.as_object() else {
        out.push(path, "block must be an object");
        return;
    };
    for key in b.keys() {
        if !BLOCK_PROPERTIES.contains(&key.as_str()) {
            out.push(
                format!("{path}.{key}"),
                format!("unknown property \"{key}\""),
            );
        }
    }

    match b.get("key").and_then(Value::as_str) {
        Some(key) if is_form_key(key) => {
            if let Some(first) = seen_keys.get(key) {
                out.push(
                    format!("{path}.key"),
                    format!("duplicate block key \"{key}\" (first used at {first})"),
                );
            } else {
                seen_keys.insert(key.to_owned(), path.to_owned());
            }
        }
        _ => out.push(
            format!("{path}.key"),
            "key must match ^[a-z][a-z0-9_]{0,39}$",
        ),
    }

    let ty = match b.get("type") {
        Some(Value::String(t)) if LAYOUT_BLOCK_TYPES.contains(&t.as_str()) => t.as_str(),
        _ => {
            out.push(
                format!("{path}.type"),
                format!("type must be one of: {}", LAYOUT_BLOCK_TYPES.join(", ")),
            );
            return;
        }
    };
    if inside_row && !LAYOUT_ROW_CHILD_TYPES.contains(&ty) {
        out.push(format!("{path}.type"), "a row cannot contain another row");
        return;
    }

    let children = b.get("blocks");
    if ty == "row" {
        match children {
            Some(Value::Array(items)) if !items.is_empty() => {
                for (i, child) in items.iter().enumerate() {
                    validate_block(out, &format!("{path}.blocks[{i}]"), child, seen_keys, true);
                }
            }
            _ => out.push(format!("{path}.blocks"), "row requires at least one block"),
        }
    } else if children.is_some() {
        out.push(
            format!("{path}.blocks"),
            format!("block \"{ty}\" does not take blocks"),
        );
    }

    let text = b.get("text");
    if ty == "text" {
        match text.and_then(Value::as_str) {
            Some(t) if !js_trim(t).is_empty() => {
                if text_length(t) > MAX_LAYOUT_TEXT_LENGTH {
                    out.push(
                        format!("{path}.text"),
                        format!("text must be at most {MAX_LAYOUT_TEXT_LENGTH} characters"),
                    );
                } else {
                    for name in layout_text_variables(t) {
                        if !LAYOUT_TEXT_VARIABLES.contains(&name.as_str()) {
                            out.push(
                                format!("{path}.text"),
                                format!("unknown variable \"{{{{{name}}}}}\""),
                            );
                        }
                    }
                }
            }
            _ => out.push(format!("{path}.text"), "text is required"),
        }
    } else if text.is_some() {
        out.push(
            format!("{path}.text"),
            format!("block \"{ty}\" does not take text"),
        );
    }

    let source = b.get("source");
    let asset_uuid = b.get("assetUuid");
    if ty == "image" {
        if !one_of(LAYOUT_IMAGE_SOURCES, source) {
            out.push(
                format!("{path}.source"),
                format!("source must be one of: {}", LAYOUT_IMAGE_SOURCES.join(", ")),
            );
        }
        if !asset_uuid
            .and_then(Value::as_str)
            .is_some_and(is_uuid_pattern)
        {
            out.push(
                format!("{path}.assetUuid"),
                "assetUuid must be the uuid of an uploaded image",
            );
        }
    } else {
        if source.is_some() {
            out.push(
                format!("{path}.source"),
                format!("block \"{ty}\" does not take a source"),
            );
        }
        if asset_uuid.is_some() {
            out.push(
                format!("{path}.assetUuid"),
                format!("block \"{ty}\" does not take an assetUuid"),
            );
        }
    }

    if let Some(layout) = b.get("layout") {
        match layout.as_object() {
            None => out.push(format!("{path}.layout"), "layout must be an object"),
            Some(l) => {
                for k in l.keys() {
                    if k != "colSpan" {
                        out.push(
                            format!("{path}.layout.{k}"),
                            format!("unknown layout property \"{k}\""),
                        );
                    }
                }
                if let Some(cs) = l.get("colSpan") {
                    let ok = cs
                        .as_f64()
                        .is_some_and(|x| is_integer(x) && x >= 1.0 && x <= GRID_COLUMNS as f64);
                    if !ok {
                        out.push(
                            format!("{path}.layout.colSpan"),
                            format!("colSpan must be an integer between 1 and {GRID_COLUMNS}"),
                        );
                    }
                }
            }
        }
    }

    validate_block_config(out, path, ty, b.get("config"));
}

/// Jumlah blok termasuk anak `row`.
fn count_blocks(blocks: &[Value]) -> usize {
    blocks
        .iter()
        .map(|b| {
            1 + b
                .get("blocks")
                .and_then(Value::as_array)
                .map_or(0, Vec::len)
        })
        .sum()
}

/// `validateLayoutSections(document)` (`form-layout.validator.ts:301`) — periksa `headers`
/// dan `footers`; `key` unik lintas kedua bagian.
pub fn validate_layout_sections(doc: &Map<String, Value>) -> Vec<FormSchemaIssue> {
    let mut out = Issues(Vec::new());
    let mut seen_keys = HashMap::new();
    for section in LAYOUT_SECTIONS {
        let Some(blocks) = doc.get(*section) else {
            continue;
        };
        let Some(blocks) = blocks.as_array() else {
            out.push(*section, format!("{section} must be an array"));
            continue;
        };
        if count_blocks(blocks) > MAX_LAYOUT_BLOCKS_PER_SECTION {
            out.push(
                *section,
                format!("{section} must not exceed {MAX_LAYOUT_BLOCKS_PER_SECTION} blocks"),
            );
            continue;
        }
        for (i, block) in blocks.iter().enumerate() {
            validate_block(
                &mut out,
                &format!("{section}[{i}]"),
                block,
                &mut seen_keys,
                false,
            );
        }
    }
    out.0
}

/// `LayoutAssetReference` (`form-layout.validator.ts:337`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutAssetReference {
    pub path: String,
    pub asset_uuid: String,
}

/// `collectLayoutAssetReferences(schema)` (`form-layout.validator.ts:346`) — defensif
/// terhadap bentuk apa pun.
pub fn collect_layout_asset_references(schema: &Value) -> Vec<LayoutAssetReference> {
    let mut references = Vec::new();
    let Some(doc) = schema.as_object() else {
        return references;
    };
    fn visit(block: &Value, path: String, out: &mut Vec<LayoutAssetReference>) {
        let Some(b) = block.as_object() else {
            return;
        };
        let ty = b.get("type").and_then(Value::as_str);
        if ty == Some("row")
            && let Some(children) = b.get("blocks").and_then(Value::as_array)
        {
            for (i, child) in children.iter().enumerate() {
                visit(child, format!("{path}.blocks[{i}]"), out);
            }
            return;
        }
        if ty != Some("image") || b.get("source").and_then(Value::as_str) != Some("upload") {
            return;
        }
        let Some(asset) = b.get("assetUuid").and_then(Value::as_str) else {
            return;
        };
        if !is_uuid_pattern(asset) {
            return;
        }
        out.push(LayoutAssetReference {
            path: format!("{path}.assetUuid"),
            asset_uuid: asset.to_lowercase(),
        });
    }
    for section in LAYOUT_SECTIONS {
        let Some(blocks) = doc.get(*section).and_then(Value::as_array) else {
            continue;
        };
        for (i, block) in blocks.iter().enumerate() {
            visit(block, format!("{section}[{i}]"), &mut references);
        }
    }
    references
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema_validator::{ValidateFormSchemaOptions, validate_form_schema};
    use serde_json::json;

    const ASSET: &str = "3c9d8e7f-6a5b-4c4d-9e3f-2a1b0c9d8e7f";
    const LOGO: &str = "9a8b7c6d-5e4f-4a3b-8c2d-1e0f9a8b7c6d";

    fn issues(layout: Value) -> Vec<FormSchemaIssue> {
        let mut doc = json!({
            "schemaVersion": 1,
            "fields": [{"key": "catatan", "type": "text", "label": "Catatan"}]
        });
        if let (Some(d), Some(l)) = (doc.as_object_mut(), layout.as_object()) {
            for (k, v) in l {
                d.insert(k.clone(), v.clone());
            }
        }
        validate_form_schema(Some(&doc), &ValidateFormSchemaOptions::default())
    }

    fn messages(layout: Value) -> Vec<String> {
        issues(layout).into_iter().map(|i| i.message).collect()
    }

    fn issue(path: &str, message: &str) -> FormSchemaIssue {
        FormSchemaIssue::new(path, message)
    }

    fn has(ms: &[String], m: &str) -> bool {
        ms.iter().any(|x| x == m)
    }

    fn full_layout() -> Value {
        json!({
            "headers": [
                {"key": "logo", "type": "image", "source": "upload", "assetUuid": LOGO,
                 "layout": {"colSpan": 3}, "config": {"height": 36, "align": "left"}},
                {"key": "judul", "type": "text",
                 "text": "LAPORAN KUNJUNGAN — {{storeName}} ({{ storeCode }})",
                 "layout": {"colSpan": 9},
                 "config": {"size": "lg", "bold": true, "italic": false, "align": "right", "color": "#0F172A"}},
                {"key": "garis", "type": "divider", "config": {"thickness": 1.5, "color": "#E2E8F0", "style": "dashed"}}
            ],
            "footers": [
                {"key": "jeda", "type": "spacer", "config": {"height": 4}},
                {"key": "stempel", "type": "image", "source": "upload", "assetUuid": ASSET, "layout": {"colSpan": 4}},
                {"key": "catatan", "type": "text", "text": "Halaman {{page}} dari {{pageCount}}",
                 "layout": {"colSpan": 8}, "config": {"size": "xs"}}
            ]
        })
    }

    fn row(blocks: Value, config: Option<Value>) -> Value {
        let mut r = json!({"key": "baris", "type": "row"});
        if let Some(c) = config {
            r["config"] = c;
        }
        r["blocks"] = blocks;
        r
    }

    #[test]
    fn accepts_full_layout() {
        assert_eq!(issues(full_layout()), vec![]);
    }

    #[test]
    fn both_optional_and_empty_ok() {
        assert_eq!(issues(json!({})), vec![]);
        assert_eq!(issues(json!({"headers": [], "footers": []})), vec![]);
    }

    #[test]
    fn rejects_non_array_sections() {
        assert_eq!(
            issues(json!({"headers": {}})),
            vec![issue("headers", "headers must be an array")]
        );
        assert!(has(
            &messages(json!({"footers": "x"})),
            "footers must be an array"
        ));
    }

    #[test]
    fn caps_blocks_per_section() {
        let blocks: Vec<Value> = (0..=MAX_LAYOUT_BLOCKS_PER_SECTION)
            .map(|i| json!({"key": format!("s{i}"), "type": "spacer"}))
            .collect();
        assert!(has(
            &messages(json!({"headers": blocks})),
            "headers must not exceed 20 blocks"
        ));
    }

    #[test]
    fn keys_unique_across_sections() {
        assert_eq!(
            issues(json!({
                "headers": [{"key": "garis", "type": "divider"}],
                "footers": [{"key": "garis", "type": "divider"}]
            })),
            vec![issue(
                "footers[0].key",
                "duplicate block key \"garis\" (first used at headers[0])"
            )]
        );
        assert!(has(
            &messages(json!({"headers": [{"key": "Garis", "type": "divider"}]})),
            "key must match ^[a-z][a-z0-9_]{0,39}$"
        ));
    }

    #[test]
    fn rejects_unknown_type_property_config() {
        assert_eq!(
            messages(json!({"headers": [{"key": "a", "type": "video"}]})),
            vec!["type must be one of: text, image, divider, spacer, row".to_owned()]
        );
        assert!(has(
            &messages(json!({"headers": [{"key": "a", "type": "divider", "label": "x"}]})),
            "unknown property \"label\""
        ));
        assert!(has(
            &messages(
                json!({"headers": [{"key": "a", "type": "spacer", "config": {"bold": true}}]})
            ),
            "config.bold is not allowed on block \"spacer\""
        ));
    }

    #[test]
    fn text_rules() {
        assert!(has(
            &messages(json!({"headers": [{"key": "a", "type": "text"}]})),
            "text is required"
        ));
        assert!(has(
            &messages(json!({"headers": [{"key": "a", "type": "text", "text": "x".repeat(301)}]})),
            "text must be at most 300 characters"
        ));
        assert_eq!(
            messages(
                json!({"headers": [{"key": "a", "type": "text", "text": "Toko {{namaToko}}"}]})
            ),
            vec!["unknown variable \"{{namaToko}}\"".to_owned()]
        );
        assert!(has(
            &messages(json!({"headers": [{"key": "a", "type": "divider", "text": "x"}]})),
            "block \"divider\" does not take text"
        ));
    }

    #[test]
    fn image_upload_only() {
        assert!(has(
            &messages(json!({"headers": [{"key": "a", "type": "image", "assetUuid": ASSET}]})),
            "source must be one of: upload"
        ));
        assert!(has(
            &messages(json!({"headers": [{"key": "a", "type": "image", "source": "upload"}]})),
            "assetUuid must be the uuid of an uploaded image"
        ));
        assert_eq!(
            messages(json!({"headers": [
                {"key": "a", "type": "image", "source": "company_logo", "assetUuid": ASSET}
            ]})),
            vec!["source must be one of: upload".to_owned()]
        );
        assert!(has(
            &messages(
                json!({"headers": [{"key": "a", "type": "text", "text": "x", "source": "upload"}]})
            ),
            "block \"text\" does not take a source"
        ));
    }

    #[test]
    fn config_ranges_and_enums() {
        let one = |mut block: Value| {
            block["key"] = json!("a");
            messages(json!({ "headers": [block] }))
        };
        assert!(has(
            &one(json!({"type": "text", "text": "x", "config": {"size": "huge"}})),
            "size must be one of: xs, sm, md, lg, xl"
        ));
        assert!(has(
            &one(json!({"type": "text", "text": "x", "config": {"bold": "ya"}})),
            "bold must be a boolean"
        ));
        assert!(has(
            &one(json!({"type": "text", "text": "x", "config": {"color": "red"}})),
            "color must be a #rrggbb colour"
        ));
        assert!(has(
            &one(json!({"type": "text", "text": "x", "config": {"align": "justify"}})),
            "align must be one of: left, center, right"
        ));
        assert!(has(
            &one(
                json!({"type": "image", "source": "upload", "assetUuid": ASSET, "config": {"height": 200}})
            ),
            "height must be a number between 12 and 100"
        ));
        assert!(has(
            &one(json!({"type": "spacer", "config": {"height": 1}})),
            "height must be a number between 2 and 60"
        ));
        assert!(has(
            &one(json!({"type": "divider", "config": {"thickness": 10}})),
            "thickness must be a number between 0.25 and 4"
        ));
        assert!(has(
            &one(json!({"type": "divider", "config": {"style": "dotted"}})),
            "style must be one of: solid, dashed"
        ));
        assert!(has(
            &one(json!({"type": "spacer", "layout": {"colSpan": 13}})),
            "colSpan must be an integer between 1 and 12"
        ));
    }

    #[test]
    fn layout_errors_do_not_hide_field_errors() {
        let doc = json!({
            "schemaVersion": 1,
            "fields": [{"key": "a", "type": "text", "label": "A",
                        "visibleIf": {"all": [{"field": "hantu", "op": "notEmpty"}]}}],
            "headers": [{"key": "b", "type": "video"}]
        });
        let result: Vec<String> =
            validate_form_schema(Some(&doc), &ValidateFormSchemaOptions::default())
                .into_iter()
                .map(|i| i.message)
                .collect();
        assert!(has(
            &result,
            "\"hantu\" is not a field in the same data scope"
        ));
        assert!(has(
            &result,
            "type must be one of: text, image, divider, spacer, row"
        ));
    }

    #[test]
    fn row_accepts_leaf_blocks_with_justify() {
        assert_eq!(
            issues(json!({"headers": [row(
                json!([
                    {"key": "logo", "type": "image", "source": "upload", "assetUuid": ASSET, "layout": {"colSpan": 3}},
                    {"key": "judul", "type": "text", "text": "Judul", "layout": {"colSpan": 6}}
                ]),
                Some(json!({"justify": "between"}))
            )]})),
            vec![]
        );
    }

    #[test]
    fn row_rules() {
        assert!(has(
            &messages(json!({"headers": [row(json!([]), None)]})),
            "row requires at least one block"
        ));
        let nested = issues(json!({"headers": [row(
            json!([row(json!([{"key": "x", "type": "spacer"}]), None)]),
            None
        )]}));
        assert!(nested.contains(&issue(
            "headers[0].blocks[0].type",
            "a row cannot contain another row"
        )));
        assert!(has(
            &messages(json!({"headers": [row(
                json!([{"key": "a", "type": "spacer"}]),
                Some(json!({"justify": "spread"}))
            )]})),
            "justify must be one of: stretch, start, center, end, between, around"
        ));
        assert!(has(
            &messages(
                json!({"headers": [{"key": "a", "type": "text", "text": "x", "blocks": []}]})
            ),
            "block \"text\" does not take blocks"
        ));
    }

    #[test]
    fn row_children_keys_and_cap() {
        assert_eq!(
            issues(json!({
                "headers": [row(json!([{"key": "garis", "type": "divider"}]), None)],
                "footers": [{"key": "garis", "type": "divider"}]
            })),
            vec![issue(
                "footers[0].key",
                "duplicate block key \"garis\" (first used at headers[0].blocks[0])"
            )]
        );
        let many: Vec<Value> = (0..MAX_LAYOUT_BLOCKS_PER_SECTION)
            .map(|i| json!({"key": format!("s{i}"), "type": "spacer"}))
            .collect();
        assert!(has(
            &messages(json!({"headers": [row(Value::Array(many), None)]})),
            "headers must not exceed 20 blocks"
        ));
    }

    #[test]
    fn collect_refs_inside_row() {
        assert_eq!(
            collect_layout_asset_references(&json!({"headers": [row(
                json!([{"key": "logo", "type": "image", "source": "upload", "assetUuid": ASSET}]),
                None
            )]})),
            vec![LayoutAssetReference {
                path: "headers[0].blocks[0].assetUuid".into(),
                asset_uuid: ASSET.into()
            }]
        );
    }

    #[test]
    fn collect_refs_both_sections() {
        assert_eq!(
            collect_layout_asset_references(&full_layout()),
            vec![
                LayoutAssetReference {
                    path: "headers[0].assetUuid".into(),
                    asset_uuid: LOGO.into()
                },
                LayoutAssetReference {
                    path: "footers[1].assetUuid".into(),
                    asset_uuid: ASSET.into()
                },
            ]
        );
        assert_eq!(collect_layout_asset_references(&Value::Null), vec![]);
        assert_eq!(
            collect_layout_asset_references(&json!({"headers": "rusak"})),
            vec![]
        );
    }

    #[test]
    fn text_variables_allow_spaces() {
        assert_eq!(
            layout_text_variables("{{a}} - {{ b }} - {c} - {{}}"),
            vec!["a".to_owned(), "b".to_owned()]
        );
    }

    #[test]
    fn hex_color() {
        assert!(is_hex_color("#0F172A"));
        assert!(!is_hex_color("#0F172"));
        assert!(!is_hex_color("0F172A0"));
    }
}
