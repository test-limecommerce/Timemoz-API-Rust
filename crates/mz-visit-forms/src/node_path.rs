//! Port `form-node-path.ts`: jalur data (`diserahkan.ttd`, `unit_list.2.foto`) →
//! node schema, dan penulisan nilai pada jalur itu di dalam `answers`.

use serde_json::Value;

use crate::consts::FormNodeType;
use crate::types::{FormDocument, FormNode};

/// Cari satu key di satu scope, menembus `section` yang transparan.
fn find_in_scope<'a>(fields: &'a [FormNode], key: &str) -> Option<&'a FormNode> {
    for node in fields {
        if node.ty == FormNodeType::Section {
            if let Some(found) = find_in_scope(node.children(), key) {
                return Some(found);
            }
            continue;
        }
        if node.key == key {
            return Some(node);
        }
    }
    None
}

/// `INDEX_PATTERN` `^\d+$`.
fn is_index(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// `findNodeByFieldPath` — `None` kalau jalurnya tidak ada.
pub fn find_node_by_field_path<'a>(
    schema: &'a FormDocument,
    field_path: &str,
) -> Option<&'a FormNode> {
    if field_path.is_empty() {
        return None;
    }
    let segments: Vec<&str> = field_path.split('.').collect();
    let mut fields: &[FormNode] = &schema.fields;
    let mut i = 0;
    while i < segments.len() {
        let segment = segments.get(i)?;
        let node = find_in_scope(fields, segment)?;
        match node.ty {
            FormNodeType::Group => {
                fields = node.children();
            }
            FormNodeType::Repeater => {
                let index = segments.get(i + 1)?;
                if !is_index(index) {
                    return None;
                }
                i += 1;
                fields = node.children();
            }
            _ => return (i == segments.len() - 1).then_some(node),
        }
        i += 1;
    }
    None
}

/// `Number(segment)` untuk segmen `^\d+$` (angka raksasa → tak terjangkau).
fn index_of(s: &str) -> usize {
    s.parse().unwrap_or(usize::MAX)
}

/// `setValueAtFieldPath` — diam saja kalau jalurnya tidak ada.
pub fn set_value_at_field_path(answers: &mut Value, field_path: &str, value: Value) {
    let segments: Vec<&str> = field_path.split('.').collect();
    let Some((last, init)) = segments.split_last() else {
        return;
    };
    let mut cursor: &mut Value = answers;
    for segment in init {
        let next = match cursor {
            Value::Array(a) => {
                if !is_index(segment) {
                    return;
                }
                a.get_mut(index_of(segment))
            }
            Value::Object(o) => o.get_mut(*segment),
            _ => return,
        };
        let Some(next) = next else {
            return;
        };
        cursor = next;
    }
    match cursor {
        Value::Array(a) => {
            if !is_index(last) {
                return;
            }
            if let Some(slot) = a.get_mut(index_of(last)) {
                *slot = value;
            }
        }
        Value::Object(o) => {
            o.insert((*last).to_owned(), value);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> FormDocument {
        FormDocument::from_value(&json!({
            "schemaVersion": 1,
            "fields": [
                {"key": "sec", "type": "section", "fields": [
                    {"key": "diserahkan", "type": "group", "fields": [{"key": "ttd", "type": "signature"}]}
                ]},
                {"key": "unit_list", "type": "repeater", "fields": [{"key": "foto", "type": "photo"}]},
                {"key": "nama", "type": "text"}
            ]
        }))
    }

    #[test]
    fn finds_nodes() {
        let s = schema();
        assert_eq!(
            find_node_by_field_path(&s, "diserahkan.ttd").unwrap().key,
            "ttd"
        );
        assert_eq!(
            find_node_by_field_path(&s, "unit_list.2.foto").unwrap().key,
            "foto"
        );
        assert_eq!(find_node_by_field_path(&s, "nama").unwrap().key, "nama");
        assert!(find_node_by_field_path(&s, "unit_list.foto").is_none());
        assert!(find_node_by_field_path(&s, "unit_list").is_none());
        assert!(find_node_by_field_path(&s, "nama.x").is_none());
        assert!(find_node_by_field_path(&s, "sec").is_none());
        assert!(find_node_by_field_path(&s, "").is_none());
    }

    #[test]
    fn sets_values() {
        let mut a = json!({"unit_list": [{"foto": {"fileUuid": "x"}}], "diserahkan": {}});
        set_value_at_field_path(
            &mut a,
            "unit_list.0.foto",
            json!({"fileUuid": "x", "url": "u"}),
        );
        set_value_at_field_path(&mut a, "diserahkan.ttd", json!(1));
        set_value_at_field_path(&mut a, "unit_list.5.foto", json!(2));
        set_value_at_field_path(&mut a, "missing.x", json!(3));
        assert_eq!(
            a,
            json!({"unit_list": [{"foto": {"fileUuid": "x", "url": "u"}}], "diserahkan": {"ttd": 1}})
        );
    }
}
