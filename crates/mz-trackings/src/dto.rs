//! DTO + validasi gaya class-validator (`dto/*.ts`, `trackings.paginator.dto.ts`).
//!
//! Field body disimpan sebagai `Option<Value>` supaya "tidak ada" (`None`) dan
//! `null` (`Some(Value::Null)`) tetap dibedakan seperti di JS, dan supaya tipe
//! yang salah menghasilkan pesan class-validator, bukan galat serde.

use axum::http::Uri;
use mz_core::ApiError;
use mz_core::extract::Validate;
use mz_core::iso8601::is_iso8601;
use mz_core::qs::QVal;
pub use mz_core::uuid_rules::parse_uuid_pipe as parse_uuid_param;
use serde::{Deserialize, Deserializer};
use serde_json::Value;

use crate::calc::parse_int_js;

/// `Some(Null)` untuk `null`, `None` untuk field yang tidak dikirim.
fn present<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(d).map(Some)
}

/// `value !== '' && value !== null && value !== undefined` (negasi `IsNotEmpty`).
fn is_empty(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) => s.is_empty(),
        _ => false,
    }
}

fn is_nullish(v: Option<&Value>) -> bool {
    matches!(v, None | Some(Value::Null))
}

fn check_date(errs: &mut Vec<String>, v: Option<&Value>) {
    if is_empty(v) {
        errs.push("takenDate should not be empty".into());
    }
    if !matches!(v, Some(Value::String(s)) if is_iso8601(s)) {
        errs.push("takenDate must be a valid ISO 8601 date string".into());
    }
}

fn check_bool(errs: &mut Vec<String>, v: Option<&Value>) {
    if is_empty(v) {
        errs.push("isAbsent should not be empty".into());
    }
    if !matches!(v, Some(Value::Bool(_))) {
        errs.push("isAbsent must be a boolean value".into());
    }
}

fn check_optional_string(errs: &mut Vec<String>, name: &str, v: Option<&Value>) {
    if !is_nullish(v) && !matches!(v, Some(Value::String(_))) {
        errs.push(format!("{name} must be a string"));
    }
}

/// `CreateTrackingDto`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateTrackingDto {
    #[serde(default, deserialize_with = "present")]
    pub taken_date: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub is_absent: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub user_uuid: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub from: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub timezone: Option<Value>,
}

impl Validate for CreateTrackingDto {
    fn validate(&self) -> Vec<String> {
        let mut errs = Vec::new();
        check_date(&mut errs, self.taken_date.as_ref());
        check_bool(&mut errs, self.is_absent.as_ref());
        check_optional_string(&mut errs, "from", self.from.as_ref());
        check_optional_string(&mut errs, "timezone", self.timezone.as_ref());
        errs
    }
}

/// JS truthiness untuk string opsional (`if (dto.from)`).
pub fn truthy_str(v: Option<&Value>) -> Option<&str> {
    match v {
        Some(Value::String(s)) if !s.is_empty() => Some(s),
        _ => None,
    }
}

/// `UpdateTrackingDto = PartialType(CreateTrackingDto)`: semua field `@IsOptional`
/// (`null`/tak ada melewati validator lain).
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateTrackingDto {
    #[serde(default, deserialize_with = "present")]
    pub taken_date: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub is_absent: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub from: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub timezone: Option<Value>,
}

impl Validate for UpdateTrackingDto {
    fn validate(&self) -> Vec<String> {
        let mut errs = Vec::new();
        if !is_nullish(self.taken_date.as_ref()) {
            check_date(&mut errs, self.taken_date.as_ref());
        }
        if !is_nullish(self.is_absent.as_ref()) {
            check_bool(&mut errs, self.is_absent.as_ref());
        }
        check_optional_string(&mut errs, "from", self.from.as_ref());
        check_optional_string(&mut errs, "timezone", self.timezone.as_ref());
        errs
    }
}

