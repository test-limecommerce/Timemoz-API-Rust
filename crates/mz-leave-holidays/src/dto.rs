//! DTO `leave-holidays` (`ValidationPipe({ transform: true })` global, tanpa whitelist):
//! `CreateLeaveHolidayDto`, `UpdateLeaveHolidayDto`, `HolidayQueryDto`. Pesan & urutan =
//! class-validator 0.14 (properti menurut urutan deklarasi, dekorator dari bawah ke atas).

use mz_admin_jobs::validate::{Errors, is_iso8601_strict, present};
use mz_core::ApiError;
use mz_core::qs::QVal;
use serde_json::Value;

/// `MAX_JOINT_LEAVE_DATES`.
pub const MAX_JOINT_LEAVE_DATES: usize = 10;

const TYPES: &[&str] = &["NATIONAL_HOLIDAY", "JOINT_LEAVE"];

/// `CreateLeaveHolidayDto`.
#[derive(Debug, Clone, Default)]
pub struct CreateDto {
    pub name: String,
    pub date: String,
    pub ty: Option<String>,
    pub deducts_leave: Option<bool>,
    pub joint_leave_dates: Option<Vec<String>>,
    pub joint_leave_deducts_leave: Option<bool>,
}

/// `UpdateLeaveHolidayDto` (semua opsional).
#[derive(Debug, Clone, Default)]
pub struct UpdateDto {
    pub name: Option<String>,
    pub date: Option<String>,
    pub ty: Option<String>,
    pub deducts_leave: Option<bool>,
    pub joint_leave_dates: Option<Vec<String>>,
    pub joint_leave_deducts_leave: Option<bool>,
}

fn strict_date(v: &Value) -> bool {
    v.as_str().is_some_and(is_iso8601_strict)
}

/// `@IsDateString({ strict: true })`.
fn date_string(e: &mut Errors, field: &str, v: Option<&Value>) {
    if !v.is_some_and(strict_date) {
        e.push(format!("{field} must be a valid ISO 8601 date string"));
    }
}

/// `@ArrayUnique()` memakai `indexOf` (`===`): hanya nilai primitif yang bisa kembar.
fn has_duplicates(items: &[Value]) -> bool {
    items.iter().enumerate().any(|(i, a)| {
        !matches!(a, Value::Array(_) | Value::Object(_)) && items.iter().take(i).any(|b| b == a)
    })
}

/// `jointLeaveDates`: IsOptional, `@IsDateString({strict}, {each})`, `@ArrayUnique`,
/// `@ArrayMaxSize(10)`, `@IsArray` (urutan pesan dari bawah ke atas). `each` pada nilai
/// non-array memvalidasi nilai itu sendiri (class-validator).
fn joint_dates(e: &mut Errors, v: Option<&Value>) -> Option<Vec<String>> {
    let v = v.filter(|v| present(Some(v)))?;
    let field = "jointLeaveDates";
    let each_ok = match v {
        Value::Array(items) => items.iter().all(strict_date),
        other => strict_date(other),
    };
    if !each_ok {
        e.push(format!(
            "each value in {field} must be a valid ISO 8601 date string"
        ));
    }
    let items = v.as_array();
    if items.is_none_or(|i| has_duplicates(i)) {
        e.push(format!("All {field}'s elements must be unique"));
    }
    if items.is_none_or(|i| i.len() > MAX_JOINT_LEAVE_DATES) {
        e.push(format!(
            "{field} must contain no more than {MAX_JOINT_LEAVE_DATES} elements"
        ));
    }
    if items.is_none() {
        e.push(format!("{field} must be an array"));
    }
    items.map(|i| {
        i.iter()
            .filter_map(|x| x.as_str().map(str::to_owned))
            .collect()
    })
}

fn opt_string(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str).map(str::to_owned)
}

fn opt_bool(v: Option<&Value>) -> Option<bool> {
    v.and_then(Value::as_bool)
}

/// Properti opsional dengan `@IsOptional`: `null`/`undefined` dianggap tidak dikirim.
fn get<'a>(body: &'a Value, key: &str) -> Option<&'a Value> {
    body.get(key).filter(|v| present(Some(v)))
}

