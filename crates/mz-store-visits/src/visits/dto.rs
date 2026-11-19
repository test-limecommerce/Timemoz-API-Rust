//! DTO modul kunjungan mobile (class-validator + `ValidationPipe({ transform: true })`):
//! `CheckInDto`, `CheckOutDto`, `CancelVisitDto`, `ValidateLocationDto`, `StoreVisitEventDto`,
//! `ListMobileStoreVisitsQueryDto`, `MobileStoreVisitsSummaryQueryDto`,
//! `StoreVisitEventHistoryQueryDto`. Pesan per properti mengikuti urutan dekorator dari bawah.

use mz_core::extract::{check_int_range, validator_js_length};
use mz_core::iso8601::is_iso8601;
use mz_core::uuid_rules::is_uuid;
use mz_core::{ApiError, ApiResult};
use mz_sales_geo::dto::{
    Field, as_number, check_bool, check_number, js_to_string, parse_int_value, present,
};
use serde_json::{Map, Value};

use crate::enums::{StoreVisitMethod, StoreVisitStatus, is_enum_message};

fn finish<T>(errs: Vec<String>, v: Option<T>) -> ApiResult<T> {
    match v {
        Some(v) if errs.is_empty() => Ok(v),
        _ => Err(ApiError::Validation(errs)),
    }
}

/// `@IsUUID()` wajib.
fn check_uuid(errs: &mut Vec<String>, field: &str, v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::String(s)) if is_uuid(s) => Some(s.clone()),
        _ => {
            errs.push(format!("{field} must be a UUID"));
            None
        }
    }
}

/// `@IsOptional() @IsInt() @Min(0)` (pesan Min, IsInt).
fn check_opt_int_min0(errs: &mut Vec<String>, field: &str, m: &Map<String, Value>) -> Option<i64> {
    let v = present(m, field)?;
    let n = as_number(Some(v));
    let before = errs.len();
    if n.is_none_or(|x| x < 0.0) {
        errs.push(format!("{field} must not be less than 0"));
    }
    let int = n.filter(|x| x.fract() == 0.0);
    if int.is_none() {
        errs.push(format!("{field} must be an integer number"));
    }
    #[allow(clippy::cast_possible_truncation)]
    int.filter(|_| errs.len() == before).map(|x| x as i64)
}

/// Koordinat & akurasi wajib (`@IsNumber() @Min @Max`).
fn coord(
    errs: &mut Vec<String>,
    m: &Map<String, Value>,
    field: &str,
    min: f64,
    max: f64,
) -> Option<f64> {
    check_number(errs, field, as_number(m.get(field)), Some(min), Some(max))
}

/// `@IsOptional() @IsBoolean()`.
fn opt_bool(errs: &mut Vec<String>, m: &Map<String, Value>, field: &str) -> Option<bool> {
    let v = present(m, field)?;
    check_bool(errs, field, Some(v))
}

/// `@IsOptional() @IsString() @MaxLength(max)` (pesan MaxLength, IsString). `null` = `Field::Null`.
fn opt_max_string(
    errs: &mut Vec<String>,
    m: &Map<String, Value>,
    field: &str,
    max: usize,
) -> Field<String> {
    match m.get(field) {
        None => Field::Absent,
        Some(Value::Null) => Field::Null,
        Some(v) => {
            let s = v.as_str();
            if s.is_none_or(|s| validator_js_length(s) > max) {
                errs.push(format!(
                    "{field} must be shorter than or equal to {max} characters"
                ));
            }
            if s.is_none() {
                errs.push(format!("{field} must be a string"));
            }
            s.map_or(Field::Absent, |s| Field::Val(s.to_owned()))
        }
    }
}

/// `CheckInDto`.
#[derive(Debug, Clone, PartialEq)]
pub struct CheckIn {
    pub store_id: String,
    pub client_request_id: String,
    pub client_at: Option<i64>,
    pub lat: f64,
    pub lng: f64,
    pub accuracy_meters: f64,
    pub method: Option<StoreVisitMethod>,
    pub is_mock: Option<bool>,
    pub is_override: Option<bool>,
    pub override_reason: Option<String>,
}

