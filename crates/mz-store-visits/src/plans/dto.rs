//! DTO jadwal kunjungan (`src/store-visits/dto/*store-visit-plan*.dto.ts`) — validasi manual
//! dengan pesan & urutan `ValidationPipe` Nest, termasuk perataan galat bersarang
//! (`stores.0.storeId must be a UUID`; `@ValidateNested` dengan anak bergalat menjatuhkan galat
//! induknya, persis `mapChildrenToValidationErrors`).

use mz_core::extract::check_int_range;
use mz_core::jsfmt::parse_int;
use mz_core::uuid_rules::is_uuid_v4;
use mz_sales_geo::dto::{
    Field, check_max_string, check_number, js_to_string, parse_float, to_boolean,
};
use serde_json::{Map, Value};

use crate::enums::{StoreVisitPlanComplianceGroupBy, StoreVisitPlanStatus, is_enum_message};
use crate::plan_dates::matches_plan_date_pattern;

// ───────────────────────── pembantu ─────────────────────────

/// `@Matches(PLAN_DATE_PATTERN, { message })` (wajib).
fn check_plan_date(errs: &mut Vec<String>, v: Option<&Value>, message: &str) -> Option<String> {
    match v {
        Some(Value::String(s)) if matches_plan_date_pattern(s) => Some(s.clone()),
        _ => {
            errs.push(message.to_owned());
            None
        }
    }
}

/// `@IsUUID('4')` (wajib, atau `@ValidateIf(v !== undefined)`).
fn check_uuid(errs: &mut Vec<String>, field: &str, v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::String(s)) if is_uuid_v4(s) => Some(s.clone()),
        _ => {
            errs.push(format!("{field} must be a UUID"));
            None
        }
    }
}

/// `toTrimmedNote`: string di-trim, kosong → `null`; selain string apa adanya.
fn trimmed_note(v: &Value) -> Value {
    match v {
        Value::String(s) => {
            let t = mz_core::jsfmt::trim(s);
            if t.is_empty() {
                Value::Null
            } else {
                Value::String(t.to_owned())
            }
        }
        other => other.clone(),
    }
}

/// `@IsOptional() @Transform(toTrimmedNote) @IsString() @MaxLength(255)`.
fn check_note(errs: &mut Vec<String>, field: &str, v: Option<&Value>) -> Field<String> {
    let Some(v) = v.map(trimmed_note) else {
        return Field::Absent;
    };
    if v.is_null() {
        return Field::Null;
    }
    check_max_string(errs, field, Some(&v), 255).map_or(Field::Absent, Field::Val)
}

/// `@IsOptional() @IsInt() @Min(0) @Max(999)` atas angka JSON (pesan Max, Min, IsInt).
fn check_sort_order(errs: &mut Vec<String>, v: Option<&Value>) -> Field<i32> {
    match v {
        None => Field::Absent,
        Some(Value::Null) => Field::Null,
        Some(v) => {
            let n = match v {
                Value::Number(n) => n.as_f64(),
                _ => None,
            };
            check_int_range(errs, "sortOrder", n, 0, 999)
                .and_then(|i| i32::try_from(i).ok())
                .map_or(Field::Absent, Field::Val)
        }
    }
}

/// Elemen `stores[]` (`StoreVisitPlanStoreDto`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PlanStoreItem {
    pub store_id: String,
    pub sort_order: Option<i32>,
    pub note: Option<String>,
    /// `undefined` = tidak dikirim (`null` ditolak validasi).
    pub visit_type_id: Option<String>,
}

fn parse_store_item(m: &Map<String, Value>, errs: &mut Vec<String>) -> PlanStoreItem {
    let store_id = check_uuid(errs, "storeId", m.get("storeId"));
    let sort_order = match check_sort_order(errs, m.get("sortOrder")) {
        Field::Val(v) => Some(v),
        _ => None,
    };
    let note = match check_note(errs, "note", m.get("note")) {
        Field::Val(v) => Some(v),
        _ => None,
    };
    let visit_type_id = match m.get("visitTypeId") {
        None => None,
        v => check_uuid(errs, "visitTypeId", v),
    };
    PlanStoreItem {
        store_id: store_id.unwrap_or_default(),
        sort_order,
        note,
        visit_type_id,
    }
}

