//! DTO `src/store-visits/dto/{create,update}-store-group.dto.ts`, `list-store-groups-query.dto.ts`,
//! `replace-store-group-sales.dto.ts` — validasi manual, pesan & urutan class-validator.

use mz_core::uuid_rules::is_uuid_v4;
use mz_sales_geo::dto::{
    Field, check_length_string, check_max_string, page_limit, present, to_boolean,
};
use serde_json::{Map, Value};

use crate::enums::{SortOrder, StoreGroupSortField, is_enum_message};

/// `STORE_GROUP_CODE_PATTERN` (`/^[A-Z0-9-]{2,32}$/`).
pub fn group_code_ok(s: &str) -> bool {
    (2..=32).contains(&s.len())
        && s.bytes()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'-')
}

const CODE_MESSAGE: &str = "code must be 2-32 characters of uppercase letters, digits, or hyphens";

/// `@IsString() @Matches(PATTERN, {message})` (pesan Matches lalu IsString).
fn check_code(errs: &mut Vec<String>, v: Option<&Value>) -> Option<String> {
    let s = v.and_then(Value::as_str);
    let ok = s.is_some_and(group_code_ok);
    if !ok {
        errs.push(CODE_MESSAGE.to_owned());
    }
    if s.is_none() {
        errs.push("code must be a string".to_owned());
    }
    s.filter(|_| ok).map(str::to_owned)
}

/// `CreateStoreGroupDto`.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateStoreGroup {
    pub code: String,
    pub name: String,
    pub description: Option<String>,
}

pub fn parse_create(m: &Map<String, Value>) -> Result<CreateStoreGroup, Vec<String>> {
    let mut errs = Vec::new();
    let code = check_code(&mut errs, m.get("code"));
    let name = check_length_string(&mut errs, "name", m.get("name"), 1, 120);
    let description = present(m, "description")
        .and_then(|v| check_max_string(&mut errs, "description", Some(v), 255));
    match (code, name) {
        (Some(code), Some(name)) if errs.is_empty() => Ok(CreateStoreGroup {
            code,
            name,
            description,
        }),
        _ => Err(errs),
    }
}

/// `UpdateStoreGroupDto` (`isActive` sengaja tidak ada). `Field::Null` = dikirim `null`
/// (melewati validasi, ditulis apa adanya oleh Prisma).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct UpdateStoreGroup {
    pub code: Field<String>,
    pub name: Field<String>,
    pub description: Field<String>,
}

fn opt_field(
    m: &Map<String, Value>,
    key: &str,
    errs: &mut Vec<String>,
    check: impl FnOnce(&mut Vec<String>, Option<&Value>) -> Option<String>,
) -> Field<String> {
    match m.get(key) {
        None => Field::Absent,
        Some(Value::Null) => Field::Null,
        Some(v) => check(errs, Some(v)).map_or(Field::Absent, Field::Val),
    }
}

pub fn parse_update(m: &Map<String, Value>) -> Result<UpdateStoreGroup, Vec<String>> {
    let mut errs = Vec::new();
    let code = opt_field(m, "code", &mut errs, check_code);
    let name = opt_field(m, "name", &mut errs, |e, v| {
        check_length_string(e, "name", v, 1, 120)
    });
    let description = opt_field(m, "description", &mut errs, |e, v| {
        check_max_string(e, "description", v, 255)
    });
    if errs.is_empty() {
        Ok(UpdateStoreGroup {
            code,
            name,
            description,
        })
    } else {
        Err(errs)
    }
}

/// `ListStoreGroupsQueryDto`.
#[derive(Debug, Clone, PartialEq)]
pub struct ListStoreGroups {
    pub search: Option<String>,
    pub is_active: Option<bool>,
    pub sort: StoreGroupSortField,
    pub order: SortOrder,
    pub page: i64,
    pub limit: i64,
}

