//! Port `form-visibility.ts`: evaluasi `visibleIf` (§2.6) dan pemangkasan nilai
//! tersembunyi (§7.1). Fungsi murni, dipakai validator jawaban DAN generator PDF.

use std::cmp::Ordering;

use serde_json::{Map, Value};

use crate::consts::{FormNodeType, VisibleIfOp};
use crate::types::{FormDocument, FormNode, FormVisibleIfCondition};
use crate::value_util::{is_empty_value, js_includes, js_str_cmp, js_strict_eq};

/// Satu scope data: akar dokumen, isi `group`, atau satu baris `repeater`.
pub type AnswerScope = Map<String, Value>;

fn same_value(left: Option<&Value>, right: Option<&Value>) -> bool {
    match (left, right) {
        (Some(Value::Array(l)), Some(Value::Array(r))) => {
            // Multiselect: himpunannya yang dibandingkan.
            l.len() == r.len() && l.iter().all(|item| js_includes(r, Some(item)))
        }
        (Some(Value::Array(_)), _) | (_, Some(Value::Array(_))) => false,
        _ => js_strict_eq(left, right),
    }
}

fn is_member(left: Option<&Value>, list: Option<&Value>) -> bool {
    let Some(Value::Array(list)) = list else {
        return false;
    };
    match left {
        Some(Value::Array(l)) => l.iter().any(|item| js_includes(list, Some(item))),
        _ => js_includes(list, left),
    }
}

fn compare(left: Option<&Value>, right: Option<&Value>) -> Option<Ordering> {
    match (left?, right?) {
        (Value::Number(l), Value::Number(r)) => {
            let (l, r) = (l.as_f64()?, r.as_f64()?);
            Some(if l == r {
                Ordering::Equal
            } else if l < r {
                Ordering::Less
            } else {
                Ordering::Greater
            })
        }
        (Value::String(l), Value::String(r)) => Some(js_str_cmp(l, r)),
        _ => None,
    }
}

/// `evaluateCondition` — rujukan field dibaca dari scope yang sedang berjalan.
pub fn evaluate_condition(c: &FormVisibleIfCondition, values: &AnswerScope) -> bool {
    let left = values.get(&c.field);
    let right = c.value.as_ref();
    match c.op {
        VisibleIfOp::Empty => is_empty_value(left),
        VisibleIfOp::NotEmpty => !is_empty_value(left),
        VisibleIfOp::Eq => same_value(left, right),
        VisibleIfOp::Ne => !same_value(left, right),
        VisibleIfOp::In => is_member(left, right),
        VisibleIfOp::Nin => !is_member(left, right),
        VisibleIfOp::Gt | VisibleIfOp::Gte | VisibleIfOp::Lt | VisibleIfOp::Lte => {
            let Some(order) = compare(left, right) else {
                return false;
            };
            match c.op {
                VisibleIfOp::Gt => order == Ordering::Greater,
                VisibleIfOp::Gte => order != Ordering::Less,
                VisibleIfOp::Lt => order == Ordering::Less,
                _ => order != Ordering::Greater,
            }
        }
    }
}

/// `isNodeVisible`: `all` semua, `any` minimal satu, keduanya di-AND.
pub fn is_node_visible(node: &FormNode, values: &AnswerScope) -> bool {
    let Some(rule) = &node.visible_if else {
        return true;
    };
    let all = rule.all.as_deref().unwrap_or(&[]);
    let any = rule.any.as_deref().unwrap_or(&[]);
    if !all.is_empty() && !all.iter().all(|c| evaluate_condition(c, values)) {
        return false;
    }
    if !any.is_empty() && !any.iter().any(|c| evaluate_condition(c, values)) {
        return false;
    }
    true
}

fn prune_scope(fields: &[FormNode], values: &mut AnswerScope, parent_visible: bool) -> bool {
    let mut changed = false;
    for node in fields {
        if node.ty.is_display() {
            continue;
        }
        let visible = parent_visible && is_node_visible(node, values);
        if node.ty == FormNodeType::Section {
            if prune_scope(node.children(), values, visible) {
                changed = true;
            }
            continue;
        }
        if !visible {
            if values.remove(&node.key).is_some() {
                changed = true;
            }
            continue;
        }
        match node.ty {
            FormNodeType::Group => {
                if let Some(Value::Object(child)) = values.get_mut(&node.key)
                    && prune_scope(node.children(), child, true)
                {
                    changed = true;
                }
            }
            FormNodeType::Repeater => {
                if let Some(Value::Array(rows)) = values.get_mut(&node.key) {
                    for row in rows.iter_mut() {
                        if let Value::Object(row) = row
                            && prune_scope(node.children(), row, true)
                        {
                            changed = true;
                        }
                    }
                }
            }
            _ => {}
        }
    }
    changed
}