/// Validasi `@ValidateNested` atas `stores` (anak bergalat → hanya galat anak, ber-prefix).
/// Mengembalikan `(items, child_errors)`.
fn nested_stores(v: Option<&Value>) -> (Vec<PlanStoreItem>, Vec<String>) {
    let mut child = Vec::new();
    let mut items = Vec::new();
    match v {
        Some(Value::Array(a)) => {
            for (i, el) in a.iter().enumerate() {
                match el {
                    Value::Object(m) => {
                        let mut e = Vec::new();
                        items.push(parse_store_item(m, &mut e));
                        child.extend(e.into_iter().map(|x| format!("stores.{i}.{x}")));
                    }
                    _ => child.push(format!(
                        "stores.nested property {i} must be either object or array"
                    )),
                }
            }
        }
        Some(Value::Object(m)) => {
            let mut e = Vec::new();
            parse_store_item(m, &mut e);
            child.extend(e.into_iter().map(|x| format!("stores.{x}")));
        }
        _ => {}
    }
    (items, child)
}

/// Pesan larik bawaan class-validator.
fn array_unique_msg(field: &str) -> String {
    format!("All {field}'s elements must be unique")
}

/// `stores`: `@IsArray() [@ArrayMinSize(1)] @ArrayMaxSize(200) @ArrayUnique(storeId) @ValidateNested`.
fn check_stores(
    errs: &mut Vec<String>,
    v: Option<&Value>,
    min_one: bool,
) -> Option<Vec<PlanStoreItem>> {
    let (items, child) = nested_stores(v);
    if !child.is_empty() {
        errs.extend(child);
        return None;
    }
    let before = errs.len();
    let arr = v.and_then(Value::as_array);
    let unique = arr.is_some_and(|a| {
        let ids: Vec<Option<&Value>> = a.iter().map(|x| x.get("storeId")).collect();
        ids.iter()
            .enumerate()
            .all(|(i, id)| !ids.iter().take(i).any(|p| p == id))
    });
    if !unique {
        errs.push(array_unique_msg("stores"));
    }
    if arr.is_none_or(|a| a.len() > 200) {
        errs.push("stores must contain no more than 200 elements".into());
    }
    if min_one && arr.is_none_or(Vec::is_empty) {
        errs.push("stores must contain at least 1 elements".into());
    }
    if arr.is_none() {
        errs.push("stores must be an array".into());
    }
    if v.is_some_and(|x| !x.is_array() && !x.is_object()) {
        errs.push("nested property stores must be either object or array".into());
    }
    (errs.len() == before).then_some(items)
}

// ───────────────────────── body ─────────────────────────

/// `CreateStoreVisitPlansRangeDto`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PlanRange {
    pub start_date: Option<String>,
    pub end_date: Option<String>,
    pub weekdays: Option<Vec<i64>>,
}

fn parse_range_obj(m: &Map<String, Value>, errs: &mut Vec<String>) -> PlanRange {
    let start_date = check_plan_date(
        errs,
        m.get("startDate"),
        "range.startDate must be a YYYY-MM-DD date",
    );
    let end_date = check_plan_date(
        errs,
        m.get("endDate"),
        "range.endDate must be a YYYY-MM-DD date",
    );
    let weekdays = match m.get("weekdays") {
        None | Some(Value::Null) => None,
        Some(v) => {
            let arr = v.as_array();
            let elems: Vec<&Value> = arr.map_or_else(|| vec![v], |a| a.iter().collect());
            let num = |x: &Value| x.as_f64();
            if !elems.iter().all(|x| num(x).is_some_and(|n| n <= 6.0)) {
                errs.push("each value in weekdays must not be greater than 6".into());
            }
            if !elems.iter().all(|x| num(x).is_some_and(|n| n >= 0.0)) {
                errs.push("each value in weekdays must not be less than 0".into());
            }
            if !elems
                .iter()
                .all(|x| num(x).is_some_and(|n| n.is_finite() && n.fract() == 0.0))
            {
                errs.push("each value in weekdays must be an integer number".into());
            }
            let unique = arr.is_some_and(|a| {
                a.iter()
                    .enumerate()
                    .all(|(i, x)| !a.iter().take(i).any(|p| p == x))
            });
            if !unique {
                errs.push(array_unique_msg("weekdays"));
            }
            if arr.is_none_or(|a| a.len() > 7) {
                errs.push("weekdays must contain no more than 7 elements".into());
            }
            if arr.is_none_or(Vec::is_empty) {
                errs.push("weekdays must contain at least 1 elements".into());
            }
            if arr.is_none() {
                errs.push("weekdays must be an array".into());
            }
            #[allow(clippy::cast_possible_truncation)]
            arr.map(|a| {
                a.iter()
                    .filter_map(Value::as_f64)
                    .map(|n| n as i64)
                    .collect()
            })
        }
    };
    PlanRange {
        start_date,
        end_date,
        weekdays,
    }
}

