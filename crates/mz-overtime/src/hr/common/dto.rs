//! Validator DTO bersama lembur (bunyi pesan class-validator persis; urutan pesan satu
//! properti = dekorator dari BAWAH ke atas, properti sesuai urutan deklarasi kelas):
//! `dto/date-key.validator.ts`, `reject-overtime.dto.ts`, `confirm-over-quota.dto.ts`,
//! `overtime-request-status-filter.ts`, plus pembaca query gaya `qs` (Express).
//!
//! Pola `@ValidateIf((_, v) => v !== undefined)`: field yang ADA tetapi `null` tetap
//! divalidasi (ditolak); hanya yang tidak dikirim yang dilewati. Pakai
//! [`present`] sebagai `deserialize_with` agar `null` → `Some(Value::Null)`.

use axum::extract::{FromRequestParts, Path};
use axum::http::request::Parts;
use chrono::NaiveDate;
use mz_core::ApiError;
use mz_core::extract::Validate;
pub use mz_core::extract::check_int_range as check_query_int;
pub use mz_core::extract::validator_js_length;
pub use mz_core::jsfmt::{number as js_number, trim as js_trim};
pub use mz_core::uuid_rules::is_uuid_v4;
use serde::{Deserialize, Deserializer};
use serde_json::Value;

use super::enums::{OvertimeRequestStatus, status_enum_values};

// ── query gaya `qs` ──────────────────────────────────────────────────────────

/// `req.query` Express: satu sumber di `mz_core::qs`.
pub use mz_core::qs::{QVal, get as query_get};

/// Query mentah → `req.query`.
pub fn query_map(raw: Option<&str>) -> Vec<(String, QVal)> {
    mz_core::qs::parse(raw.unwrap_or_default())
}

// ── `@IsDateKey()` ───────────────────────────────────────────────────────────

/// `IsDateKey`: string `YYYY-MM-DD` ketat yang ada di kalender.
pub fn is_date_key(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_str)
        .and_then(crate::zone::parse_date_key)
        .is_some()
}

/// Versi string (query).
pub fn is_date_key_str(value: &str) -> bool {
    crate::zone::parse_date_key(value).is_some()
}

/// Pesan bawaan `IsDateKey`: `<field> harus tanggal YYYY-MM-DD yang valid`.
pub fn date_key_message(field: &str) -> String {
    format!("{field} harus tanggal YYYY-MM-DD yang valid")
}

// ── pemeriksa class-validator kecil ─────────────────────────────────────────

/// `@IsBoolean()`.
pub fn is_boolean_message(field: &str) -> String {
    format!("{field} must be a boolean value")
}

/// `deserialize_with` untuk field opsional yang membedakan "tidak dikirim" (`None`) dari
/// `null` (`Some(Value::Null)`). Wajib dipasang bersama `#[serde(default)]`.
pub fn present<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(d).map(Some)
}

/// `@ValidateIf(v !== undefined) @IsBoolean()` — dorong pesan bila ada dan bukan boolean.
pub fn validate_optional_bool(errors: &mut Vec<String>, field: &str, value: Option<&Value>) {
    if let Some(v) = value
        && !v.is_boolean()
    {
        errors.push(is_boolean_message(field));
    }
}

/// Angka JS bulat (`Number.isInteger`), termasuk `5.0`.
pub fn as_js_integer(v: &Value) -> Option<i64> {
    if let Some(i) = v.as_i64() {
        return Some(i);
    }
    let f = v.as_f64()?;
    #[allow(clippy::cast_possible_truncation)]
    (f.is_finite() && f.fract() == 0.0 && f.abs() < 9_007_199_254_740_992.0).then_some(f as i64)
}

/// `@ValidateIf(v !== undefined) @IsInt() @Min(min) @Max(max)` — urutan pesan Max, Min, IsInt.
pub fn validate_optional_int(
    errors: &mut Vec<String>,
    field: &str,
    value: Option<&Value>,
    min: i64,
    max: i64,
) {
    let Some(v) = value else { return };
    let num = v.as_f64();
    if num.is_none_or(|n| n.is_nan() || n > max as f64) {
        errors.push(format!("{field} must not be greater than {max}"));
    }
    if num.is_none_or(|n| n.is_nan() || n < min as f64) {
        errors.push(format!("{field} must not be less than {min}"));
    }
    if as_js_integer(v).is_none() {
        errors.push(format!("{field} must be an integer number"));
    }
}

