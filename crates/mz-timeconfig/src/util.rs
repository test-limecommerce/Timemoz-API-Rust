//! Helper kecil bersama modul-modul crate ini.

pub use mz_core::uuid_rules::uuid_pipe as parse_uuid;

use serde_json::{Value, json};

/// `{ success: false, message, data: null }`.
pub fn failure(message: &str) -> Value {
    json!({ "success": false, "message": message, "data": null })
}

/// Nilai non-null (`undefined`/`null` = tidak dikirim).
pub fn defined(v: Option<&Value>) -> Option<&Value> {
    v.filter(|v| !v.is_null())
}