/// `CreateStoreVisitPlansDto`.
#[derive(Debug, Clone, PartialEq)]
pub struct CreatePlans {
    pub user_id: String,
    pub dates: Option<Vec<String>>,
    /// `Some` = `range` dikirim bukan `null` (`hasRange`).
    pub range: Option<PlanRange>,
    pub stores: Vec<PlanStoreItem>,
    pub visit_type_id: Option<String>,
    pub skip_existing: bool,
}

pub fn parse_create(m: &Map<String, Value>) -> Result<CreatePlans, Vec<String>> {
    let mut errs = Vec::new();
    let user_id = check_uuid(&mut errs, "userId", m.get("userId"));
    let dates = match m.get("dates") {
        None | Some(Value::Null) => None,
        Some(v) => {
            let arr = v.as_array();
            let elems: Vec<&Value> = arr.map_or_else(|| vec![v], |a| a.iter().collect());
            if !elems
                .iter()
                .all(|x| x.as_str().is_some_and(matches_plan_date_pattern))
            {
                errs.push("dates must contain YYYY-MM-DD dates".into());
            }
            let unique = arr.is_some_and(|a| {
                a.iter()
                    .enumerate()
                    .all(|(i, x)| !a.iter().take(i).any(|p| p == x))
            });
            if !unique {
                errs.push(array_unique_msg("dates"));
            }
            if arr.is_none_or(|a| a.len() > 92) {
                errs.push("dates must contain no more than 92 elements".into());
            }
            if arr.is_none_or(Vec::is_empty) {
                errs.push("dates must contain at least 1 elements".into());
            }
            if arr.is_none() {
                errs.push("dates must be an array".into());
            }
            arr.map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_owned))
                    .collect()
            })
        }
    };
    let range = match m.get("range") {
        None | Some(Value::Null) => None,
        Some(Value::Object(r)) => {
            let mut e = Vec::new();
            let range = parse_range_obj(r, &mut e);
            errs.extend(e.into_iter().map(|x| format!("range.{x}")));
            Some(range)
        }
        Some(Value::Array(a)) => {
            for (i, el) in a.iter().enumerate() {
                match el {
                    Value::Object(r) => {
                        let mut e = Vec::new();
                        parse_range_obj(r, &mut e);
                        errs.extend(e.into_iter().map(|x| format!("range.{i}.{x}")));
                    }
                    _ => errs.push(format!(
                        "range.nested property {i} must be either object or array"
                    )),
                }
            }
            Some(PlanRange::default())
        }
        Some(_) => {
            errs.push("nested property range must be either object or array".into());
            None
        }
    };
    let stores = check_stores(&mut errs, m.get("stores"), true);
    let visit_type_id = match m.get("visitTypeId") {
        None => None,
        v => check_uuid(&mut errs, "visitTypeId", v),
    };
    let skip_existing = match m.get("skipExisting").map(to_boolean) {
        None | Some(Value::Null) => true,
        Some(Value::Bool(b)) => b,
        Some(_) => {
            errs.push("skipExisting must be a boolean value".into());
            true
        }
    };
    match (user_id, stores) {
        (Some(user_id), Some(stores)) if errs.is_empty() => Ok(CreatePlans {
            user_id,
            dates,
            range,
            stores,
            visit_type_id,
            skip_existing,
        }),
        _ => Err(errs),
    }
}

/// `ReplaceStoreVisitPlanDayDto`.
#[derive(Debug, Clone, PartialEq)]
pub struct ReplaceDay {
    pub user_id: String,
    pub date: String,
    pub stores: Vec<PlanStoreItem>,
}

