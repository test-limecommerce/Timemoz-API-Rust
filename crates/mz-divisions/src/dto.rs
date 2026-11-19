//! DTO `src/divisions/dto/*` dan `ValidationPipe({ transform: true })`. Body/query diterima
//! sebagai JSON mentah lalu divalidasi manual supaya pesan class-validator (urutan dekorator
//! dari bawah ke atas, awalan `members.0.` untuk nested) sama dengan Node.

use chrono::NaiveDateTime;
use mz_core::extract::{Validate, check_int_range, validator_js_length};
use mz_core::iso8601::{DateArg, parse_date_arg};
use mz_core::jsfmt::{number as js_number, trim as js_trim};
use mz_core::uuid_rules::is_uuid_v4;
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::consts::*;

const MS_PER_DAY: i64 = 24 * 60 * 60 * 1000;

/// `DivisionMemberRole`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberRole {
    Leader,
    Member,
}

impl MemberRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Leader => "LEADER",
            Self::Member => "MEMBER",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "LEADER" => Some(Self::Leader),
            "MEMBER" => Some(Self::Member),
            _ => None,
        }
    }
}

/// `DivisionMemberDto`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberInput {
    pub user_uuid: String,
    pub role: MemberRole,
}

/// Nilai properti DTO: tidak dikirim / `null` / bernilai.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Field<T> {
    Absent,
    Null,
    Val(T),
}