/// Validasi `name`/`date`/`type`/`deductsLeave`/`jointLeaveDates`/`jointLeaveDeductsLeave`;
/// `required` = `name` & `date` wajib (Create).
fn validate_common(body: &Value, required: bool) -> Result<UpdateDto, ApiError> {
    let mut e = Errors::default();
    let name = if required {
        body.get("name")
    } else {
        get(body, "name")
    };
    if required || name.is_some() {
        e.max_length("name", name, 200);
        e.string("name", name);
    }
    let date = if required {
        body.get("date")
    } else {
        get(body, "date")
    };
    if required || date.is_some() {
        date_string(&mut e, "date", date);
    }
    let ty = get(body, "type");
    if ty.is_some() {
        e.one_of("type", ty, TYPES);
    }
    let deducts = get(body, "deductsLeave");
    if deducts.is_some() {
        e.boolean("deductsLeave", deducts);
    }
    let dates = joint_dates(&mut e, body.get("jointLeaveDates"));
    let joint_deducts = get(body, "jointLeaveDeductsLeave");
    if joint_deducts.is_some() {
        e.boolean("jointLeaveDeductsLeave", joint_deducts);
    }
    e.into_result()?;
    Ok(UpdateDto {
        name: opt_string(name),
        date: opt_string(date),
        ty: opt_string(ty),
        deducts_leave: opt_bool(deducts),
        joint_leave_dates: dates,
        joint_leave_deducts_leave: opt_bool(joint_deducts),
    })
}

/// Body JSON array → `{}` (ValidationPipe memeriksa properti DTO pada array = semuanya
/// `undefined`).
fn object_body(body: &Value) -> Value {
    if body.is_object() {
        body.clone()
    } else {
        Value::Object(serde_json::Map::new())
    }
}

pub fn validate_create(body: &Value) -> Result<CreateDto, ApiError> {
    let d = validate_common(&object_body(body), true)?;
    Ok(CreateDto {
        name: d.name.unwrap_or_default(),
        date: d.date.unwrap_or_default(),
        ty: d.ty,
        deducts_leave: d.deducts_leave,
        joint_leave_dates: d.joint_leave_dates,
        joint_leave_deducts_leave: d.joint_leave_deducts_leave,
    })
}

pub fn validate_update(body: &Value) -> Result<UpdateDto, ApiError> {
    validate_common(&object_body(body), false)
}

/// `HolidayQueryDto`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListQuery {
    pub year: Option<i32>,
    pub ty: Option<String>,
    pub is_active: Option<bool>,
    pub deducts_leave: Option<bool>,
}

/// `@Type(() => Boolean)`: `Boolean(value)` — hanya string kosong yang false.
fn boolean_of(v: &QVal) -> bool {
    match v {
        QVal::One(s) => !s.is_empty(),
        QVal::Many(_) | QVal::Obj => true,
    }
}