/// `CreateSyncLocalDto`.
#[derive(Debug, Default, Deserialize)]
pub struct CreateSyncLocalDto {
    #[serde(default, deserialize_with = "present")]
    pub data: Option<Value>,
}

impl Validate for CreateSyncLocalDto {
    fn validate(&self) -> Vec<String> {
        if matches!(self.data, Some(Value::String(_))) {
            Vec::new()
        } else {
            vec!["data must be a string".into()]
        }
    }
}

/// `req.query` Express (`mz_core::qs`): kunci berulang / `k[]` = array, `k[x]` = objek.
pub struct QueryPairs(Vec<(String, QVal)>);

impl QueryPairs {
    pub fn from_uri(uri: &Uri) -> Self {
        Self(mz_core::qs::parse(uri.query().unwrap_or_default()))
    }

    #[cfg(test)]
    pub fn from_pairs(p: &[(&str, &str)]) -> Self {
        let enc = |s: &str| {
            s.bytes()
                .map(|b| {
                    if b.is_ascii_alphanumeric() {
                        char::from(b).to_string()
                    } else {
                        format!("%{b:02X}")
                    }
                })
                .collect::<String>()
        };
        let raw = p
            .iter()
            .map(|(k, v)| format!("{}={}", enc(k), enc(v)))
            .collect::<Vec<_>>()
            .join("&");
        Self(mz_core::qs::parse(&raw))
    }

    /// Nilai untuk query mentah tanpa DTO (array → elemen pertama, seperti dulu).
    pub fn first(&self, key: &str) -> Option<&str> {
        match mz_core::qs::get(&self.0, key)? {
            QVal::One(s) => Some(s),
            QVal::Many(v) => v.first().map(String::as_str),
            QVal::Obj => None,
        }
    }

    /// `@IsString() @IsOptional()`: tak ada = `Ok(None)`; berulang = bukan string.
    fn string(&self, key: &str, errs: &mut Vec<String>) -> Option<String> {
        match mz_core::qs::get(&self.0, key)? {
            QVal::One(one) => Some(one.clone()),
            QVal::Many(_) | QVal::Obj => {
                errs.push(format!("{key} must be a string"));
                None
            }
        }
    }

    /// `@IsNumber() @IsOptional() @Transform(parseInt)`.
    fn number(&self, key: &str, errs: &mut Vec<String>) -> Option<i64> {
        match mz_core::qs::get(&self.0, key)? {
            QVal::One(one) => {
                let parsed = parse_int_js(one);
                if parsed.is_none() {
                    errs.push(format!(
                        "{key} must be a number conforming to the specified constraints"
                    ));
                }
                parsed
            }
            QVal::Many(_) | QVal::Obj => {
                errs.push(format!(
                    "{key} must be a number conforming to the specified constraints"
                ));
                None
            }
        }
    }
}

/// `TrackingPaginatorDto`. Hanya `takenDate` dan `userUuid` yang dipakai service.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct TrackingPaginator {
    pub order_by: Option<String>,
    pub order_column: Option<String>,
    pub user_uuid: Option<String>,
    pub taken_date: Option<String>,
}

pub fn parse_paginator(q: &QueryPairs) -> Result<TrackingPaginator, ApiError> {
    let mut errs = Vec::new();
    q.number("pageNumber", &mut errs);
    q.number("itemsPerPage", &mut errs);
    let order_by = q.string("orderBy", &mut errs);
    let order_column = q.string("orderColumn", &mut errs);
    q.string("search", &mut errs);
    let user_uuid = q.string("userUuid", &mut errs);
    let taken_date = q.string("takenDate", &mut errs);
    if !errs.is_empty() {
        return Err(ApiError::Validation(errs));
    }
    Ok(TrackingPaginator {
        order_by,
        order_column,
        user_uuid,
        taken_date,
    })
}