/// Daftar anggota. `Crash` = `null` atau elemen `null`/bukan objek yang lolos validasi Node
/// (`@IsOptional` / `@ValidateNested` melewatkannya) lalu `TypeError` di service → 500.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Members {
    Absent,
    Crash,
    List(Vec<MemberInput>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateInput {
    pub name: String,
    pub description: Option<String>,
    pub members: Members,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateInput {
    pub name: Field<String>,
    pub description: Field<String>,
    pub members: Members,
    pub version: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleInput {
    pub role: MemberRole,
    pub version: i64,
}

// ───────────────────────── helper validator ─────────────────────────

fn enum_msg(field: &str) -> String {
    format!("{field} must be one of the following values: LEADER, MEMBER")
}

use mz_core::extract::length_message;

fn max_len_message(field: &str, max: usize) -> String {
    format!("{field} must be shorter than or equal to {max} characters")
}

/// `@Transform(trim)` atas nilai mentah.
fn trimmed(v: Option<&Value>) -> Option<Value> {
    v.map(|v| match v {
        Value::String(s) => Value::String(js_trim(s).to_owned()),
        other => other.clone(),
    })
}

/// `name` wajib (`@IsString @Transform(trim) @Length(1, 60)`; pesan Length dulu, lalu IsString).
fn name_required(m: &Map<String, Value>, errs: &mut Vec<String>) -> String {
    let v = trimmed(m.get("name"));
    let text = match &v {
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    };
    let ok_len = text
        .as_deref()
        .is_some_and(|s| (1..=DIVISION_NAME_MAX).contains(&validator_js_length(s)));
    if !ok_len {
        errs.push(length_message("name", v.as_ref(), 1, DIVISION_NAME_MAX));
    }
    if text.is_none() {
        errs.push("name must be a string".to_owned());
    }
    text.unwrap_or_default()
}

/// `name` opsional (`@IsOptional`: `undefined`/`null` melewati validasi).
fn name_optional(m: &Map<String, Value>, errs: &mut Vec<String>) -> Field<String> {
    match m.get("name") {
        None => Field::Absent,
        Some(Value::Null) => Field::Null,
        Some(_) => Field::Val(name_required(m, errs)),
    }
}

/// `description`: `@IsOptional @Transform(trim || null) @IsString @MaxLength(200)`.
fn description_field(m: &Map<String, Value>, errs: &mut Vec<String>) -> Field<String> {
    match m.get("description") {
        None => Field::Absent,
        Some(Value::Null) => Field::Null,
        Some(Value::String(s)) => {
            let t = js_trim(s);
            if t.is_empty() {
                return Field::Null;
            }
            if validator_js_length(t) > DIVISION_DESCRIPTION_MAX {
                errs.push(max_len_message("description", DIVISION_DESCRIPTION_MAX));
            }
            Field::Val(t.to_owned())
        }
        Some(_) => {
            errs.push(max_len_message("description", DIVISION_DESCRIPTION_MAX));
            errs.push("description must be a string".to_owned());
            Field::Absent
        }
    }
}

/// `members`: `@IsArray @ArrayMaxSize(1000) @ValidateNested({ each }) @Type(DivisionMemberDto)`.
/// `required` = tanpa `@IsOptional`. Nest membuang constraint induk bila ada galat nested.
fn members_field(m: &Map<String, Value>, required: bool, errs: &mut Vec<String>) -> Members {
    let v = m.get("members");
    if !required && matches!(v, None | Some(Value::Null)) {
        return if v.is_some() {
            Members::Crash
        } else {
            Members::Absent
        };
    }
    let own = |errs: &mut Vec<String>, tail: Option<&str>| {
        errs.push(format!(
            "members must contain no more than {DIVISION_MEMBERS_MAX} elements"
        ));
        errs.push("members must be an array".to_owned());
        if let Some(t) = tail {
            errs.push(t.to_owned());
        }
    };
    let Some(Value::Array(items)) = v else {
        match v {
            Some(Value::String(_) | Value::Number(_) | Value::Bool(_)) => own(
                errs,
                Some("nested property members must be either object or array"),
            ),
            _ => own(errs, None),
        }
        return Members::Absent;
    };

    let mut nested: Vec<String> = Vec::new();
    let mut out = Vec::new();
    let mut crash = false;
    for (i, item) in items.iter().enumerate() {
        match item {
            Value::Object(o) => {
                let uuid = o
                    .get("userUuid")
                    .and_then(Value::as_str)
                    .filter(|s| is_uuid_v4(s));
                if uuid.is_none() {
                    nested.push(format!("members.{i}.userUuid must be a UUID"));
                }
                let role = o
                    .get("role")
                    .and_then(Value::as_str)
                    .and_then(MemberRole::parse);
                if role.is_none() {
                    nested.push(format!("members.{i}.{}", enum_msg("role")));
                }
                if let (Some(u), Some(r)) = (uuid, role) {
                    out.push(MemberInput {
                        user_uuid: u.to_owned(),
                        role: r,
                    });
                }
            }
            Value::Null | Value::Array(_) => crash = true,
            _ => nested.push(format!(
                "members.{i}.nested property members must be either object or array"
            )),
        }
    }
    if !nested.is_empty() {
        errs.extend(nested);
    } else if items.len() > DIVISION_MEMBERS_MAX {
        errs.push(format!(
            "members must contain no more than {DIVISION_MEMBERS_MAX} elements"
        ));
    }
    if crash {
        Members::Crash
    } else {
        Members::List(out)
    }
}

/// `confirmMoveUserUuids`: `@IsOptional @IsArray @ArrayMaxSize(1000) @IsUUID('4', { each })`.
fn confirm_move_field(m: &Map<String, Value>, errs: &mut Vec<String>) {
    let Some(v) = m.get("confirmMoveUserUuids").filter(|v| !v.is_null()) else {
        return;
    };
    let each = "each value in confirmMoveUserUuids must be a UUID";
    let max =
        format!("confirmMoveUserUuids must contain no more than {DIVISION_MEMBERS_MAX} elements");
    match v {
        Value::Array(a) => {
            if a.iter().any(|x| !x.as_str().is_some_and(is_uuid_v4)) {
                errs.push(each.to_owned());
            }
            if a.len() > DIVISION_MEMBERS_MAX {
                errs.push(max);
            }
        }
        _ => {
            errs.push(each.to_owned());
            errs.push(max);
            errs.push("confirmMoveUserUuids must be an array".to_owned());
        }
    }
}

/// `@IsInt() @Min(min)` atas angka JSON (pesan Min dulu). Mengembalikan nilai bila lolos.
fn json_int_min(errs: &mut Vec<String>, field: &str, n: Option<f64>, min: i64) -> Option<i64> {
    let before = errs.len();
    #[allow(clippy::cast_precision_loss)]
    let minf = min as f64;
    if !n.is_some_and(|n| n >= minf) {
        errs.push(format!("{field} must not be less than {min}"));
    }
    let int = n.filter(|n| n.is_finite() && n.fract() == 0.0);
    if int.is_none() {
        errs.push(format!("{field} must be an integer number"));
    }
    #[allow(clippy::cast_possible_truncation)]
    int.filter(|_| errs.len() == before).map(|n| n as i64)
}

fn obj(v: &Value) -> Map<String, Value> {
    v.as_object().cloned().unwrap_or_default()
}

fn role_field(m: &Map<String, Value>, errs: &mut Vec<String>) -> Option<MemberRole> {
    let r = m
        .get("role")
        .and_then(Value::as_str)
        .and_then(MemberRole::parse);
    if r.is_none() {
        errs.push(enum_msg("role"));
    }
    r
}

// ───────────────────────── body ─────────────────────────

/// `CreateDivisionDto`.
pub fn parse_create(v: &Value) -> Result<CreateInput, Vec<String>> {
    let m = obj(v);
    let mut errs = Vec::new();
    let name = name_required(&m, &mut errs);
    let description = description_field(&m, &mut errs);
    let members = members_field(&m, true, &mut errs);
    confirm_move_field(&m, &mut errs);
    if !errs.is_empty() {
        return Err(errs);
    }
    Ok(CreateInput {
        name,
        description: match description {
            Field::Val(s) => Some(s),
            _ => None,
        },
        members,
    })
}

/// `UpdateDivisionDto`.
pub fn parse_update(v: &Value) -> Result<UpdateInput, Vec<String>> {
    let m = obj(v);
    let mut errs = Vec::new();
    let name = name_optional(&m, &mut errs);
    let description = description_field(&m, &mut errs);
    let members = members_field(&m, false, &mut errs);
    confirm_move_field(&m, &mut errs);
    let version = json_int_min(
        &mut errs,
        "version",
        m.get("version").and_then(Value::as_f64),
        0,
    );
    if !errs.is_empty() {
        return Err(errs);
    }
    Ok(UpdateInput {
        name,
        description,
        members,
        version: version.unwrap_or_default(),
    })
}

/// `UpdateMemberRoleDto`.
pub fn parse_role(v: &Value) -> Result<RoleInput, Vec<String>> {
    let m = obj(v);
    let mut errs = Vec::new();
    let role = role_field(&m, &mut errs);
    let version = json_int_min(
        &mut errs,
        "version",
        m.get("version").and_then(Value::as_f64),
        0,
    );
    match (role, version) {
        (Some(role), Some(version)) if errs.is_empty() => Ok(RoleInput { role, version }),
        _ => Err(errs),
    }
}

macro_rules! raw_body {
    ($name:ident, $parse:ident) => {
        #[derive(Debug, Clone, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Value);

        impl Validate for $name {
            fn validate(&self) -> Vec<String> {
                $parse(&self.0).err().unwrap_or_default()
            }
        }
    };
}

raw_body!(CreateBody, parse_create);
raw_body!(UpdateBody, parse_update);
raw_body!(RoleBody, parse_role);

// ───────────────────────── query ─────────────────────────

/// `@Type(() => Number)`: string tunggal → `Number(s)`; selain itu bukan angka.
fn query_number(v: Option<&Value>) -> Option<f64> {
    match v {
        Some(Value::String(s)) => Some(js_number(s)),
        _ => None,
    }
}

/// `search`: `@IsOptional @IsString @Transform(trim) @MaxLength(60)` (pesan MaxLength dulu).
fn search_field(m: &Map<String, Value>, errs: &mut Vec<String>) -> Option<String> {
    match m.get("search") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => {
            let t = js_trim(s);
            if validator_js_length(t) > DIVISION_SEARCH_MAX {
                errs.push(max_len_message("search", DIVISION_SEARCH_MAX));
            }
            Some(t.to_owned())
        }
        Some(_) => {
            errs.push(max_len_message("search", DIVISION_SEARCH_MAX));
            errs.push("search must be a string".to_owned());
            None
        }
    }
}

/// `ListDivisionsQueryDto`: `search` kosong sesudah trim = tanpa filter.
pub fn parse_list_query(m: &Map<String, Value>) -> Result<Option<String>, Vec<String>> {
    let mut errs = Vec::new();
    let search = search_field(m, &mut errs).filter(|s| !s.is_empty());
    if errs.is_empty() {
        Ok(search)
    } else {
        Err(errs)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmployeesQuery {
    pub search: Option<String>,
    pub page_number: i64,
    pub items_per_page: i64,
}

/// `ListDivisionEmployeesQueryDto`.
pub fn parse_employees_query(m: &Map<String, Value>) -> Result<EmployeesQuery, Vec<String>> {
    let mut errs = Vec::new();
    let search = search_field(m, &mut errs).filter(|s| !s.is_empty());
    let page = match m.get("pageNumber") {
        None => Some(1),
        v => json_int_min(&mut errs, "pageNumber", query_number(v), 1),
    };
    let per = match m.get("itemsPerPage") {
        None => Some(DIVISION_EMPLOYEES_PAGE_MAX),
        v => check_int_range(
            &mut errs,
            "itemsPerPage",
            query_number(v),
            1,
            DIVISION_EMPLOYEES_PAGE_MAX,
        ),
    };
    match (page, per) {
        (Some(page_number), Some(items_per_page)) if errs.is_empty() => Ok(EmployeesQuery {
            search,
            page_number,
            items_per_page,
        }),
        _ => Err(errs),
    }
}

/// `VersionQueryDto` (`?version=`): `@Type(() => Number) @IsInt @Min(0)`.
pub fn parse_version_query(m: &Map<String, Value>) -> Result<i64, Vec<String>> {
    let mut errs = Vec::new();
    let v = json_int_min(&mut errs, "version", query_number(m.get("version")), 0);
    match v {
        Some(v) if errs.is_empty() => Ok(v),
        _ => Err(errs),
    }
}

/// `new Date(x).getTime()` untuk nilai query; `None` = Invalid Date.
fn date_ms(v: Option<&Value>) -> Option<i64> {
    match v {
        Some(Value::String(s)) => match parse_date_arg(s) {
            Some(DateArg::Ok(d)) => Some(d.and_utc().timestamp_millis()),
            _ => None,
        },
        _ => None,
    }
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

/// Port `divisionReportRangeError` (`dto/division-report-query.dto.ts`).
pub fn report_range_error(start: Option<&Value>, end: Option<&Value>) -> Option<String> {
    let (has_from, has_to) = (truthy(start), truthy(end));
    if !has_from && !has_to {
        return None;
    }
    if !has_from || !has_to {
        return Some("startAt and endAt must be given together".to_owned());
    }
    let (Some(from), Some(to)) = (date_ms(start), date_ms(end)) else {
        return Some("startAt and endAt must be valid dates".to_owned());
    };
    if to < from {
        return Some("endAt must be on or after startAt".to_owned());
    }
    let days = (to - from).div_euclid(MS_PER_DAY);
    if days > DIVISION_REPORT_MAX_DAYS {
        return Some(format!(
            "Date range cannot exceed {DIVISION_REPORT_MAX_DAYS} days"
        ));
    }
    None
}

fn is_date_string(v: &Value) -> bool {
    v.as_str().is_some_and(|s| parse_date_arg(s).is_some())
}

/// Rentang laporan yang sudah tervalidasi (kedua ujung terisi), atau `None` = bawaan.
pub type ReportRange = Option<(NaiveDateTime, NaiveDateTime)>;

/// `DivisionReportQueryDto`.
pub fn parse_report_query(m: &Map<String, Value>) -> Result<ReportRange, Vec<String>> {
    let (start, end) = (m.get("startAt"), m.get("endAt"));
    let mut errs = Vec::new();
    // startAt: @IsOptional @IsDateString.
    if let Some(s) = start.filter(|v| !v.is_null())
        && !is_date_string(s)
    {
        errs.push("startAt must be a valid ISO 8601 date string".to_owned());
    }
    // endAt: @ValidateIf(startAt/endAt terdefinisi) @IsDateString @Validate(range) → range dulu.
    if start.is_some() || end.is_some() {
        if let Some(msg) = report_range_error(start, end) {
            errs.push(msg);
        }
        if !end.is_some_and(is_date_string) {
            errs.push("endAt must be a valid ISO 8601 date string".to_owned());
        }
    }
    if !errs.is_empty() {
        return Err(errs);
    }
    let date = |v: Option<&Value>| match v.and_then(Value::as_str).map(parse_date_arg) {
        Some(Some(DateArg::Ok(d))) => Some(d),
        _ => None,
    };
    Ok(match (truthy(start), truthy(end), date(start), date(end)) {
        (true, true, Some(a), Some(b)) => Some((a, b)),
        _ => None,
    })
}

macro_rules! raw_query {
    ($name:ident, $parse:ident) => {
        #[derive(Debug, Clone, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Map<String, Value>);

        impl Validate for $name {
            fn validate(&self) -> Vec<String> {
                $parse(&self.0).err().unwrap_or_default()
            }
        }
    };
}

raw_query!(ListQuery, parse_list_query);
raw_query!(EmployeesQueryRaw, parse_employees_query);
raw_query!(VersionQuery, parse_version_query);
raw_query!(ReportQuery, parse_report_query);

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const U1: &str = "3f2504e0-4f89-41d3-9a0c-0305e82c3301";

    fn errs(r: Result<impl std::fmt::Debug, Vec<String>>) -> Vec<String> {
        r.err().unwrap_or_default()
    }

    #[test]
    fn create_valid_trims_and_nulls_blank_description() {
        let r = parse_create(&json!({
            "name": "  Eng  ", "description": "   ",
            "members": [{ "userUuid": U1, "role": "LEADER" }],
        }));
        let Ok(c) = r else {
            return assert!(r.is_ok(), "{r:?}");
        };
        assert_eq!(c.name, "Eng");
        assert_eq!(c.description, None);
        assert_eq!(
            c.members,
            Members::List(vec![MemberInput {
                user_uuid: U1.into(),
                role: MemberRole::Leader
            }])
        );
    }

    #[test]
    fn create_messages_follow_decorator_order() {
        // Length dulu, lalu IsString; members: ArrayMaxSize, IsArray.
        assert_eq!(
            errs(parse_create(&json!({}))),
            vec![
                "name must be longer than or equal to 1 characters",
                "name must be a string",
                "members must contain no more than 1000 elements",
                "members must be an array",
            ]
        );
        assert_eq!(
            errs(parse_create(&json!({ "name": "   ", "members": [] }))),
            vec!["name must be longer than or equal to 1 characters"]
        );
        let long = "x".repeat(61);
        assert_eq!(
            errs(parse_create(&json!({ "name": long, "members": [] }))),
            vec!["name must be shorter than or equal to 60 characters"]
        );
        assert_eq!(
            errs(parse_create(&json!({ "name": 5, "members": [] }))),
            vec![
                "name must be longer than or equal to 1 and shorter than or equal to 60 characters",
                "name must be a string"
            ]
        );
    }

    #[test]
    fn description_rules() {
        let m = json!({ "name": "a", "members": [], "description": 5 });
        assert_eq!(
            errs(parse_create(&m)),
            vec![
                "description must be shorter than or equal to 200 characters",
                "description must be a string"
            ]
        );
        let long = "y".repeat(201);
        assert_eq!(
            errs(parse_create(
                &json!({ "name": "a", "members": [], "description": long })
            )),
            vec!["description must be shorter than or equal to 200 characters"]
        );
    }

    #[test]
    fn nested_member_errors_replace_parent_constraints() {
        let r = parse_create(&json!({
            "name": "a",
            "members": [{ "userUuid": "x", "role": "BOSS" }, { "userUuid": U1, "role": "MEMBER" }, "s"],
        }));
        assert_eq!(
            errs(r),
            vec![
                "members.0.userUuid must be a UUID",
                "members.0.role must be one of the following values: LEADER, MEMBER",
                "members.2.nested property members must be either object or array",
            ]
        );
    }

    #[test]
    fn uuid_must_be_v4() {
        // versi 1 ditolak `@IsUUID('4')`.
        let v1 = "3f2504e0-4f89-11d3-9a0c-0305e82c3301";
        assert_eq!(
            errs(parse_create(&json!({
                "name": "a", "members": [{ "userUuid": v1, "role": "LEADER" }]
            }))),
            vec!["members.0.userUuid must be a UUID"]
        );
    }

    #[test]
    fn confirm_move_is_validated_but_ignored() {
        let base = |c: Value| json!({ "name": "a", "members": [], "confirmMoveUserUuids": c });
        assert!(parse_create(&base(json!([U1]))).is_ok());
        assert!(parse_create(&base(Value::Null)).is_ok());
        assert_eq!(
            errs(parse_create(&base(json!(["x"])))),
            vec!["each value in confirmMoveUserUuids must be a UUID"]
        );
        assert_eq!(
            errs(parse_create(&base(json!("x")))),
            vec![
                "each value in confirmMoveUserUuids must be a UUID",
                "confirmMoveUserUuids must contain no more than 1000 elements",
                "confirmMoveUserUuids must be an array",
            ]
        );
    }

    #[test]
    fn update_optional_fields_and_version() {
        let ok = parse_update(&json!({ "version": 3 }));
        assert_eq!(
            ok.ok(),
            Some(UpdateInput {
                name: Field::Absent,
                description: Field::Absent,
                members: Members::Absent,
                version: 3
            })
        );
        assert_eq!(
            errs(parse_update(&json!({}))),
            vec![
                "version must not be less than 0",
                "version must be an integer number"
            ]
        );
        assert_eq!(
            errs(parse_update(&json!({ "version": -1 }))),
            vec!["version must not be less than 0"]
        );
        assert_eq!(
            errs(parse_update(&json!({ "version": 1.5 }))),
            vec!["version must be an integer number"]
        );
        assert_eq!(
            errs(parse_update(&json!({ "version": "3" }))),
            vec![
                "version must not be less than 0",
                "version must be an integer number"
            ]
        );
        // description: null mengosongkan; name: null lolos validasi (500 di service Node).
        let r = parse_update(&json!({ "version": 0, "description": null, "name": null }));
        assert_eq!(
            r.as_ref().map(|u| u.description.clone()).ok(),
            Some(Field::Null)
        );
        assert_eq!(r.map(|u| u.name).ok(), Some(Field::Null));
    }

    #[test]
    fn role_body() {
        assert_eq!(
            errs(parse_role(&json!({ "role": "X" }))),
            vec![
                "role must be one of the following values: LEADER, MEMBER",
                "version must not be less than 0",
                "version must be an integer number"
            ]
        );
        assert_eq!(
            parse_role(&json!({ "role": "MEMBER", "version": 2 })).ok(),
            Some(RoleInput {
                role: MemberRole::Member,
                version: 2
            })
        );
    }

    fn q(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn query_validation() {
        assert_eq!(
            parse_list_query(&q(json!({ "search": "  eng " }))).ok(),
            Some(Some("eng".into()))
        );
        assert_eq!(
            parse_list_query(&q(json!({ "search": "  " }))).ok(),
            Some(None)
        );
        assert_eq!(
            errs(parse_list_query(&q(json!({ "search": ["a", "b"] })))),
            vec![
                "search must be shorter than or equal to 60 characters",
                "search must be a string"
            ]
        );
        let e = parse_employees_query(&q(json!({}))).ok();
        assert_eq!(
            e,
            Some(EmployeesQuery {
                search: None,
                page_number: 1,
                items_per_page: 500
            })
        );
        assert_eq!(
            errs(parse_employees_query(&q(
                json!({ "pageNumber": "0", "itemsPerPage": "501" })
            ))),
            vec![
                "pageNumber must not be less than 1",
                "itemsPerPage must not be greater than 500"
            ]
        );
        assert_eq!(
            errs(parse_employees_query(&q(json!({ "itemsPerPage": "abc" })))),
            vec![
                "itemsPerPage must not be greater than 500",
                "itemsPerPage must not be less than 1",
                "itemsPerPage must be an integer number"
            ]
        );
        assert_eq!(
            parse_version_query(&q(json!({ "version": "3" }))).ok(),
            Some(3)
        );
        assert_eq!(
            errs(parse_version_query(&q(json!({})))),
            vec![
                "version must not be less than 0",
                "version must be an integer number"
            ]
        );
    }

    #[test]
    fn report_range_rules() {
        let r = |a: Option<&str>, b: Option<&str>| {
            let (a, b) = (a.map(|s| json!(s)), b.map(|s| json!(s)));
            report_range_error(a.as_ref(), b.as_ref())
        };
        assert_eq!(r(None, None), None);
        assert_eq!(r(Some(""), Some("")), None);
        assert_eq!(
            r(Some("2026-11-01"), None).as_deref(),
            Some("startAt and endAt must be given together")
        );
        assert_eq!(
            r(Some("foo"), Some("2026-11-01")).as_deref(),
            Some("startAt and endAt must be valid dates")
        );
        assert_eq!(
            r(Some("2026-11-02"), Some("2026-11-01")).as_deref(),
            Some("endAt must be on or after startAt")
        );
        // 366 hari persis lolos; 367 ditolak (floor selisih hari).
        assert_eq!(r(Some("2025-11-01"), Some("2026-11-02")), None);
        assert_eq!(
            r(Some("2025-11-01"), Some("2026-11-03")).as_deref(),
            Some("Date range cannot exceed 366 days")
        );
        // Selisih 366 hari + 23:59:59 masih lolos (floor).
        assert_eq!(
            r(
                Some("2025-11-01T00:00:00.000Z"),
                Some("2026-11-02T23:59:59.999Z")
            ),
            None
        );
    }

    #[test]
    fn report_query_messages() {
        assert_eq!(parse_report_query(&q(json!({}))).ok(), Some(None));
        assert_eq!(
            errs(parse_report_query(&q(json!({ "startAt": "2026-11-01" })))),
            vec![
                "startAt and endAt must be given together",
                "endAt must be a valid ISO 8601 date string"
            ]
        );
        assert_eq!(
            errs(parse_report_query(&q(json!({ "endAt": "2026-11-01" })))),
            vec!["startAt and endAt must be given together"]
        );
        assert_eq!(
            errs(parse_report_query(&q(
                json!({ "startAt": "foo", "endAt": "2026-11-01" })
            ))),
            vec![
                "startAt must be a valid ISO 8601 date string",
                "startAt and endAt must be valid dates"
            ]
        );
        let ok = parse_report_query(&q(json!({
            "startAt": "2026-11-02T00:00:00.000Z", "endAt": "2026-11-04T23:59:59.999Z"
        })));
        assert!(matches!(ok, Ok(Some(_))), "{ok:?}");
    }
}