/// Validasi `CheckInDto`.
pub fn parse_check_in(m: &Map<String, Value>) -> ApiResult<CheckIn> {
    let mut e = Vec::new();
    let store_id = check_uuid(&mut e, "storeId", m.get("storeId"));
    let client_request_id = check_uuid(&mut e, "clientRequestId", m.get("clientRequestId"));
    let client_at = check_opt_int_min0(&mut e, "clientAt", m);
    let lat = coord(&mut e, m, "lat", -90.0, 90.0);
    let lng = coord(&mut e, m, "lng", -180.0, 180.0);
    let acc = coord(&mut e, m, "accuracyMeters", 0.0, 100_000.0);
    let method = present(m, "method").and_then(|v| {
        let parsed = v.as_str().and_then(|s| s.parse::<StoreVisitMethod>().ok());
        if parsed.is_none() {
            e.push(is_enum_message(
                "method",
                &crate::enum_values!(StoreVisitMethod),
            ));
        }
        parsed
    });
    let is_mock = opt_bool(&mut e, m, "isMock");
    let is_override = opt_bool(&mut e, m, "isOverride");
    let override_reason = match opt_max_string(&mut e, m, "overrideReason", 255) {
        Field::Val(s) => Some(s),
        _ => None,
    };
    let v = match (store_id, client_request_id, lat, lng, acc) {
        (Some(store_id), Some(client_request_id), Some(lat), Some(lng), Some(accuracy_meters)) => {
            Some(CheckIn {
                store_id,
                client_request_id,
                client_at,
                lat,
                lng,
                accuracy_meters,
                method,
                is_mock,
                is_override,
                override_reason,
            })
        }
        _ => None,
    };
    finish(e, v)
}

/// `CheckOutDto`.
#[derive(Debug, Clone, PartialEq)]
pub struct CheckOut {
    pub status: StoreVisitStatus,
    pub client_at: Option<i64>,
    pub lat: f64,
    pub lng: f64,
    pub accuracy_meters: f64,
    pub is_mock: Option<bool>,
    pub note: Option<String>,
    /// `Absent` = jangan tulis; `Null` = Prisma menolak `tags: null` (500).
    pub tags: Field<Vec<String>>,
}

/// Validasi `CheckOutDto`.
pub fn parse_check_out(m: &Map<String, Value>) -> ApiResult<CheckOut> {
    let mut e = Vec::new();
    let status = match m.get("status") {
        Some(Value::String(s)) => s.parse::<StoreVisitStatus>().ok(),
        _ => None,
    };
    if status.is_none() {
        e.push(is_enum_message(
            "status",
            &crate::enum_values!(StoreVisitStatus),
        ));
    }
    let client_at = check_opt_int_min0(&mut e, "clientAt", m);
    let lat = coord(&mut e, m, "lat", -90.0, 90.0);
    let lng = coord(&mut e, m, "lng", -180.0, 180.0);
    let acc = coord(&mut e, m, "accuracyMeters", 0.0, 100_000.0);
    let is_mock = opt_bool(&mut e, m, "isMock");
    let note = match opt_max_string(&mut e, m, "note", 1000) {
        Field::Val(s) => Some(s),
        _ => None,
    };
    let tags = match m.get("tags") {
        None => Field::Absent,
        Some(Value::Null) => Field::Null,
        Some(v) => check_tags(&mut e, v).map_or(Field::Absent, Field::Val),
    };
    let v = match (status, lat, lng, acc) {
        (Some(status), Some(lat), Some(lng), Some(accuracy_meters)) => Some(CheckOut {
            status,
            client_at,
            lat,
            lng,
            accuracy_meters,
            is_mock,
            note,
            tags,
        }),
        _ => None,
    };
    finish(e, v)
}

/// `@IsArray() @ArrayMaxSize(10) @IsString({each}) @MaxLength(40, {each})` (dari bawah).
fn check_tags(e: &mut Vec<String>, v: &Value) -> Option<Vec<String>> {
    let before = e.len();
    let items: Vec<&Value> = match v {
        Value::Array(a) => a.iter().collect(),
        other => vec![other],
    };
    if items
        .iter()
        .any(|x| x.as_str().is_none_or(|s| validator_js_length(s) > 40))
    {
        e.push("each value in tags must be shorter than or equal to 40 characters".into());
    }
    if items.iter().any(|x| !x.is_string()) {
        e.push("each value in tags must be a string".into());
    }
    let arr = v.as_array();
    if arr.is_none_or(|a| a.len() > 10) {
        e.push("tags must contain no more than 10 elements".into());
    }
    if arr.is_none() {
        e.push("tags must be an array".into());
    }
    if e.len() != before {
        return None;
    }
    arr.map(|a| {
        a.iter()
            .filter_map(|x| x.as_str().map(str::to_owned))
            .collect()
    })
}

