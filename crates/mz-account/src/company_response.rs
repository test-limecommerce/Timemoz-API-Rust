//! Port `src/companies/company-response.ts`.

use serde_json::Value;

/// Kolom `company` yang tidak boleh keluar di respons API.
pub const HIDDEN_COMPANY_FIELDS: &[&str] = &["profilePictureFullPath"];

/// `toCompanyResponse`: salinan baris company (objek JSON) tanpa kolom tersembunyi.
/// Nilai non-objek dikembalikan apa adanya.
pub fn to_company_response(row: Value) -> Value {
    match row {
        Value::Object(mut map) => {
            map.retain(|k, _| !HIDDEN_COMPANY_FIELDS.contains(&k.as_str()));
            Value::Object(map)
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn strips_hidden() {
        let out = to_company_response(json!({"id":1,"profilePictureFullPath":"gs://x","name":"A"}));
        assert_eq!(out, json!({"id":1,"name":"A"}));
    }
}
