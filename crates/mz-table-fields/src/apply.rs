//! Penegakan kebijakan pada NILAI (`apply-field-policy.ts`, `table-fields.decorator.ts`),
//! SIAP DIPAKAI route sales-tracker yang kelak di-port. TIDAK dipasang ke route mana pun di
//! crate ini (interceptor `@TableFields` hanya dipakai route yang belum di-port).
//!
//! Catatan urutan kunci: `serde_json::Value` di workspace ini menjaga urutan penyisipan
//! (fitur `preserve_order`), jadi varian `Value` ([`apply_field_policy`],
//! [`censor_response_body`]) menghasilkan urutan yang sama dengan Node (`fieldPolicy`
//! ditempel di akhir). Pemanggil yang memakai struct bisa memakai [`censored_paths`] +
//! [`field_policy_envelope`] pada tipe mereka sendiri.

use serde::Serialize;
use serde_json::Value;

use crate::policy::CensoredColumns;
use crate::registry::{FieldVisibilityMode, table_by_key};

/// `TABLE_FIELDS_ROOT_PATH`: record telanjang sebagai akar respons.
pub const TABLE_FIELDS_ROOT_PATH: &str = ".";

/// `TableFieldsTarget` (`@TableFields(tableKey, { path, within })`).
#[derive(Debug, Clone, Copy)]
pub struct TableFieldsTarget {
    pub table_key: &'static str,
    /// `'data'` untuk envelope list/detail, `'.'` untuk record telanjang.
    pub path: &'static str,
    /// Jalur baris tabel DI DALAM satu record (mis. `store`).
    pub within: Option<&'static str>,
}

/// Bentuk `fieldPolicy` di envelope respons (§9).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FieldPolicyEnvelope {
    #[serde(rename = "tableKey")]
    pub table_key: String,
    pub hidden: Vec<String>,
    pub masked: Vec<String>,
    pub revealable: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub within: Option<String>,
}

/// `resolveResponsePath`: jalur titik di dalam body; `None` bila tidak ada.
pub fn resolve_response_path<'a>(body: &'a Value, path: &str) -> Option<&'a Value> {
    if path == TABLE_FIELDS_ROOT_PATH {
        return Some(body);
    }
    let mut current = body;
    for segment in path.split('.') {
        current = current.as_object()?.get(segment)?;
    }
    Some(current)
}

fn resolve_response_path_mut<'a>(body: &'a mut Value, path: &str) -> Option<&'a mut Value> {
    if path == TABLE_FIELDS_ROOT_PATH {
        return Some(body);
    }
    let mut current = body;
    for segment in path.split('.') {
        current = current.as_object_mut()?.get_mut(segment)?;
    }
    Some(current)
}

/// Jalur nilai di dalam SATU record untuk tiap kolom tersensor, urutan registry. `within`
/// ditambahkan sebagai prefiks (penyensoran berhenti di sub-objek tabelnya).
pub fn censored_paths(
    table_key: &str,
    censored: &CensoredColumns,
    within: Option<&str>,
) -> Vec<String> {
    let Some(table) = table_by_key(table_key) else {
        return Vec::new();
    };
    table
        .columns
        .iter()
        .filter(|c| censored.contains_key(c.key))
        .map(|c| match within {
            Some(w) => format!("{w}.{}", c.response_path()),
            None => c.response_path().to_owned(),
        })
        .collect()
}

/// Satu kolom di satu record → `null`. Kunci yang TIDAK ADA tidak dibuat; yang ADA tidak dihapus.
fn censor_at(record: &mut Value, path: &str) {
    let mut segments: Vec<&str> = path.split('.').collect();
    let Some(leaf) = segments.pop() else { return };
    let mut container = record;
    for segment in segments {
        match container.as_object_mut().and_then(|o| o.get_mut(segment)) {
            Some(next) => container = next,
            None => return,
        }
    }
    if let Some(slot) = container.as_object_mut().and_then(|o| o.get_mut(leaf)) {
        *slot = Value::Null;
    }
}

/// `applyFieldPolicy`: menyensor satu record atau array record di TEMPAT.
pub fn apply_field_policy(
    target: &mut Value,
    table_key: &str,
    censored: &CensoredColumns,
    within: Option<&str>,
) {
    if censored.is_empty() {
        return;
    }
    let paths = censored_paths(table_key, censored, within);
    if paths.is_empty() {
        return;
    }
    let mut each = |record: &mut Value| {
        if record.is_object() {
            for p in &paths {
                censor_at(record, p);
            }
        }
    };
    match target {
        Value::Array(items) => items.iter_mut().for_each(&mut each),
        other => each(other),
    }
}