/// `CancelVisitDto`.
pub fn parse_cancel(m: &Map<String, Value>) -> ApiResult<Option<String>> {
    let mut e = Vec::new();
    let reason = match opt_max_string(&mut e, m, "reason", 255) {
        Field::Val(s) => Some(s),
        _ => None,
    };
    finish(e, Some(reason))
}

/// `ValidateLocationDto`.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidateLocation {
    pub store_id: String,
    pub lat: f64,
    pub lng: f64,
    pub accuracy_meters: f64,
    pub is_mock: Option<bool>,
}

/// Validasi `ValidateLocationDto`.
pub fn parse_validate_location(m: &Map<String, Value>) -> ApiResult<ValidateLocation> {
    let mut e = Vec::new();
    let store_id = check_uuid(&mut e, "storeId", m.get("storeId"));
    let lat = coord(&mut e, m, "lat", -90.0, 90.0);
    let lng = coord(&mut e, m, "lng", -180.0, 180.0);
    let acc = coord(&mut e, m, "accuracyMeters", 0.0, 100_000.0);
    let is_mock = opt_bool(&mut e, m, "isMock");
    let v = match (store_id, lat, lng, acc) {
        (Some(store_id), Some(lat), Some(lng), Some(accuracy_meters)) => Some(ValidateLocation {
            store_id,
            lat,
            lng,
            accuracy_meters,
            is_mock,
        }),
        _ => None,
    };
    finish(e, v)
}

/// `StoreVisitEventDto` (keempatnya `@IsNumber()` saja).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StoreVisitEvent {
    pub lat: f64,
    pub lng: f64,
    pub accuracy: f64,
    pub timestamp: f64,
}

/// Validasi `StoreVisitEventDto`.
pub fn parse_event(m: &Map<String, Value>) -> ApiResult<StoreVisitEvent> {
    let mut e = Vec::new();
    let mut num = |f: &str| check_number(&mut e, f, as_number(m.get(f)), None, None);
    let (lat, lng, accuracy, timestamp) =
        (num("lat"), num("lng"), num("accuracy"), num("timestamp"));
    let v = match (lat, lng, accuracy, timestamp) {
        (Some(lat), Some(lng), Some(accuracy), Some(timestamp)) => Some(StoreVisitEvent {
            lat,
            lng,
            accuracy,
            timestamp,
        }),
        _ => None,
    };
    finish(e, v)
}

// ───────────────────────── query ─────────────────────────

fn is_blank(v: Option<&Value>) -> bool {
    matches!(v, None | Some(Value::Null)) || v.and_then(Value::as_str) == Some("")
}

/// `@IsOptional() @IsDateString()` atas nilai query.
fn opt_date_string(e: &mut Vec<String>, m: &Map<String, Value>, field: &str) -> Option<String> {
    let v = present(m, field)?;
    match v.as_str() {
        Some(s) if is_iso8601(s) => Some(s.to_owned()),
        _ => {
            e.push(format!("{field} must be a valid ISO 8601 date string"));
            None
        }
    }
}

/// `@IsOptional() @IsUUID()` atas nilai query.
fn opt_uuid(e: &mut Vec<String>, m: &Map<String, Value>, field: &str) -> Option<String> {
    let v = present(m, field)?;
    check_uuid(e, field, Some(v))
}

/// `page` `@IsInt() @Min(1)` (pesan Min, IsInt) atas hasil `parseInt`.
fn page_value(e: &mut Vec<String>, raw: Option<f64>) -> i64 {
    let before = e.len();
    if raw.is_none_or(|x| x.is_nan() || x < 1.0) {
        e.push("page must not be less than 1".into());
    }
    if raw.is_none_or(|x| !x.is_finite() || x.fract() != 0.0) {
        e.push("page must be an integer number".into());
    }
    #[allow(clippy::cast_possible_truncation)]
    raw.filter(|_| e.len() == before).map_or(1, |x| x as i64)
}

/// `limit` `@IsInt() @Min(1) @Max(100)`.
fn limit_value(e: &mut Vec<String>, raw: Option<f64>) -> i64 {
    check_int_range(e, "limit", raw, 1, 100).unwrap_or(20)
}

#[allow(clippy::cast_precision_loss)]
fn parse_int_f64(v: &Value) -> Option<f64> {
    Some(parse_int_value(v).map_or(f64::NAN, |n| n as f64))
}