/// `CountTrackingDto`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CountQuery {
    pub month: Option<String>,
    pub year: Option<String>,
    pub date: Option<String>,
}

pub fn parse_count(q: &QueryPairs) -> Result<CountQuery, ApiError> {
    let mut errs = Vec::new();
    let month = q.string("month", &mut errs);
    let year = q.string("year", &mut errs);
    let date = q.string("date", &mut errs);
    if !errs.is_empty() {
        return Err(ApiError::Validation(errs));
    }
    Ok(CountQuery { month, year, date })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn dto<T: for<'de> Deserialize<'de>>(v: Value) -> T {
        serde_json::from_value(v).unwrap_or_else(|_| unreachable_dto())
    }

    #[allow(clippy::panic)]
    fn unreachable_dto<T>() -> T {
        panic!("dto deserialization failed")
    }

    #[test]
    fn create_requires_date_and_bool() {
        let d: CreateTrackingDto = dto(json!({}));
        assert_eq!(
            d.validate(),
            vec![
                "takenDate should not be empty",
                "takenDate must be a valid ISO 8601 date string",
                "isAbsent should not be empty",
                "isAbsent must be a boolean value",
            ]
        );
        let ok: CreateTrackingDto =
            dto(json!({"takenDate":"2026-11-16","isAbsent":false,"from":"x"}));
        assert!(ok.validate().is_empty());
        let bad: CreateTrackingDto =
            dto(json!({"takenDate":"2026-11-16","isAbsent":"no","from":1,"timezone":true}));
        assert_eq!(
            bad.validate(),
            vec![
                "isAbsent must be a boolean value",
                "from must be a string",
                "timezone must be a string"
            ]
        );
    }

    #[test]
    fn update_skips_null_and_missing() {
        let d: UpdateTrackingDto = dto(json!({"takenDate":null}));
        assert!(d.validate().is_empty());
        let d: UpdateTrackingDto = dto(json!({"takenDate":"nope"}));
        assert_eq!(
            d.validate(),
            vec!["takenDate must be a valid ISO 8601 date string"]
        );
    }

    #[test]
    fn sync_requires_string() {
        let d: CreateSyncLocalDto = dto(json!({"data":[]}));
        assert_eq!(d.validate(), vec!["data must be a string"]);
        let d: CreateSyncLocalDto = dto(json!({"data":"[]"}));
        assert!(d.validate().is_empty());
    }

    #[test]
    fn paginator_parsing() {
        let q = QueryPairs::from_pairs(&[
            ("pageNumber", "2x"),
            ("takenDate", "2026-11-16"),
            ("userUuid", "u"),
        ]);
        let p = parse_paginator(&q);
        assert_eq!(
            p.ok(),
            Some(TrackingPaginator {
                order_by: None,
                order_column: None,
                user_uuid: Some("u".into()),
                taken_date: Some("2026-11-16".into()),
            })
        );
        let q =
            QueryPairs::from_pairs(&[("pageNumber", "abc"), ("orderBy", "a"), ("orderBy", "b")]);
        match parse_paginator(&q) {
            Err(ApiError::Validation(m)) => assert_eq!(
                m,
                vec![
                    "pageNumber must be a number conforming to the specified constraints",
                    "orderBy must be a string"
                ]
            ),
            _ => unreachable_dto(),
        }
    }

    #[test]
    fn uuid_pipe() {
        assert!(parse_uuid_param("9c9dd5b0-645e-499d-9c3e-d6cfc2f4ac55").is_ok());
        assert!(parse_uuid_param("9c9dd5b0645e499d9c3ed6cfc2f4ac55").is_err());
        assert!(parse_uuid_param("date-time").is_err());
    }
}

#[cfg(test)]
mod static_path_tests {
    use super::parse_uuid_param;

    #[test]
    fn static_segments_are_not_uuids() {
        for s in ["sync-local-db", "date-time"] {
            assert!(parse_uuid_param(s).is_err(), "{s}");
        }
    }
}