pub fn parse_replace_day(m: &Map<String, Value>) -> Result<ReplaceDay, Vec<String>> {
    let mut errs = Vec::new();
    let user_id = check_uuid(&mut errs, "userId", m.get("userId"));
    let date = check_plan_date(&mut errs, m.get("date"), "date must be a YYYY-MM-DD date");
    let stores = check_stores(&mut errs, m.get("stores"), false);
    match (user_id, date, stores) {
        (Some(user_id), Some(date), Some(stores)) if errs.is_empty() => Ok(ReplaceDay {
            user_id,
            date,
            stores,
        }),
        _ => Err(errs),
    }
}

/// `UpdateStoreVisitPlanDto` (`null` = lepas; tidak dikirim = jangan sentuh).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct UpdatePlan {
    pub sort_order: Field<i32>,
    pub note: Field<String>,
    pub visit_type_id: Option<String>,
}

pub fn parse_update(m: &Map<String, Value>) -> Result<UpdatePlan, Vec<String>> {
    let mut errs = Vec::new();
    let sort_order = check_sort_order(&mut errs, m.get("sortOrder"));
    let note = check_note(&mut errs, "note", m.get("note"));
    let visit_type_id = match m.get("visitTypeId") {
        None => None,
        v => check_uuid(&mut errs, "visitTypeId", v),
    };
    if errs.is_empty() {
        Ok(UpdatePlan {
            sort_order,
            note,
            visit_type_id,
        })
    } else {
        Err(errs)
    }
}

// ───────────────────────── query ─────────────────────────

/// `toQueryArray` + `@IsOptional() @IsArray() @ArrayMaxSize(100) @IsUUID('4', {each})`.
fn check_query_uuids(
    errs: &mut Vec<String>,
    m: &Map<String, Value>,
    field: &str,
) -> Option<Vec<String>> {
    let arr: Vec<String> = match m.get(field) {
        None | Some(Value::Null) => return None,
        Some(Value::String(s)) if s.is_empty() => return None,
        Some(Value::Array(a)) => a.iter().map(js_to_string).collect(),
        Some(v) => js_to_string(v).split(',').map(str::to_owned).collect(),
    };
    let before = errs.len();
    if !arr.iter().all(|s| is_uuid_v4(s)) {
        errs.push(format!("each value in {field} must be a UUID"));
    }
    if arr.len() > 100 {
        errs.push(format!("{field} must contain no more than 100 elements"));
    }
    (errs.len() == before).then_some(arr)
}

fn check_query_date(errs: &mut Vec<String>, m: &Map<String, Value>, field: &str) -> Option<String> {
    check_plan_date(
        errs,
        m.get(field),
        &format!("{field} must be a YYYY-MM-DD date"),
    )
}

/// Filter bersama daftar/kalender/kepatuhan admin.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PlanFilters {
    pub start_date: String,
    pub end_date: String,
    pub sales_id: Option<Vec<String>>,
    pub store_id: Option<Vec<String>>,
    pub group_id: Option<Vec<String>>,
    pub visit_type_id: Option<Vec<String>>,
}

/// `ListStoreVisitPlansQueryDto`.
#[derive(Debug, Clone, PartialEq)]
pub struct ListPlans {
    pub filters: PlanFilters,
    pub status: Option<StoreVisitPlanStatus>,
    pub page: i64,
    pub limit: i64,
}

/// `toInt(params, fallback)`: kosong → fallback, selain itu `parseInt(String(v))` (NaN = `None`).
fn to_int(v: Option<&Value>, fallback: i64) -> Option<i64> {
    match v {
        None | Some(Value::Null) => Some(fallback),
        Some(Value::String(s)) if s.is_empty() => Some(fallback),
        Some(v) => parse_int(&js_to_string(v)),
    }
}

