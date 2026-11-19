//! Helper kecil bersama: validasi UUID path (`ParseUUIDPipe`), `IsDateString`, `encodeURIComponent`.

pub use mz_core::uuid_rules::parse_uuid_pipe as parse_uuid_param;
use serde::{Deserialize, Deserializer};
use serde_json::Value;

/// `Some(Null)` untuk `null`, `None` untuk field yang tidak dikirim.
pub fn present<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(d).map(Some)
}

/// Negasi `IsNotEmpty` (`''`, `null`, `undefined`).
pub fn is_empty(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) => s.is_empty(),
        _ => false,
    }
}

pub fn is_nullish(v: Option<&Value>) -> bool {
    matches!(v, None | Some(Value::Null))
}

/// `@IsUUID()` pada nilai JSON.
pub fn is_uuid_value(v: Option<&Value>) -> bool {
    matches!(v, Some(Value::String(s)) if mz_core::uuid_rules::is_uuid(s))
}
