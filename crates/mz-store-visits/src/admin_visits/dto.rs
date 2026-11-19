//! `ListAdminStoreVisitsQueryDto` + `ExportStoreVisitsQueryDto` (`OmitType(page, limit)` + `format`)
//! — validasi manual meniru `ValidationPipe({ transform: true })`: properti dalam urutan deklarasi,
//! dekorator dari bawah ke atas.

use mz_core::uuid_rules::{is_uuid, is_uuid_v4};
use mz_sales_geo::dto::{js_to_string, parse_int_value};
use serde_json::{Map, Value};

use crate::enum_values;
use crate::enums::{
    ExportFormat, GpsValidation, StoreVisitDateRange, StoreVisitState, StoreVisitStatus,
};
use crate::plan_dates::matches_plan_date_pattern;

/// Query daftar/export kunjungan admin.
#[derive(Debug, Clone, PartialEq)]
pub struct VisitListQuery {
    pub start_date: Option<String>,
    pub end_date: Option<String>,
    /// Bawaan `last7Days`.
    pub range: StoreVisitDateRange,
    pub sales_id: Option<Vec<String>>,
    pub store_id: Option<Vec<String>>,
    pub status: Option<Vec<StoreVisitStatus>>,
    pub state: Option<StoreVisitState>,
    pub gps_validation: Option<Vec<GpsValidation>>,
    pub search: Option<String>,
    pub group_id: Option<Vec<String>>,
    pub unassigned: Option<bool>,
    pub planned: Option<bool>,
    pub page: i64,
    pub limit: i64,
    /// Export saja (bawaan `csv`).
    pub format: ExportFormat,
}

impl Default for VisitListQuery {
    fn default() -> Self {
        Self {
            start_date: None,
            end_date: None,
            range: StoreVisitDateRange::last7Days,
            sales_id: None,
            store_id: None,
            status: None,
            state: None,
            gps_validation: None,
            search: None,
            group_id: None,
            unassigned: None,
            planned: None,
            page: 1,
            limit: 20,
            format: ExportFormat::csv,
        }
    }
}

/// `isBlank`: `undefined`/`null`/`''`.
fn is_blank(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) => s.is_empty(),
        _ => false,
    }
}

/// `@IsOptional()`: lewati `null`/`undefined` saja.
fn optional(m: &Map<String, Value>, key: &str) -> Option<Value> {
    m.get(key).filter(|v| !v.is_null()).cloned()
}

/// `toArray`: blank → `undefined`; array apa adanya; selain itu `String(v).split(',')`.
fn to_array(v: Option<&Value>) -> Option<Value> {
    if is_blank(v) {
        return None;
    }
    match v {
        Some(Value::Array(a)) => Some(Value::Array(a.clone())),
        Some(other) => Some(Value::Array(
            js_to_string(other)
                .split(',')
                .map(|s| Value::String(s.to_owned()))
                .collect(),
        )),
        None => None,
    }
}

/// `value === 'true' ? true : value === 'false' ? false : value`.
fn to_bool(v: Value) -> Value {
    match v {
        Value::String(s) if s == "true" => Value::Bool(true),
        Value::String(s) if s == "false" => Value::Bool(false),
        other => other,
    }
}

/// `@Matches(DATE_ONLY, { message })`.
fn date_only(errs: &mut Vec<String>, m: &Map<String, Value>, key: &str) -> Option<String> {
    let v = optional(m, key)?;
    match v {
        Value::String(s) if matches_plan_date_pattern(&s) => Some(s),
        _ => {
            errs.push(format!("{key} must be a YYYY-MM-DD date"));
            None
        }
    }
}

/// `@IsEnum(E)` atas satu nilai.
fn one_enum<T: std::str::FromStr>(
    errs: &mut Vec<String>,
    key: &str,
    v: Option<Value>,
    values: &[&str],
) -> Option<T> {
    let v = v?;
    let parsed = v.as_str().and_then(|s| s.parse::<T>().ok());
    if parsed.is_none() {
        errs.push(crate::enums::is_enum_message(key, values));
    }
    parsed
}

