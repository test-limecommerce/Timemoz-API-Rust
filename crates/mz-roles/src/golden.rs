//! Uji golden registry fitur/template terhadap keluaran Node (`tests/golden/*.json`, regenerasi
//! lewat `tools/dump_node_registry.js`). Registry-nya sendiri ada di `mz_core::feature_registry`.

use mz_core::feature_registry::{FEATURES, ROLE_TEMPLATES};
use serde::Serialize;

#[derive(Serialize)]
struct Data<T> {
    data: T,
}

/// Byte-identik dengan `JSON.stringify({ data: FEATURES })` Node.
#[test]
fn golden_features_match_node() {
    let rust = serde_json::to_string(&Data { data: FEATURES }).unwrap_or_default();
    let node = include_str!("../tests/golden/features.json");
    assert_eq!(rust, node.trim_end());
}

/// Byte-identik dengan `JSON.stringify({ data: ROLE_TEMPLATES })` Node.
#[test]
fn golden_templates_match_node() {
    let rust = serde_json::to_string(&Data {
        data: &*ROLE_TEMPLATES,
    })
    .unwrap_or_default();
    let node = include_str!("../tests/golden/templates.json");
    assert_eq!(rust, node.trim_end());
}