/// `fieldPolicyEnvelope`: ringkasan §9 dalam urutan registry (bukan urutan peta).
pub fn field_policy_envelope(
    table_key: &str,
    censored: &CensoredColumns,
    within: Option<&str>,
) -> FieldPolicyEnvelope {
    let mut env = FieldPolicyEnvelope {
        table_key: table_key.to_owned(),
        hidden: Vec::new(),
        masked: Vec::new(),
        revealable: Vec::new(),
        within: within.map(str::to_owned),
    };
    for column in table_by_key(table_key).map_or(&[][..], |t| t.columns) {
        match censored.get(column.key) {
            Some(FieldVisibilityMode::Hidden) => env.hidden.push(column.key.to_owned()),
            Some(FieldVisibilityMode::Masked) => env.masked.push(column.key.to_owned()),
            Some(FieldVisibilityMode::Revealable) => env.revealable.push(column.key.to_owned()),
            _ => {}
        }
    }
    env
}

/// `censorResponseBody`: sensor di jalur `path` lalu tempel `fieldPolicy` (bukan untuk akar
/// array). Tanpa kolom tersensor body tidak disentuh sama sekali (tanpa `fieldPolicy` kosong).
pub fn censor_response_body(
    body: &mut Value,
    table_key: &str,
    path: &str,
    censored: &CensoredColumns,
    within: Option<&str>,
) {
    if censored.is_empty() || !(body.is_object() || body.is_array()) {
        return;
    }
    if let Some(target) = resolve_response_path_mut(body, path) {
        apply_field_policy(target, table_key, censored, within);
    }
    if let Some(obj) = body.as_object_mut() {
        let env = field_policy_envelope(table_key, censored, within);
        obj.insert(
            "fieldPolicy".to_owned(),
            serde_json::to_value(env).unwrap_or(Value::Null),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn censored(pairs: &[(&str, FieldVisibilityMode)]) -> CensoredColumns {
        pairs.iter().map(|(k, m)| ((*k).to_owned(), *m)).collect()
    }

    #[test]
    fn nulls_existing_keys_only() {
        let c = censored(&[
            ("phone", FieldVisibilityMode::Masked),
            ("address", FieldVisibilityMode::Hidden),
        ]);
        let mut body = json!({"data": [{"phone": "0812", "name": "A"}, {"name": "B"}]});
        censor_response_body(&mut body, "stores", "data", &c, None);
        assert_eq!(body["data"][0]["phone"], Value::Null);
        assert_eq!(body["data"][0]["name"], "A");
        assert!(body["data"][1].get("phone").is_none());
        assert_eq!(body["fieldPolicy"]["tableKey"], "stores");
        assert_eq!(body["fieldPolicy"]["hidden"], json!(["address"]));
        assert_eq!(body["fieldPolicy"]["masked"], json!(["phone"]));
        assert!(body["fieldPolicy"].get("within").is_none());
    }

    #[test]
    fn within_limits_censoring_to_child() {
        let c = censored(&[("group", FieldVisibilityMode::Hidden)]);
        let mut body = json!({"data": [{"group": "snap", "store": {"group": "g"}}]});
        censor_response_body(&mut body, "stores", "data", &c, Some("store"));
        assert_eq!(body["data"][0]["group"], "snap");
        assert_eq!(body["data"][0]["store"]["group"], Value::Null);
        assert_eq!(body["fieldPolicy"]["within"], "store");
    }

    #[test]
    fn empty_policy_leaves_body_untouched() {
        let mut body = json!({"data": [{"phone": "1"}]});
        let before = body.clone();
        censor_response_body(&mut body, "stores", "data", &CensoredColumns::new(), None);
        assert_eq!(body, before);
    }

    #[test]
    fn root_array_gets_no_envelope() {
        let c = censored(&[("phone", FieldVisibilityMode::Masked)]);
        let mut body = json!([{"phone": "1"}]);
        censor_response_body(&mut body, "stores", ".", &c, None);
        assert_eq!(body[0]["phone"], Value::Null);
        assert!(body.is_array());
    }

    #[test]
    fn path_resolution() {
        let body = json!({"a": {"b": 1}});
        assert_eq!(resolve_response_path(&body, "a.b"), Some(&json!(1)));
        assert_eq!(resolve_response_path(&body, "a.c"), None);
        assert_eq!(resolve_response_path(&body, "."), Some(&body));
    }
}