/// Validator `each` + `@ArrayMaxSize` atas hasil `toArray` (pesan each lebih dulu).
fn each_array<T>(
    errs: &mut Vec<String>,
    v: Option<Value>,
    each_message: &str,
    max_size: Option<(usize, String)>,
    each: impl Fn(&Value) -> Option<T>,
) -> Option<Vec<T>> {
    let v = v?;
    let before = errs.len();
    let items: Vec<Value> = match &v {
        Value::Array(a) => a.clone(),
        other => vec![other.clone()],
    };
    let parsed: Vec<Option<T>> = items.iter().map(&each).collect();
    if parsed.iter().any(Option::is_none) {
        errs.push(each_message.to_owned());
    }
    // `toArray` selalu menghasilkan array, jadi `@IsArray()` tidak pernah gagal di sini.
    if let Some((max, msg)) = max_size
        && items.len() > max
    {
        errs.push(msg);
    }
    if errs.len() != before {
        return None;
    }
    parsed.into_iter().collect()
}

fn uuid_list(errs: &mut Vec<String>, m: &Map<String, Value>, key: &str) -> Option<Vec<String>> {
    each_array(
        errs,
        to_array(m.get(key)),
        &format!("each value in {key} must be a UUID"),
        None,
        |v| v.as_str().filter(|s| is_uuid(s)).map(str::to_owned),
    )
}

fn enum_list<T: std::str::FromStr>(
    errs: &mut Vec<String>,
    m: &Map<String, Value>,
    key: &str,
    values: &[&str],
) -> Option<Vec<T>> {
    let msg = format!(
        "each value in {key} must be one of the following values: {}",
        values.join(", ")
    );
    each_array(errs, to_array(m.get(key)), &msg, None, |v| {
        v.as_str().and_then(|s| s.parse::<T>().ok())
    })
}

fn boolean(errs: &mut Vec<String>, m: &Map<String, Value>, key: &str) -> Option<bool> {
    let v = m.get(key).cloned().map(to_bool).filter(|v| !v.is_null())?;
    if let Value::Bool(b) = v {
        Some(b)
    } else {
        errs.push(format!("{key} must be a boolean value"));
        None
    }
}

/// `toInt(params, fallback)` lalu `@IsInt @Min(1) [@Max(max)]`.
fn int_param(
    errs: &mut Vec<String>,
    m: &Map<String, Value>,
    key: &str,
    fallback: i64,
    max: Option<i64>,
) -> i64 {
    let Some(v) = m.get(key) else {
        return fallback;
    };
    if is_blank(Some(v)) {
        return fallback;
    }
    #[allow(clippy::cast_precision_loss)]
    let n = parse_int_value(v).map(|n| n as f64);
    let before = errs.len();
    if let Some(mx) = max {
        #[allow(clippy::cast_precision_loss)]
        if n.is_none_or(|x| x > mx as f64) {
            errs.push(format!("{key} must not be greater than {mx}"));
        }
    }
    if n.is_none_or(|x| x < 1.0) {
        errs.push(format!("{key} must not be less than 1"));
    }
    if n.is_none() {
        errs.push(format!("{key} must be an integer number"));
    }
    #[allow(clippy::cast_possible_truncation)]
    n.filter(|_| errs.len() == before)
        .map_or(fallback, |x| x as i64)
}