pub fn validate_query(q: &[(String, QVal)]) -> Result<ListQuery, ApiError> {
    let mut e = Errors::default();
    let mut out = ListQuery::default();

    if let Some(v) = mz_core::qs::get(q, "year") {
        // `@Type(() => Number)`: array/objek bukan bilangan.
        let num = match v {
            QVal::One(s) => Some(mz_core::jsfmt::number(s)),
            _ => None,
        };
        out.year = mz_core::extract::check_int_range(&mut e.0, "year", num, 2000, 2100)
            .and_then(|y| i32::try_from(y).ok());
    }
    if let Some(v) = mz_core::qs::get(q, "type") {
        match v {
            QVal::One(s) if TYPES.contains(&s.as_str()) => out.ty = Some(s.clone()),
            _ => e.push(format!(
                "type must be one of the following values: {}",
                TYPES.join(", ")
            )),
        }
    }
    if let Some(v) = mz_core::qs::get(q, "isActive") {
        out.is_active = Some(boolean_of(v));
    }
    if let Some(v) = mz_core::qs::get(q, "deductsLeave") {
        match v {
            QVal::One(s) if s == "true" => out.deducts_leave = Some(true),
            QVal::One(s) if s == "false" => out.deducts_leave = Some(false),
            _ => e.push("deductsLeave must be a boolean value"),
        }
    }
    e.into_result()?;
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use serde_json::json;

    fn errs<T>(r: Result<T, ApiError>) -> Vec<String> {
        match r {
            Err(ApiError::Validation(v)) => v,
            Err(other) => vec![format!("OTHER {other}")],
            Ok(_) => Vec::new(),
        }
    }

    #[test]
    fn create_valid_dan_wajib() {
        let d = validate_create(&json!({
            "name": "Idul Fitri", "date": "2026-03-20", "type": "NATIONAL_HOLIDAY",
            "deductsLeave": false, "jointLeaveDates": ["2026-03-23"], "jointLeaveDeductsLeave": true,
            "ignored": 1
        }))
        .unwrap();
        assert_eq!(d.name, "Idul Fitri");
        assert_eq!(
            d.joint_leave_dates.as_deref(),
            Some(&["2026-03-23".to_owned()][..])
        );
        assert_eq!(
            errs(validate_create(&json!({}))),
            [
                "name must be shorter than or equal to 200 characters",
                "name must be a string",
                "date must be a valid ISO 8601 date string",
            ]
        );
        // Array = `{}`.
        assert_eq!(errs(validate_create(&json!(["x"]))).len(), 3);
    }

    #[test]
    fn create_pesan_per_properti() {
        assert_eq!(
            errs(validate_create(&json!({
                "name": "x".repeat(201), "date": "2026-02-30", "type": "X", "deductsLeave": "ya",
                "jointLeaveDeductsLeave": 1
            }))),
            [
                "name must be shorter than or equal to 200 characters",
                "date must be a valid ISO 8601 date string",
                "type must be one of the following values: NATIONAL_HOLIDAY, JOINT_LEAVE",
                "deductsLeave must be a boolean value",
                "jointLeaveDeductsLeave must be a boolean value",
            ]
        );
    }

    #[test]
    fn joint_leave_dates_aturan() {
        let base = |v: Value| json!({"name": "a", "date": "2026-03-20", "jointLeaveDates": v});
        assert_eq!(
            errs(validate_create(&base(json!(["2026-03-21", "2026-03-21"])))),
            ["All jointLeaveDates's elements must be unique"]
        );
        assert_eq!(
            errs(validate_create(&base(json!(["x"])))),
            ["each value in jointLeaveDates must be a valid ISO 8601 date string"]
        );
        let eleven: Vec<String> = (1..=11).map(|d| format!("2026-04-{d:02}")).collect();
        assert_eq!(
            errs(validate_create(&base(json!(eleven)))),
            ["jointLeaveDates must contain no more than 10 elements"]
        );
        // String tunggal: each lolos, tiga lainnya gagal.
        assert_eq!(
            errs(validate_create(&base(json!("2026-03-21")))),
            [
                "All jointLeaveDates's elements must be unique",
                "jointLeaveDates must contain no more than 10 elements",
                "jointLeaveDates must be an array",
            ]
        );
        assert!(errs(validate_create(&base(json!([])))).is_empty());
        assert!(errs(validate_create(&base(Value::Null))).is_empty());
    }

    #[test]
    fn update_semua_opsional() {
        assert!(errs(validate_update(&json!({}))).is_empty());
        assert!(errs(validate_update(&json!({"name": null, "date": null}))).is_empty());
        assert_eq!(
            errs(validate_update(&json!({"date": "bad", "name": 5}))),
            [
                "name must be shorter than or equal to 200 characters",
                "name must be a string",
                "date must be a valid ISO 8601 date string",
            ]
        );
    }

    #[test]
    fn query_aturan() {
        let q = |s: &str| validate_query(&mz_core::qs::parse(s));
        assert_eq!(
            q("year=2026&type=JOINT_LEAVE&isActive=false&deductsLeave=true").unwrap(),
            ListQuery {
                year: Some(2026),
                ty: Some("JOINT_LEAVE".into()),
                // `@Type(() => Boolean)`: "false" → true.
                is_active: Some(true),
                deducts_leave: Some(true),
            }
        );
        assert_eq!(q("isActive=").unwrap().is_active, Some(false));
        assert_eq!(errs(q("year=1999")), ["year must not be less than 2000"]);
        assert_eq!(
            errs(q("year=abc&type=x&deductsLeave=ya")),
            [
                "year must not be greater than 2100",
                "year must not be less than 2000",
                "year must be an integer number",
                "type must be one of the following values: NATIONAL_HOLIDAY, JOINT_LEAVE",
                "deductsLeave must be a boolean value",
            ]
        );
        assert_eq!(errs(q("year=2026.5")), ["year must be an integer number"]);
        assert!(q("").unwrap() == ListQuery::default());
    }
}