/// `ListMobileStoreVisitsQueryDto`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ListVisitsQuery {
    pub store_id: Option<String>,
    pub status: Option<Vec<StoreVisitStatus>>,
    pub start_date: Option<String>,
    pub end_date: Option<String>,
    pub search: Option<String>,
    pub page: i64,
    pub limit: i64,
}

/// Validasi `ListMobileStoreVisitsQueryDto` (`toArray`, `toInt` dengan fallback).
pub fn parse_list_query(m: &Map<String, Value>) -> ApiResult<ListVisitsQuery> {
    let mut e = Vec::new();
    let store_id = opt_uuid(&mut e, m, "storeId");
    let status = if is_blank(m.get("status")) {
        None
    } else {
        let items: Vec<String> = match m.get("status") {
            Some(Value::Array(a)) => a.iter().map(js_to_string).collect(),
            Some(v) => js_to_string(v).split(',').map(str::to_owned).collect(),
            None => Vec::new(),
        };
        let parsed: Vec<Option<StoreVisitStatus>> = items.iter().map(|s| s.parse().ok()).collect();
        if parsed.iter().any(Option::is_none) {
            e.push(format!(
                "each value in {}",
                is_enum_message("status", &crate::enum_values!(StoreVisitStatus))
            ));
            None
        } else {
            Some(parsed.into_iter().flatten().collect())
        }
    };
    let start_date = opt_date_string(&mut e, m, "startDate");
    let end_date = opt_date_string(&mut e, m, "endDate");
    let search = present(m, "search").and_then(|v| match v {
        Value::String(s) => Some(s.clone()),
        _ => {
            e.push("search must be a string".into());
            None
        }
    });
    let page_raw = if is_blank(m.get("page")) {
        Some(1.0)
    } else {
        m.get("page").and_then(parse_int_f64)
    };
    let page = page_value(&mut e, page_raw);
    let limit_raw = if is_blank(m.get("limit")) {
        Some(20.0)
    } else {
        m.get("limit").and_then(parse_int_f64)
    };
    let limit = limit_value(&mut e, limit_raw);
    finish(
        e,
        Some(ListVisitsQuery {
            store_id,
            status,
            start_date,
            end_date,
            search,
            page,
            limit,
        }),
    )
}

/// `MobileStoreVisitsSummaryQueryDto` (`@IsDateString() @IsNotEmpty()` → IsNotEmpty, IsDateString).
pub fn parse_summary_query(m: &Map<String, Value>) -> ApiResult<String> {
    let mut e = Vec::new();
    let v = m.get("date");
    let empty = match v {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) => s.is_empty(),
        _ => false,
    };
    if empty {
        e.push("date should not be empty".into());
    }
    let date = match v {
        Some(Value::String(s)) if is_iso8601(s) => Some(s.clone()),
        _ => {
            e.push("date must be a valid ISO 8601 date string".into());
            None
        }
    };
    finish(e, date)
}

/// `StoreVisitEventHistoryQueryDto`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct EventHistoryQuery {
    pub visit_id: Option<String>,
    pub event: Option<String>,
    pub start_date: Option<String>,
    pub end_date: Option<String>,
    pub page: i64,
    pub limit: i64,
}