pub fn parse_list(m: &Map<String, Value>) -> Result<ListStoreGroups, Vec<String>> {
    let mut errs = Vec::new();
    let search =
        present(m, "search").and_then(|v| check_max_string(&mut errs, "search", Some(v), 120));
    let is_active = match m.get("isActive").map(to_boolean) {
        None | Some(Value::Null) => None,
        Some(Value::Bool(b)) => Some(b),
        Some(_) => {
            errs.push("isActive must be a boolean value".into());
            None
        }
    };
    let sort = match present(m, "sort") {
        None => Some(StoreGroupSortField::name),
        Some(v) => {
            let p = v.as_str().and_then(|s| s.parse().ok());
            if p.is_none() {
                errs.push(is_enum_message(
                    "sort",
                    &crate::enum_values!(StoreGroupSortField),
                ));
            }
            p
        }
    };
    let order = match present(m, "order") {
        None => Some(SortOrder::asc),
        Some(v) => {
            let p = v.as_str().and_then(|s| s.parse().ok());
            if p.is_none() {
                errs.push(is_enum_message("order", &crate::enum_values!(SortOrder)));
            }
            p
        }
    };
    let (page, limit) = page_limit(m, &mut errs);
    match (sort, order) {
        (Some(sort), Some(order)) if errs.is_empty() => Ok(ListStoreGroups {
            search,
            is_active,
            sort,
            order,
            page,
            limit,
        }),
        _ => Err(errs),
    }
}

/// `ReplaceStoreGroupSalesDto`: `@IsArray() @ArrayMaxSize(1000) @IsUUID('4', {each})`
/// (pesan: IsUUID, ArrayMaxSize, IsArray).
pub fn parse_replace_sales(m: &Map<String, Value>) -> Result<Vec<String>, Vec<String>> {
    let mut errs = Vec::new();
    let v = m.get("userUuids");
    let arr = v.and_then(Value::as_array);
    let each_ok = match (arr, v) {
        (Some(a), _) => a.iter().all(|x| x.as_str().is_some_and(is_uuid_v4)),
        (None, Some(Value::String(s))) => is_uuid_v4(s),
        _ => false,
    };
    if !each_ok {
        errs.push("each value in userUuids must be a UUID".into());
    }
    if arr.is_none_or(|a| a.len() > 1000) {
        errs.push("userUuids must contain no more than 1000 elements".into());
    }
    if arr.is_none() {
        errs.push("userUuids must be an array".into());
    }
    match arr {
        Some(a) if errs.is_empty() => Ok(a
            .iter()
            .filter_map(|x| x.as_str().map(str::to_owned))
            .collect()),
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

    #[test]
    fn code_pattern() {
        assert!(group_code_ok("JOG"));
        assert!(group_code_ok("JOG-B2"));
        assert!(!group_code_ok("J"));
        assert!(!group_code_ok("jog"));
        assert!(!group_code_ok("JOG_B"));
        assert!(!group_code_ok(&"A".repeat(33)));
    }

    #[test]
    fn create_messages() {
        assert_eq!(
            parse_create(&obj(json!({"code": 5, "name": ""}))).unwrap_err(),
            vec![
                CODE_MESSAGE,
                "code must be a string",
                "name must be longer than or equal to 1 characters"
            ]
        );
        let ok = parse_create(&obj(
            json!({"code": "JOG", "name": "Jogja", "description": null}),
        ))
        .unwrap();
        assert_eq!(ok.description, None);
    }

    /// "UpdateStoreGroupDto tidak mendeklarasikan isActive": kunci itu diabaikan.
    #[test]
    fn update_ignores_is_active() {
        let u = parse_update(&obj(json!({"isActive": false, "name": "X"}))).unwrap();
        assert_eq!(u.name, Field::Val("X".into()));
        assert_eq!(u.code, Field::Absent);
        assert!(parse_update(&obj(json!({"code": "x"}))).is_err());
    }

    #[test]
    fn list_defaults_and_errors() {
        let q = parse_list(&obj(json!({}))).unwrap();
        assert_eq!(
            (q.sort, q.order, q.page, q.limit),
            (StoreGroupSortField::name, SortOrder::asc, 1, 20)
        );
        let q = parse_list(&obj(json!({"isActive": "false"}))).unwrap();
        assert_eq!(q.is_active, Some(false));
        assert_eq!(
            parse_list(&obj(
                json!({"isActive": "yes", "sort": "storeCount", "limit": "101"})
            ))
            .unwrap_err(),
            vec![
                "isActive must be a boolean value",
                "sort must be one of the following values: name, code, createdAt",
                "limit must not be greater than 100"
            ]
        );
    }

    #[test]
    fn replace_sales_messages() {
        assert_eq!(
            parse_replace_sales(&obj(json!({}))).unwrap_err(),
            vec![
                "each value in userUuids must be a UUID",
                "userUuids must contain no more than 1000 elements",
                "userUuids must be an array"
            ]
        );
        assert_eq!(
            parse_replace_sales(&obj(json!({"userUuids": ["x"]}))).unwrap_err(),
            vec!["each value in userUuids must be a UUID"]
        );
        assert!(
            parse_replace_sales(&obj(json!({"userUuids": []})))
                .unwrap()
                .is_empty()
        );
    }
}