fn count_nodes(fields: &[FormNode]) -> usize {
    fields.iter().map(|n| 1 + count_nodes(n.children())).sum()
}

/// `pruneToFixedPoint` — buang di tempat sampai stabil.
pub fn prune_to_fixed_point(schema: &FormDocument, values: &mut AnswerScope) {
    let limit = count_nodes(&schema.fields) + 1;
    for _ in 0..limit {
        if !prune_scope(&schema.fields, values, true) {
            return;
        }
    }
}

/// `pruneHiddenValues` — salinan yang sudah dipangkas.
pub fn prune_hidden_values(schema: &FormDocument, values: &AnswerScope) -> AnswerScope {
    let mut pruned = values.clone();
    prune_to_fixed_point(schema, &mut pruned);
    pruned
}

#[cfg(test)]
mod tests {
    //! Port `form-visibility.spec.ts`.
    use super::*;
    use serde_json::json;

    fn node(v: Value) -> FormNode {
        FormNode::from_value(&v).unwrap()
    }

    fn scope(v: Value) -> AnswerScope {
        v.as_object().unwrap().clone()
    }

    fn schema(fields: Value) -> FormDocument {
        FormDocument::from_value(&json!({ "schemaVersion": 1, "fields": fields }))
    }

    fn gated_by(key: &str, condition: Value) -> Value {
        json!({ "key": key, "type": "text", "label": key, "visibleIf": { "all": [condition] } })
    }

    fn visible_with(condition: Value, values: Value) -> bool {
        is_node_visible(&node(gated_by("target", condition)), &scope(values))
    }

    fn prune(doc: &FormDocument, values: Value) -> Value {
        Value::Object(prune_hidden_values(doc, &scope(values)))
    }

    #[test]
    fn type_categories() {
        for t in [
            FormNodeType::Heading,
            FormNodeType::Paragraph,
            FormNodeType::Divider,
        ] {
            assert!(t.is_display() && !t.is_container());
        }
        for t in [
            FormNodeType::Section,
            FormNodeType::Group,
            FormNodeType::Repeater,
        ] {
            assert!(t.is_container() && !t.is_display());
        }
        for t in [
            FormNodeType::Text,
            FormNodeType::Number,
            FormNodeType::Photo,
            FormNodeType::Select,
        ] {
            assert!(!t.is_display() && !t.is_container());
        }
    }

    #[test]
    fn no_rule_always_visible() {
        assert!(is_node_visible(
            &node(json!({"key": "a", "type": "text"})),
            &Map::new()
        ));
        assert!(is_node_visible(
            &node(json!({"key": "a", "type": "text", "visibleIf": {}})),
            &Map::new()
        ));
    }

    #[test]
    fn eq_ne() {
        let c = json!({"field": "status", "op": "eq", "value": "open"});
        assert!(visible_with(c.clone(), json!({"status": "open"})));
        assert!(!visible_with(c.clone(), json!({"status": "closed"})));
        assert!(!visible_with(c, json!({})));
        let c = json!({"field": "tags", "op": "eq", "value": ["a", "b"]});
        assert!(visible_with(c.clone(), json!({"tags": ["b", "a"]})));
        assert!(visible_with(c.clone(), json!({"tags": ["a", "b"]})));
        assert!(!visible_with(c.clone(), json!({"tags": ["a"]})));
        assert!(!visible_with(c.clone(), json!({"tags": ["a", "b", "c"]})));
        assert!(!visible_with(c, json!({"tags": "a"})));
        assert!(!visible_with(
            json!({"field": "tags", "op": "eq", "value": "a"}),
            json!({"tags": ["a"]})
        ));
        let c = json!({"field": "status", "op": "ne", "value": "open"});
        assert!(visible_with(c.clone(), json!({"status": "closed"})));
        assert!(!visible_with(c.clone(), json!({"status": "open"})));
        assert!(visible_with(c, json!({})));
    }

    #[test]
    fn in_nin() {
        let c = json!({"field": "status", "op": "in", "value": ["open", "draft"]});
        assert!(visible_with(c.clone(), json!({"status": "draft"})));
        assert!(!visible_with(c, json!({"status": "closed"})));
        let c = json!({"field": "tags", "op": "in", "value": ["a", "b"]});
        assert!(visible_with(c.clone(), json!({"tags": ["c", "b"]})));
        assert!(!visible_with(c.clone(), json!({"tags": ["c", "d"]})));
        assert!(!visible_with(c, json!({"tags": []})));
        assert!(!visible_with(
            json!({"field": "status", "op": "in", "value": "open"}),
            json!({"status": "open"})
        ));
        let c = json!({"field": "tags", "op": "nin", "value": ["a", "b"]});
        assert!(visible_with(c.clone(), json!({"tags": ["c"]})));
        assert!(!visible_with(c.clone(), json!({"tags": ["a", "c"]})));
        assert!(visible_with(c, json!({})));
    }