pub fn parse_list(m: &Map<String, Value>) -> Result<ListPlans, Vec<String>> {
    let mut errs = Vec::new();
    let start_date = check_query_date(&mut errs, m, "startDate");
    let end_date = check_query_date(&mut errs, m, "endDate");
    let sales_id = check_query_uuids(&mut errs, m, "salesId");
    let store_id = check_query_uuids(&mut errs, m, "storeId");
    let group_id = check_query_uuids(&mut errs, m, "groupId");
    let visit_type_id = check_query_uuids(&mut errs, m, "visitTypeId");
    let status = match m.get("status") {
        None | Some(Value::Null) => None,
        Some(v) => {
            let p = v
                .as_str()
                .and_then(|s| s.parse::<StoreVisitPlanStatus>().ok());
            if p.is_none() {
                errs.push(is_enum_message(
                    "status",
                    &crate::enum_values!(StoreVisitPlanStatus),
                ));
            }
            p
        }
    };
    let page = to_int(m.get("page"), 1);
    if page.is_none_or(|p| p < 1) {
        errs.push("page must not be less than 1".into());
    }
    if page.is_none() {
        errs.push("page must be an integer number".into());
    }
    #[allow(clippy::cast_precision_loss)]
    let limit = check_int_range(
        &mut errs,
        "limit",
        to_int(m.get("limit"), 20).map(|n| n as f64),
        1,
        100,
    );
    match (start_date, end_date, page, limit) {
        (Some(start_date), Some(end_date), Some(page), Some(limit)) if errs.is_empty() => {
            Ok(ListPlans {
                filters: PlanFilters {
                    start_date,
                    end_date,
                    sales_id,
                    store_id,
                    group_id,
                    visit_type_id,
                },
                status,
                page,
                limit,
            })
        }
        _ => Err(errs),
    }
}

/// `StoreVisitPlansCalendarQueryDto`.
pub fn parse_calendar(m: &Map<String, Value>) -> Result<PlanFilters, Vec<String>> {
    let mut errs = Vec::new();
    let start_date = check_query_date(&mut errs, m, "startDate");
    let end_date = check_query_date(&mut errs, m, "endDate");
    let sales_id = check_query_uuids(&mut errs, m, "salesId");
    let group_id = check_query_uuids(&mut errs, m, "groupId");
    match (start_date, end_date) {
        (Some(start_date), Some(end_date)) if errs.is_empty() => Ok(PlanFilters {
            start_date,
            end_date,
            sales_id,
            group_id,
            ..Default::default()
        }),
        _ => Err(errs),
    }
}

/// `StoreVisitPlansComplianceQueryDto`.
pub fn parse_compliance(
    m: &Map<String, Value>,
) -> Result<(PlanFilters, StoreVisitPlanComplianceGroupBy), Vec<String>> {
    let mut errs = Vec::new();
    let start_date = check_query_date(&mut errs, m, "startDate");
    let end_date = check_query_date(&mut errs, m, "endDate");
    let group_by = match m.get("groupBy") {
        None | Some(Value::Null) => Some(StoreVisitPlanComplianceGroupBy::sales),
        Some(v) => {
            let p = v.as_str().and_then(|s| s.parse().ok());
            if p.is_none() {
                errs.push(is_enum_message(
                    "groupBy",
                    &crate::enum_values!(StoreVisitPlanComplianceGroupBy),
                ));
            }
            p
        }
    };
    let sales_id = check_query_uuids(&mut errs, m, "salesId");
    let group_id = check_query_uuids(&mut errs, m, "groupId");
    match (start_date, end_date, group_by) {
        (Some(start_date), Some(end_date), Some(g)) if errs.is_empty() => Ok((
            PlanFilters {
                start_date,
                end_date,
                sales_id,
                group_id,
                ..Default::default()
            },
            g,
        )),
        _ => Err(errs),
    }
}

/// `ListMobileStoreVisitPlansQueryDto`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ListMobilePlans {
    pub date: Option<String>,
    pub lat: Option<f64>,
    pub lng: Option<f64>,
}

pub fn parse_mobile_list(m: &Map<String, Value>) -> Result<ListMobilePlans, Vec<String>> {
    let mut errs = Vec::new();
    let date = match m.get("date") {
        None | Some(Value::Null) => None,
        v => check_plan_date(&mut errs, v, "date must be a YYYY-MM-DD date"),
    };
    let mut coord = |key: &str, lim: f64| -> Option<f64> {
        let v = m.get(key)?;
        if v.is_null() || v.as_str() == Some("") {
            return None;
        }
        let n = parse_float(&js_to_string(v));
        let num = (!n.is_nan()).then_some(n);
        // `@IsNumber()` default menolak Infinity juga.
        let num = num.filter(|x| x.is_finite());
        check_number(&mut errs, key, num, Some(-lim), Some(lim))
    };
    let lat = coord("lat", 90.0);
    let lng = coord("lng", 180.0);
    if errs.is_empty() {
        Ok(ListMobilePlans { date, lat, lng })
    } else {
        Err(errs)
    }
}