/// Validasi `StoreVisitEventHistoryQueryDto` (`value !== undefined ? parseInt : default`).
pub fn parse_event_history_query(m: &Map<String, Value>) -> ApiResult<EventHistoryQuery> {
    let mut e = Vec::new();
    let visit_id = opt_uuid(&mut e, m, "visitId");
    let event = present(m, "event").and_then(|v| match v.as_str() {
        Some(s @ ("enter" | "exit")) => Some(s.to_owned()),
        _ => {
            e.push("event must be one of the following values: enter, exit".into());
            None
        }
    });
    let start_date = opt_date_string(&mut e, m, "startDate");
    let end_date = opt_date_string(&mut e, m, "endDate");
    let page_raw = m.get("page").map_or(Some(1.0), parse_int_f64);
    // `@IsOptional`: `null` (tak mungkin dari query) dilewati.
    let page = page_value(&mut e, page_raw);
    let limit_raw = m.get("limit").map_or(Some(20.0), parse_int_f64);
    let limit = limit_value(&mut e, limit_raw);
    finish(
        e,
        Some(EventHistoryQuery {
            visit_id,
            event,
            start_date,
            end_date,
            page,
            limit,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    fn errs<T: std::fmt::Debug>(r: ApiResult<T>) -> Vec<String> {
        match r {
            Err(ApiError::Validation(e)) => e,
            other => panic!("expected validation error, got {other:?}"),
        }
    }

    const S: &str = "3b7e1a90-22c4-4f6d-9a11-8c2de5f47031";

    #[test]
    fn check_in_minimal_and_errors() {
        let ok = parse_check_in(&obj(json!({
            "storeId": S, "clientRequestId": S, "lat": -7.75, "lng": 110.29, "accuracyMeters": 12
        })))
        .unwrap();
        assert_eq!(ok.method, None);
        assert_eq!(
            errs(parse_check_in(&obj(
                json!({"storeId": "x", "lat": 91, "method": "fly", "isMock": "y", "overrideReason": 5})
            ))),
            vec![
                "storeId must be a UUID",
                "clientRequestId must be a UUID",
                "lat must not be greater than 90",
                "lng must not be greater than 180",
                "lng must not be less than -180",
                "lng must be a number conforming to the specified constraints",
                "accuracyMeters must not be greater than 100000",
                "accuracyMeters must not be less than 0",
                "accuracyMeters must be a number conforming to the specified constraints",
                "method must be one of the following values: gps, qr, manual",
                "isMock must be a boolean value",
                "overrideReason must be shorter than or equal to 255 characters",
                "overrideReason must be a string",
            ]
        );
    }

    /// `CheckOutDto` (store-visits.service.spec.ts:2042).
    #[test]
    fn check_out_dto() {
        let base = json!({"status": "success", "lat": 0, "lng": 0, "accuracyMeters": 5});
        assert!(parse_check_out(&obj(base.clone())).is_ok());
        let mut m = obj(base.clone());
        m.remove("status");
        assert_eq!(
            errs(parse_check_out(&m)),
            vec![
                "status must be one of the following values: success, closed, moved, noOwner, rejected, other"
            ]
        );
        let mut m = obj(base.clone());
        m.insert(
            "tags".into(),
            json!(["a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k"]),
        );
        assert_eq!(
            errs(parse_check_out(&m)),
            vec!["tags must contain no more than 10 elements"]
        );
        let mut m = obj(base.clone());
        m.insert("tags".into(), json!(["x".repeat(41)]));
        assert_eq!(
            errs(parse_check_out(&m)),
            vec!["each value in tags must be shorter than or equal to 40 characters"]
        );
        let mut m = obj(base);
        m.insert("lat".into(), json!(-91));
        assert_eq!(
            errs(parse_check_out(&m)),
            vec!["lat must not be less than -90"]
        );
    }

    #[test]
    fn cancel_dto() {
        assert_eq!(parse_cancel(&Map::new()).unwrap(), None);
        assert_eq!(
            errs(parse_cancel(&obj(json!({"reason": "x".repeat(256)})))),
            vec!["reason must be shorter than or equal to 255 characters"]
        );
    }

    #[test]
    fn list_query() {
        let q = parse_list_query(&obj(
            json!({"status": "success,closed", "page": "2", "limit": ""}),
        ))
        .unwrap();
        assert_eq!(
            q.status,
            Some(vec![StoreVisitStatus::success, StoreVisitStatus::closed])
        );
        assert_eq!((q.page, q.limit), (2, 20));
        assert_eq!(
            errs(parse_list_query(&obj(
                json!({"status": ["bad"], "page": "x", "limit": "500"})
            ))),
            vec![
                "each value in status must be one of the following values: success, closed, moved, noOwner, rejected, other",
                "page must not be less than 1",
                "page must be an integer number",
                "limit must not be greater than 100",
            ]
        );
    }

    #[test]
    fn summary_and_history_query() {
        assert_eq!(
            errs(parse_summary_query(&Map::new())),
            vec![
                "date should not be empty",
                "date must be a valid ISO 8601 date string"
            ]
        );
        assert_eq!(
            parse_summary_query(&obj(json!({"date": "2026-08-20"}))).unwrap(),
            "2026-08-20"
        );
        assert_eq!(
            errs(parse_event_history_query(&obj(
                json!({"event": "in", "page": ""})
            ))),
            vec![
                "event must be one of the following values: enter, exit",
                "page must not be less than 1",
                "page must be an integer number"
            ]
        );
    }

    #[test]
    fn event_dto() {
        assert_eq!(
            errs(parse_event(&obj(
                json!({"lat": 1, "lng": "2", "accuracy": 3})
            ))),
            vec![
                "lng must be a number conforming to the specified constraints",
                "timestamp must be a number conforming to the specified constraints"
            ]
        );
    }
}