/// `UUID_RE` service settings: UUID versi apa pun (`/^[0-9a-f]{8}-…$/i`).
pub fn is_uuid_any(v: &str) -> bool {
    let b = v.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

// ── `RejectOvertimeDto` ──────────────────────────────────────────────────────

/// Body penolakan (`RejectOvertimeDto`): `@Transform(trim) @IsString() @IsNotEmpty()
/// @MaxLength(500) reason`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RejectOvertimeDto {
    #[serde(default)]
    pub reason: Option<Value>,
}

impl RejectOvertimeDto {
    /// Alasan setelah trim (hanya bermakna sesudah validasi lolos).
    pub fn reason(&self) -> String {
        self.reason
            .as_ref()
            .and_then(Value::as_str)
            .map(|s| js_trim(s).to_owned())
            .unwrap_or_default()
    }
}

impl Validate for RejectOvertimeDto {
    fn validate(&self) -> Vec<String> {
        let mut e = Vec::new();
        let trimmed: Option<Value> = self.reason.as_ref().map(|v| match v.as_str() {
            Some(s) => Value::String(js_trim(s).to_owned()),
            None => v.clone(),
        });
        let s = trimmed.as_ref().and_then(Value::as_str);
        // MaxLength(500)
        if s.is_none_or(|s| validator_js_length(s) > 500) {
            e.push("reason must be shorter than or equal to 500 characters".to_owned());
        }
        // IsNotEmpty: '' / null / undefined
        if matches!(&trimmed, None | Some(Value::Null)) || s == Some("") {
            e.push("reason should not be empty".to_owned());
        }
        // IsString
        if s.is_none() {
            e.push("reason must be a string".to_owned());
        }
        e
    }
}

// ── `ConfirmOverQuotaDto` ────────────────────────────────────────────────────

/// Body `{ confirmOverQuota? }` (`ConfirmOverQuotaDto`): `null` ditolak (400).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfirmOverQuotaDto {
    #[serde(default, deserialize_with = "present")]
    pub confirm_over_quota: Option<Value>,
}

impl ConfirmOverQuotaDto {
    /// Nilai boolean (sesudah validasi).
    pub fn confirm_over_quota(&self) -> Option<bool> {
        self.confirm_over_quota.as_ref().and_then(Value::as_bool)
    }
}

impl Validate for ConfirmOverQuotaDto {
    fn validate(&self) -> Vec<String> {
        let mut e = Vec::new();
        validate_optional_bool(&mut e, "confirmOverQuota", self.confirm_over_quota.as_ref());
        e
    }
}

// ── filter `status` (`TransformStatusFilter` + `@IsOptional @IsArray @IsEnum(each)`) ──

/// `status` query: satu nilai atau berulang → daftar status. `None` = tidak diisi.
/// Pesan (urutan IsEnum lalu IsArray) didorong ke `errors`; nilai tak dikenal tidak
/// dikembalikan (validasi gagal → 400 sebelum service dipanggil).
pub fn status_filter(
    errors: &mut Vec<String>,
    field: &str,
    value: Option<&QVal>,
) -> Option<Vec<OvertimeRequestStatus>> {
    let list = value.cloned()?.into_vec();
    let parsed: Vec<Option<OvertimeRequestStatus>> = list.iter().map(|s| s.parse().ok()).collect();
    if parsed.iter().any(Option::is_none) {
        errors.push(format!(
            "each value in {field} must be one of the following values: {}",
            status_enum_values()
        ));
    }
    Some(parsed.into_iter().flatten().collect())
}

// ── `:id` (`ParseUUIDPipe({ version: '4' })`) ────────────────────────────────

/// Pesan bawaan Nest 10 `ParseUUIDPipe` dengan `version: '4'` (perhatikan spasi `v 4`).
pub const UUID_V4_MESSAGE: &str = "Validation failed (uuid v 4 is expected)";