/// `MobileStoreVisitPlansSummaryQueryDto` → `(startDate, endDate)`.
pub fn parse_mobile_summary(m: &Map<String, Value>) -> Result<(String, String), Vec<String>> {
    let mut errs = Vec::new();
    let s = check_query_date(&mut errs, m, "startDate");
    let e = check_query_date(&mut errs, m, "endDate");
    match (s, e) {
        (Some(s), Some(e)) if errs.is_empty() => Ok((s, e)),
        _ => Err(errs),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }
    const U1: &str = "4d1a2b3c-5e6f-4a7b-8c9d-0e1f2a3b4c5d";
    const U2: &str = "7c8d9e0f-1a2b-4c3d-8e5f-6a7b8c9d0e1f";

    /// "note di-trim dan string kosong jadi null".
    #[test]
    fn note_trimmed() {
        let u = parse_update(&obj(json!({"note": "  tagih  "}))).unwrap();
        assert_eq!(u.note, Field::Val("tagih".into()));
        let u = parse_update(&obj(json!({"note": "   ", "sortOrder": null}))).unwrap();
        assert_eq!(u.note, Field::Null);
        assert_eq!(u.sort_order, Field::Null);
        assert_eq!(
            parse_update(&obj(json!({}))).unwrap(),
            UpdatePlan::default()
        );
        assert_eq!(
            parse_update(&obj(json!({"visitTypeId": null}))).unwrap_err(),
            vec!["visitTypeId must be a UUID"]
        );
    }

    /// "menolak storeId ganda dalam satu request".
    #[test]
    fn duplicate_store_ids() {
        let e = parse_create(&obj(json!({
            "userId": U1, "dates": ["2026-08-20"],
            "stores": [{"storeId": U2}, {"storeId": U2}]
        })))
        .unwrap_err();
        assert_eq!(e, vec!["All stores's elements must be unique"]);
    }

    /// "menolak lebih dari 92 tanggal dan tanggal yang bukan YYYY-MM-DD".
    #[test]
    fn dates_limits() {
        let many: Vec<String> = (0..93)
            .map(|i| format!("2026-{:02}-{:02}", i / 28 + 1, i % 28 + 1))
            .collect();
        let e = parse_create(&obj(
            json!({"userId": U1, "dates": many, "stores": [{"storeId": U2}]}),
        ))
        .unwrap_err();
        assert_eq!(e, vec!["dates must contain no more than 92 elements"]);
        let e = parse_create(&obj(
            json!({"userId": U1, "dates": ["20-08-2026"], "stores": [{"storeId": U2}]}),
        ))
        .unwrap_err();
        assert_eq!(e, vec!["dates must contain YYYY-MM-DD dates"]);
    }

    /// "weekdays hanya menerima 0–6 yang unik" (galat anak ber-prefix `range.`).
    #[test]
    fn weekdays_rules() {
        let e = parse_create(&obj(json!({
            "userId": U1, "range": {"startDate": "2026-08-20", "endDate": "2026-08-30", "weekdays": [1, 1, 7]},
            "stores": [{"storeId": U2}]
        })))
        .unwrap_err();
        assert_eq!(
            e,
            vec![
                "range.each value in weekdays must not be greater than 6",
                "range.All weekdays's elements must be unique"
            ]
        );
        let e = parse_create(&obj(json!({
            "userId": U1, "range": {"startDate": "x", "endDate": "2026-08-30"}, "stores": [{"storeId": U2}]
        })))
        .unwrap_err();
        assert_eq!(e, vec!["range.range.startDate must be a YYYY-MM-DD date"]);
    }

    /// "sortOrder dibatasi 0–999" (galat anak `stores.0.`).
    #[test]
    fn sort_order_bounds() {
        let e = parse_create(&obj(json!({
            "userId": U1, "dates": ["2026-08-20"], "stores": [{"storeId": U2, "sortOrder": 1000}]
        })))
        .unwrap_err();
        assert_eq!(e, vec!["stores.0.sortOrder must not be greater than 999"]);
        let e = parse_create(&obj(json!({
            "userId": "x", "dates": ["2026-08-20"], "stores": [{"storeId": "x", "sortOrder": -1}, 5]
        })))
        .unwrap_err();
        assert_eq!(
            e,
            vec![
                "userId must be a UUID",
                "stores.0.storeId must be a UUID",
                "stores.0.sortOrder must not be less than 0",
                "stores.nested property 1 must be either object or array"
            ]
        );
        let ok = parse_create(&obj(
            json!({"userId": U1, "dates": ["2026-08-20"], "stores": [{"storeId": U2}]}),
        ))
        .unwrap();
        assert!(ok.skip_existing);
        assert_eq!(
            parse_create(&obj(
                json!({"userId": U1, "dates": ["2026-08-20"], "stores": []})
            ))
            .unwrap_err(),
            vec!["stores must contain at least 1 elements"]
        );
        // `replaceDay` boleh kosong.
        assert!(
            parse_replace_day(&obj(
                json!({"userId": U1, "date": "2026-08-20", "stores": []})
            ))
            .is_ok()
        );
    }

    /// "rentang wajib pada daftar admin, dan limit dibatasi 100".
    #[test]
    fn list_query() {
        let e = parse_list(&obj(json!({"limit": "101"}))).unwrap_err();
        assert_eq!(
            e,
            vec![
                "startDate must be a YYYY-MM-DD date",
                "endDate must be a YYYY-MM-DD date",
                "limit must not be greater than 100"
            ]
        );
        let q = parse_list(&obj(
            json!({"startDate": "2026-08-01", "endDate": "2026-08-31", "page": ""}),
        ))
        .unwrap();
        assert_eq!((q.page, q.limit), (1, 20));
        assert!(
            parse_list(&obj(
                json!({"startDate": "2026-08-01", "endDate": "2026-08-31", "status": "done"})
            ))
            .is_err()
        );
    }

    /// "salesId menerima satu string dipisah koma maupun param yang diulang".
    #[test]
    fn query_arrays() {
        let q = parse_calendar(&obj(json!({"startDate": "2026-08-01", "endDate": "2026-08-31", "salesId": format!("{U1},{U2}")}))).unwrap();
        assert_eq!(q.sales_id, Some(vec![U1.to_owned(), U2.to_owned()]));
        let q = parse_calendar(&obj(
            json!({"startDate": "2026-08-01", "endDate": "2026-08-31", "salesId": [U1, U2]}),
        ))
        .unwrap();
        assert_eq!(q.sales_id.map(|v| v.len()), Some(2));
        let e = parse_calendar(&obj(
            json!({"startDate": "2026-08-01", "endDate": "2026-08-31", "groupId": "nope"}),
        ))
        .unwrap_err();
        assert_eq!(e, vec!["each value in groupId must be a UUID"]);
    }

    /// "rentang wajib pada /compliance, groupBy default ke sales" + "hanya sales atau region".
    #[test]
    fn compliance_query() {
        let (_, g) = parse_compliance(&obj(
            json!({"startDate": "2026-08-01", "endDate": "2026-08-31"}),
        ))
        .unwrap();
        assert_eq!(g, StoreVisitPlanComplianceGroupBy::sales);
        assert_eq!(
            parse_compliance(&obj(
                json!({"startDate": "2026-08-01", "endDate": "2026-08-31", "groupBy": "store"})
            ))
            .unwrap_err(),
            vec!["groupBy must be one of the following values: sales, region"]
        );
    }

    #[test]
    fn mobile_queries() {
        let q = parse_mobile_list(&obj(json!({"lat": "-7.75", "lng": "110.3"}))).unwrap();
        assert_eq!((q.lat, q.lng), (Some(-7.75), Some(110.3)));
        assert_eq!(
            parse_mobile_list(&obj(json!({"lat": "abc"}))).unwrap_err(),
            vec![
                "lat must not be greater than 90",
                "lat must not be less than -90",
                "lat must be a number conforming to the specified constraints"
            ]
        );
        assert_eq!(
            parse_mobile_list(&obj(json!({"date": "2026-8-1"}))).unwrap_err(),
            vec!["date must be a YYYY-MM-DD date"]
        );
        assert!(parse_mobile_summary(&obj(json!({"startDate": "2026-08-01"}))).is_err());
    }
}