    #[test]
    fn ordering() {
        let c = |op: &str| json!({"field": "n", "op": op, "value": 5});
        assert!(visible_with(c("gt"), json!({"n": 6})));
        assert!(!visible_with(c("gt"), json!({"n": 5})));
        assert!(visible_with(c("gte"), json!({"n": 5})));
        assert!(visible_with(c("lt"), json!({"n": 4})));
        assert!(!visible_with(c("lt"), json!({"n": 5})));
        assert!(visible_with(c("lte"), json!({"n": 5})));
        assert!(visible_with(
            json!({"field": "d", "op": "gte", "value": "2026-01-01"}),
            json!({"d": "2026-08-24"})
        ));
        assert!(visible_with(
            json!({"field": "t", "op": "lt", "value": "12:00"}),
            json!({"t": "09:30"})
        ));
        assert!(!visible_with(
            json!({"field": "t", "op": "lt", "value": "12:00"}),
            json!({"t": "23:30"})
        ));
        assert!(!visible_with(c("gt"), json!({"n": "6"})));
        assert!(!visible_with(c("lt"), json!({"n": "4"})));
        assert!(!visible_with(c("gte"), json!({})));
        assert!(!visible_with(c("lte"), json!({"n": null})));
        assert!(!visible_with(c("gt"), json!({"n": [1, 2]})));
    }

    #[test]
    fn empty_not_empty() {
        let e = json!({"field": "v", "op": "empty"});
        let ne = json!({"field": "v", "op": "notEmpty"});
        for v in [
            json!({}),
            json!({"v": null}),
            json!({"v": "   "}),
            json!({"v": []}),
            json!({"v": {}}),
        ] {
            assert!(visible_with(e.clone(), v));
        }
        assert!(!visible_with(e.clone(), json!({"v": 0})));
        assert!(!visible_with(e, json!({"v": false})));
        assert!(visible_with(ne.clone(), json!({"v": 0})));
        assert!(visible_with(ne.clone(), json!({"v": false})));
        assert!(visible_with(ne.clone(), json!({"v": "x"})));
        assert!(visible_with(ne.clone(), json!({"v": ["x"]})));
        assert!(!visible_with(ne, json!({"v": ""})));
    }

    #[test]
    fn all_and_any() {
        let target = node(
            json!({"key": "target", "type": "text", "label": "target", "visibleIf": {
            "all": [{"field": "a", "op": "eq", "value": 1}],
            "any": [{"field": "b", "op": "eq", "value": 2}, {"field": "c", "op": "eq", "value": 3}]}}),
        );
        assert!(is_node_visible(&target, &scope(json!({"a": 1, "b": 2}))));
        assert!(is_node_visible(&target, &scope(json!({"a": 1, "c": 3}))));
        assert!(!is_node_visible(
            &target,
            &scope(json!({"a": 1, "b": 9, "c": 9}))
        ));
        assert!(!is_node_visible(&target, &scope(json!({"a": 9, "b": 2}))));
        let only_any = node(json!({"key": "x", "type": "text",
            "visibleIf": {"all": [], "any": [{"field": "b", "op": "eq", "value": 2}]}}));
        assert!(is_node_visible(&only_any, &scope(json!({"b": 2}))));
        assert!(!is_node_visible(&only_any, &scope(json!({"b": 1}))));
    }

    #[test]
    fn prune_basic() {
        let doc = schema(json!([
            {"key": "status", "type": "select", "label": "Status"},
            gated_by("alasan", json!({"field": "status", "op": "eq", "value": "closed"}))
        ]));
        assert_eq!(
            prune(&doc, json!({"status": "open", "alasan": "basi"})),
            json!({"status": "open"})
        );
        assert_eq!(
            prune(&doc, json!({"status": "closed", "alasan": "stok"})),
            json!({"status": "closed", "alasan": "stok"})
        );
        let values = scope(json!({"status": "open", "alasan": "basi"}));
        let _ = prune_hidden_values(&doc, &values);
        assert_eq!(
            Value::Object(values),
            json!({"status": "open", "alasan": "basi"})
        );
        let doc = schema(json!([
            {"key": "judul", "type": "heading", "label": "Judul"},
            {"key": "garis", "type": "divider"},
            {"key": "catatan", "type": "text", "label": "Catatan"}
        ]));
        assert_eq!(
            prune(&doc, json!({"catatan": "halo"})),
            json!({"catatan": "halo"})
        );
    }