/// Param `:id` tervalidasi UUID v4. Pipe Nest berjalan SESUDAH guard dan SEBELUM
/// `ValidationPipe` body, jadi extractor ini harus diletakkan sebelum `ValidatedJson`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UuidV4Id(pub String);

/// Murni: hasil pipe untuk nilai `:id` mentah.
pub fn parse_uuid_v4_param(raw: &str) -> Result<String, ApiError> {
    if is_uuid_v4(raw) {
        Ok(raw.to_owned())
    } else {
        Err(ApiError::bad_request(UUID_V4_MESSAGE))
    }
}

impl<S: Send + Sync> FromRequestParts<S> for UuidV4Id {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let Path(id) = Path::<String>::from_request_parts(parts, state)
            .await
            .map_err(|_| ApiError::bad_request(UUID_V4_MESSAGE))?;
        parse_uuid_v4_param(&id).map(Self)
    }
}

// ── angka query (`@Type(() => Number)`) ─────────────────────────────────────

/// Nilai query → angka (`@Type(() => Number)`): satu nilai → `Number(s)`; array → bukan number.
pub fn query_number(v: Option<&QVal>) -> Option<f64> {
    match v? {
        QVal::One(s) => Some(js_number(s)),
        QVal::Many(_) | QVal::Obj => None,
    }
}

/// `@IsDateKey()` atas nilai query (array selalu gagal).
pub fn query_date_key(
    errors: &mut Vec<String>,
    field: &str,
    v: Option<&QVal>,
) -> Option<NaiveDate> {
    let parsed = match v {
        Some(QVal::One(s)) => crate::zone::parse_date_key(s),
        _ => None,
    };
    if parsed.is_none() {
        errors.push(date_key_message(field));
    }
    parsed
}

#[cfg(test)]
mod tests {
    #![allow(clippy::indexing_slicing, clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use serde_json::json;

    fn reject(v: Value) -> Vec<String> {
        serde_json::from_value::<RejectOvertimeDto>(v)
            .unwrap_or_default()
            .validate()
    }

    #[test]
    fn date_key_validator() {
        for ok in ["2026-10-21", "2024-02-29"] {
            assert!(is_date_key(Some(&json!(ok))));
        }
        for bad in [
            json!("2026-02-30"),
            json!("2026-1-5"),
            json!(20261021),
            json!("2026-10-21T00:00"),
        ] {
            assert!(!is_date_key(Some(&bad)));
        }
        assert_eq!(
            date_key_message("value"),
            "value harus tanggal YYYY-MM-DD yang valid"
        );
    }

    #[test]
    fn reject_dto() {
        assert!(reject(json!({"reason": "  Tidak perlu  "})).is_empty());
        let dto: RejectOvertimeDto =
            serde_json::from_value(json!({"reason": "  Tidak perlu  "})).unwrap_or_default();
        assert_eq!(dto.reason(), "Tidak perlu");
        assert_eq!(
            reject(json!({})),
            vec![
                "reason must be shorter than or equal to 500 characters",
                "reason should not be empty",
                "reason must be a string"
            ]
        );
        assert_eq!(
            reject(json!({"reason": ""})),
            vec!["reason should not be empty"]
        );
        assert_eq!(
            reject(json!({"reason": "   "})),
            vec!["reason should not be empty"]
        );
        assert_eq!(
            reject(json!({"reason": "x".repeat(501)})),
            vec!["reason must be shorter than or equal to 500 characters"]
        );
        assert!(reject(json!({"reason": "x".repeat(500)})).is_empty());
        assert_eq!(
            reject(json!({"reason": 5})),
            vec![
                "reason must be shorter than or equal to 500 characters",
                "reason must be a string"
            ]
        );
    }