/// Validasi query. `export = true` → `ExportStoreVisitsQueryDto` (tanpa page/limit, + `format`).
pub fn parse_visit_list_query(
    m: &Map<String, Value>,
    export: bool,
) -> Result<VisitListQuery, Vec<String>> {
    let mut errs = Vec::new();
    let mut q = VisitListQuery {
        start_date: date_only(&mut errs, m, "startDate"),
        end_date: date_only(&mut errs, m, "endDate"),
        ..VisitListQuery::default()
    };
    if let Some(r) = one_enum(
        &mut errs,
        "range",
        optional(m, "range"),
        &enum_values!(StoreVisitDateRange),
    ) {
        q.range = r;
    }
    q.sales_id = uuid_list(&mut errs, m, "salesId");
    q.store_id = uuid_list(&mut errs, m, "storeId");
    q.status = enum_list(&mut errs, m, "status", &enum_values!(StoreVisitStatus));
    q.state = one_enum(
        &mut errs,
        "state",
        optional(m, "state"),
        &enum_values!(StoreVisitState),
    );
    q.gps_validation = enum_list(&mut errs, m, "gpsValidation", &enum_values!(GpsValidation));
    if let Some(v) = optional(m, "search") {
        match v {
            Value::String(s) => q.search = Some(s),
            _ => errs.push("search must be a string".into()),
        }
    }
    q.group_id = each_array(
        &mut errs,
        to_array(m.get("groupId")),
        "each value in groupId must be a UUID",
        Some((50, "groupId must contain no more than 50 elements".into())),
        |v| v.as_str().filter(|s| is_uuid_v4(s)).map(str::to_owned),
    );
    q.unassigned = boolean(&mut errs, m, "unassigned");
    q.planned = boolean(&mut errs, m, "planned");
    if export {
        if let Some(f) = one_enum(
            &mut errs,
            "format",
            optional(m, "format"),
            &enum_values!(ExportFormat),
        ) {
            q.format = f;
        }
    } else {
        q.page = int_param(&mut errs, m, "page", 1, None);
        q.limit = int_param(&mut errs, m, "limit", 20, Some(100));
    }
    if errs.is_empty() { Ok(q) } else { Err(errs) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(v: Value, export: bool) -> Result<VisitListQuery, Vec<String>> {
        parse_visit_list_query(v.as_object().unwrap(), export)
    }

    const U1: &str = "3b7e1a90-22c4-4f6d-9a11-8c2de5f47031";
    const U2: &str = "7c2a1b30-9d44-4e21-8f77-1b2c3d4e5f60";

    #[test]
    fn defaults() {
        let q = parse(json!({}), false).unwrap();
        assert_eq!(q.range, StoreVisitDateRange::last7Days);
        assert_eq!((q.page, q.limit), (1, 20));
        assert_eq!(parse(json!({}), true).unwrap().format, ExportFormat::csv);
        assert_eq!(
            parse(json!({"page": "", "limit": ""}), false)
                .unwrap()
                .limit,
            20
        );
    }

    #[test]
    fn comma_list_and_repeated_param() {
        let q = parse(json!({ "salesId": format!("{U1},{U2}") }), false).unwrap();
        assert_eq!(q.sales_id, Some(vec![U1.to_owned(), U2.to_owned()]));
        let q = parse(
            json!({ "salesId": [U1, U2], "status": "success,closed" }),
            false,
        )
        .unwrap();
        assert_eq!(q.sales_id.unwrap().len(), 2);
        assert_eq!(
            q.status,
            Some(vec![StoreVisitStatus::success, StoreVisitStatus::closed])
        );
        let q = parse(
            json!({ "groupId": format!("{U1},{U2}"), "unassigned": "true", "planned": "false" }),
            false,
        )
        .unwrap();
        assert_eq!(q.group_id.unwrap().len(), 2);
        assert_eq!((q.unassigned, q.planned), (Some(true), Some(false)));
    }

    #[test]
    fn rejections() {
        assert_eq!(
            parse(json!({"status": "success,bogus"}), false).unwrap_err(),
            vec![
                "each value in status must be one of the following values: success, closed, moved, noOwner, rejected, other"
            ]
        );
        assert_eq!(
            parse(json!({"startDate": "2026/08/20"}), false).unwrap_err(),
            vec!["startDate must be a YYYY-MM-DD date"]
        );
        assert_eq!(
            parse(json!({"limit": "101", "page": "0"}), false).unwrap_err(),
            vec![
                "page must not be less than 1",
                "limit must not be greater than 100"
            ]
        );
        assert_eq!(
            parse(json!({"salesId": "nope"}), false).unwrap_err(),
            vec!["each value in salesId must be a UUID"]
        );
        // groupId wajib v4.
        assert_eq!(
            parse(
                json!({"groupId": "3b7e1a90-22c4-1f6d-9a11-8c2de5f47031"}),
                false
            )
            .unwrap_err(),
            vec!["each value in groupId must be a UUID"]
        );
        assert_eq!(
            parse(json!({"planned": "yes"}), false).unwrap_err(),
            vec!["planned must be a boolean value"]
        );
        assert_eq!(
            parse(json!({"format": "pdf"}), true).unwrap_err(),
            vec!["format must be one of the following values: csv, xlsx"]
        );
        assert_eq!(
            parse(json!({"range": "forever", "state": "x"}), false).unwrap_err(),
            vec![
                "range must be one of the following values: today, yesterday, thisWeek, last7Days, thisMonth, lastMonth",
                "state must be one of the following values: active, completed, cancelled"
            ]
        );
        assert_eq!(
            parse(json!({"limit": "abc"}), false).unwrap_err(),
            vec![
                "limit must not be greater than 100",
                "limit must not be less than 1",
                "limit must be an integer number"
            ]
        );
    }

    #[test]
    fn export_ignores_paging() {
        assert!(parse(json!({"page": "0"}), true).is_ok());
    }
}