    #[test]
    fn prune_chains() {
        let doc = schema(json!([
            {"key": "a", "type": "select", "label": "A"},
            gated_by("b", json!({"field": "a", "op": "eq", "value": "ya"})),
            gated_by("c", json!({"field": "b", "op": "notEmpty"}))
        ]));
        assert_eq!(
            prune(&doc, json!({"a": "tidak", "b": "basi", "c": "basi juga"})),
            json!({"a": "tidak"})
        );
        assert_eq!(
            prune(&doc, json!({"a": "ya", "b": "isi", "c": "ikut"})),
            json!({"a": "ya", "b": "isi", "c": "ikut"})
        );
        let doc = schema(json!([
            {"key": "f0", "type": "text", "label": "F0"},
            gated_by("f5", json!({"field": "f4", "op": "notEmpty"})),
            gated_by("f4", json!({"field": "f3", "op": "notEmpty"})),
            gated_by("f3", json!({"field": "f2", "op": "notEmpty"})),
            gated_by("f2", json!({"field": "f1", "op": "notEmpty"})),
            gated_by("f1", json!({"field": "f0", "op": "eq", "value": "ya"}))
        ]));
        assert_eq!(
            prune(
                &doc,
                json!({"f0": "tidak", "f1": "x", "f2": "x", "f3": "x", "f4": "x", "f5": "x"})
            ),
            json!({"f0": "tidak"})
        );
    }

    #[test]
    fn prune_containers() {
        let doc = schema(json!([
            {"key": "mode", "type": "select", "label": "Mode"},
            {"key": "blok", "type": "section", "label": "Blok",
             "visibleIf": {"all": [{"field": "mode", "op": "eq", "value": "penuh"}]},
             "fields": [{"key": "detail", "type": "text", "label": "Detail"}]}
        ]));
        assert_eq!(
            prune(&doc, json!({"mode": "ringkas", "detail": "basi"})),
            json!({"mode": "ringkas"})
        );
        assert_eq!(
            prune(&doc, json!({"mode": "penuh", "detail": "isi"})),
            json!({"mode": "penuh", "detail": "isi"})
        );

        let doc = schema(
            json!([{"key": "toko", "type": "group", "label": "Toko", "fields": [
                {"key": "buka", "type": "boolean", "label": "Buka"},
                gated_by("jam", json!({"field": "buka", "op": "eq", "value": true}))
            ]}]),
        );
        assert_eq!(
            prune(&doc, json!({"toko": {"buka": false, "jam": "09:00"}})),
            json!({"toko": {"buka": false}})
        );

        let doc = schema(json!([
            {"key": "mode", "type": "select", "label": "Mode"},
            {"key": "toko", "type": "group", "label": "Toko",
             "visibleIf": {"all": [{"field": "mode", "op": "eq", "value": "penuh"}]},
             "fields": [{"key": "jam", "type": "time", "label": "Jam"}]}
        ]));
        assert_eq!(
            prune(&doc, json!({"mode": "ringkas", "toko": {"jam": "09:00"}})),
            json!({"mode": "ringkas"})
        );
    }

    #[test]
    fn prune_repeater() {
        let doc = schema(json!([
            {"key": "rusak", "type": "boolean", "label": "Rusak (akar)"},
            {"key": "unit_list", "type": "repeater", "label": "Unit", "fields": [
                {"key": "rusak", "type": "boolean", "label": "Rusak"},
                gated_by("catatan", json!({"field": "rusak", "op": "eq", "value": true}))
            ]}
        ]));
        assert_eq!(
            prune(
                &doc,
                json!({"rusak": true, "unit_list": [{"rusak": true, "catatan": "penyok"}, {"rusak": false, "catatan": "basi"}]})
            ),
            json!({"rusak": true, "unit_list": [{"rusak": true, "catatan": "penyok"}, {"rusak": false}]})
        );
        assert_eq!(
            prune(&doc, json!({"unit_list": "bukan array"})),
            json!({"unit_list": "bukan array"})
        );
        assert_eq!(
            prune(&doc, json!({"unit_list": [null, 7]})),
            json!({"unit_list": [null, 7]})
        );
    }

    #[test]
    fn fixed_point_in_place() {
        let doc = schema(json!([
            {"key": "a", "type": "select", "label": "A"},
            gated_by("b", json!({"field": "a", "op": "eq", "value": "ya"}))
        ]));
        let mut values = scope(json!({"a": "tidak", "b": "basi"}));
        prune_to_fixed_point(&doc, &mut values);
        assert_eq!(Value::Object(values), json!({"a": "tidak"}));
        let doc = schema(json!([{"key": "a", "type": "text", "label": "A"}]));
        let mut values = scope(json!({"a": "x"}));
        prune_to_fixed_point(&doc, &mut values);
        assert_eq!(Value::Object(values), json!({"a": "x"}));
    }
}