    #[test]
    fn confirm_over_quota_dto() {
        let parse = |v: Value| serde_json::from_value::<ConfirmOverQuotaDto>(v).unwrap_or_default();
        assert!(parse(json!({})).validate().is_empty());
        assert_eq!(parse(json!({})).confirm_over_quota(), None);
        assert_eq!(
            parse(json!({"confirmOverQuota": true})).confirm_over_quota(),
            Some(true)
        );
        for bad in [
            json!("true"),
            json!("false"),
            json!(1),
            json!([true]),
            Value::Null,
        ] {
            assert_eq!(
                parse(json!({ "confirmOverQuota": bad })).validate(),
                vec!["confirmOverQuota must be a boolean value"]
            );
        }
    }

    #[test]
    fn status_filter_rules() {
        let run = |q: &str| {
            let map = query_map(Some(q));
            let mut e = Vec::new();
            let v = status_filter(&mut e, "status", query_get(&map, "status"));
            (v, e)
        };
        let (v, e) = run("status=APPROVED");
        assert_eq!(v, Some(vec![OvertimeRequestStatus::APPROVED]));
        assert!(e.is_empty());
        let (v, _) = run("status=APPROVED&status=REJECTED");
        assert_eq!(
            v,
            Some(vec![
                OvertimeRequestStatus::APPROVED,
                OvertimeRequestStatus::REJECTED
            ])
        );
        for bad in [
            "status=DONE",
            "status=APPROVED&status=DONE",
            "status=approved",
            "status=",
        ] {
            let (_, e) = run(bad);
            assert_eq!(e.len(), 1, "{bad}");
        }
        assert_eq!(run("").0, None);
    }

    #[test]
    fn query_map_like_qs() {
        let m = query_map(Some("search=a&search=b&x=%20y+z&k[]=1"));
        assert_eq!(
            query_get(&m, "search"),
            Some(&QVal::Many(vec!["a".into(), "b".into()]))
        );
        assert_eq!(query_get(&m, "x"), Some(&QVal::One(" y z".into())));
        assert_eq!(query_get(&m, "k"), Some(&QVal::Many(vec!["1".into()])));
    }

    #[test]
    fn int_and_uuid_checks() {
        let mut e = Vec::new();
        validate_optional_int(&mut e, "n", Some(&json!("7")), 0, 31);
        assert_eq!(
            e,
            vec![
                "n must not be greater than 31",
                "n must not be less than 0",
                "n must be an integer number"
            ]
        );
        let mut e = Vec::new();
        validate_optional_int(&mut e, "n", Some(&json!(5.0)), 0, 31);
        assert!(e.is_empty());
        assert!(is_uuid_v4("5d2c8a9b-7e34-4f1a-b6c2-9a8e7d6c5b02"));
        assert!(!is_uuid_v4("6ba7b810-9dad-11d1-80b4-00c04fd430c8"));
        assert!(is_uuid_any("6ba7b810-9dad-11d1-80b4-00c04fd430c8"));
    }

    #[test]
    fn uuid_v4_param_like_parse_uuid_pipe() {
        const U: &str = "5d2c8a9b-7e34-4f1a-b6c2-9a8e7d6c5b02";
        assert_eq!(parse_uuid_v4_param(U).ok(), Some(U.to_owned()));
        assert!(parse_uuid_v4_param(&U.to_uppercase()).is_ok());
        for bad in [
            "not-a-uuid",
            "123",
            "orders",
            "6ba7b810-9dad-11d1-80b4-00c04fd430c8",
        ] {
            assert!(
                matches!(
                    parse_uuid_v4_param(bad),
                    Err(ApiError::Message { ref message, .. }) if message == UUID_V4_MESSAGE
                ),
                "{bad}"
            );
        }
    }

    #[test]
    fn query_int_order_max_min_int() {
        let mut e = Vec::new();
        assert_eq!(check_query_int(&mut e, "page", Some(2.0), 1, 10), Some(2));
        assert_eq!(check_query_int(&mut e, "page", None, 1, 10), None);
        assert_eq!(
            e,
            [
                "page must not be greater than 10",
                "page must not be less than 1",
                "page must be an integer number",
            ]
        );
        let mut e = Vec::new();
        assert_eq!(
            check_query_int(
                &mut e,
                "page",
                query_number(Some(&QVal::One("1.5".into()))),
                1,
                10
            ),
            None
        );
        assert_eq!(e, ["page must be an integer number"]);
    }
}
