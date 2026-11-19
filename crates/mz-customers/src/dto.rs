//! DTO `src/customers/dto/*` (class-validator + class-transformer) sebagai validator manual atas JSON
//! mentah, supaya urutan & bunyi pesan identik dengan `ValidationPipe({ transform: true })`.
//! Dekorator dievaluasi dari BAWAH ke ATAS; properti turunan `PartialType`/`OmitType` (PATCH) divalidasi
//! sebelum properti yang dideklarasi ulang.

use mz_core::uuid_rules::is_uuid_v4;
use mz_sales_geo::dto::{
    Field, check_bool, check_length_string, check_max_string, gate, page_limit, present, to_boolean,
};
use serde_json::{Map, Value};

/// `CUSTOMER_CODE_PATTERN` / `CUSTOMER_MASTER_CODE_PATTERN = /^[A-Z0-9][A-Z0-9_-]{0,31}$/`.
pub fn code_pattern_ok(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_uppercase() || first.is_ascii_digit())
        && s.chars().count() <= 32
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

const CODE_MESSAGE: &str =
    "code must be 1-32 characters of uppercase letters, digits, underscore, or hyphen";

/// `@Transform(upper) @IsString() @Matches(pattern)` (pesan: Matches lebih dulu, lalu IsString).
fn check_code(errs: &mut Vec<String>, v: Option<&Value>) -> Option<String> {
    let upper = match v {
        Some(Value::String(s)) => Some(s.to_uppercase()),
        _ => None,
    };
    match upper {
        Some(s) if code_pattern_ok(&s) => Some(s),
        other => {
            errs.push(CODE_MESSAGE.to_owned());
            if other.is_none() {
                errs.push("code must be a string".to_owned());
            }
            None
        }
    }
}

/// `@IsOptional() @IsUUID('4')`: `Absent` / `Null` / nilai sah.
fn opt_uuid(m: &Map<String, Value>, key: &str, errs: &mut Vec<String>) -> Field<String> {
    match m.get(key) {
        None => Field::Absent,
        Some(Value::Null) => Field::Null,
        Some(Value::String(s)) if is_uuid_v4(s) => Field::Val(s.clone()),
        Some(_) => {
            errs.push(format!("{key} must be a UUID"));
            Field::Absent
        }
    }
}

/// `@IsOptional() @IsString() @MaxLength(max)`.
fn opt_max_string(
    m: &Map<String, Value>,
    key: &str,
    max: usize,
    errs: &mut Vec<String>,
) -> Field<String> {
    match m.get(key) {
        None => Field::Absent,
        Some(Value::Null) => Field::Null,
        Some(v) => match check_max_string(errs, key, Some(v), max) {
            Some(s) => Field::Val(s),
            None => Field::Absent,
        },
    }
}

/// `@IsOptional() @IsBoolean()` (tanpa `@Transform`: body JSON).
fn opt_bool(m: &Map<String, Value>, key: &str, errs: &mut Vec<String>) -> Field<bool> {
    match m.get(key) {
        None => Field::Absent,
        Some(Value::Null) => Field::Null,
        Some(v) => match check_bool(errs, key, Some(v)) {
            Some(b) => Field::Val(b),
            None => Field::Absent,
        },
    }
}

/// `Field::Val` → `Some`, selain itu `None` (create: `null` = tidak dikirim).
fn val<T>(f: Field<T>) -> Option<T> {
    match f {
        Field::Val(v) => Some(v),
        _ => None,
    }
}

// ───────────────────────── customer ─────────────────────────

/// `CreateCustomerDto`.
#[derive(Debug)]
pub struct CreateCustomer {
    pub code: String,
    pub name: String,
    pub type_id: Option<String>,
    pub group_id: Option<String>,
    pub channel_id: Option<String>,
    pub sub_channel_id: Option<String>,
    pub price_list_id: Option<String>,
    pub tax_number: Option<String>,
    pub phone: Option<String>,
    pub is_active: Option<bool>,
}

pub fn parse_create_customer(m: &Map<String, Value>) -> Result<CreateCustomer, Vec<String>> {
    let mut errs = Vec::new();
    let code = check_code(&mut errs, m.get("code"));
    let name = check_length_string(&mut errs, "name", m.get("name"), 1, 160);
    let type_id = val(opt_uuid(m, "typeId", &mut errs));
    let group_id = val(opt_uuid(m, "groupId", &mut errs));
    let channel_id = val(opt_uuid(m, "channelId", &mut errs));
    let sub_channel_id = val(opt_uuid(m, "subChannelId", &mut errs));
    let price_list_id = val(opt_uuid(m, "priceListId", &mut errs));
    let tax_number = val(opt_max_string(m, "taxNumber", 32, &mut errs));
    let phone = val(opt_max_string(m, "phone", 20, &mut errs));
    let is_active = val(opt_bool(m, "isActive", &mut errs));
    match (code, name) {
        (Some(code), Some(name)) if errs.is_empty() => Ok(CreateCustomer {
            code,
            name,
            type_id,
            group_id,
            channel_id,
            sub_channel_id,
            price_list_id,
            tax_number,
            phone,
            is_active,
        }),
        _ => Err(errs),
    }
}

/// `UpdateCustomerDto` (PATCH; `null` eksplisit dibedakan dari tidak dikirim).
#[derive(Debug, Default)]
pub struct UpdateCustomer {
    pub code: Field<String>,
    pub name: Field<String>,
    pub tax_number: Field<String>,
    pub phone: Field<String>,
    pub is_active: Field<bool>,
    pub type_id: Field<String>,
    pub group_id: Field<String>,
    pub channel_id: Field<String>,
    pub sub_channel_id: Field<String>,
    pub price_list_id: Field<String>,
}

pub fn parse_update_customer(m: &Map<String, Value>) -> Result<UpdateCustomer, Vec<String>> {
    let mut errs = Vec::new();
    let mut d = UpdateCustomer::default();
    if let Some(v) = gate(m, "code", false) {
        d.code = check_code(&mut errs, v).map_or(Field::Absent, Field::Val);
    } else if m.get("code").is_some_and(Value::is_null) {
        d.code = Field::Null;
    }
    if let Some(v) = gate(m, "name", false) {
        d.name =
            check_length_string(&mut errs, "name", v, 1, 160).map_or(Field::Absent, Field::Val);
    } else if m.get("name").is_some_and(Value::is_null) {
        d.name = Field::Null;
    }
    d.tax_number = opt_max_string(m, "taxNumber", 32, &mut errs);
    d.phone = opt_max_string(m, "phone", 20, &mut errs);
    d.is_active = opt_bool(m, "isActive", &mut errs);
    d.type_id = opt_uuid(m, "typeId", &mut errs);
    d.group_id = opt_uuid(m, "groupId", &mut errs);
    d.channel_id = opt_uuid(m, "channelId", &mut errs);
    d.sub_channel_id = opt_uuid(m, "subChannelId", &mut errs);
    d.price_list_id = opt_uuid(m, "priceListId", &mut errs);
    if errs.is_empty() { Ok(d) } else { Err(errs) }
}

/// `SortDir`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortDir {
    Asc,
    Desc,
}

impl SortDir {
    pub fn sql(self) -> &'static str {
        match self {
            Self::Asc => "ASC",
            Self::Desc => "DESC",
        }
    }
}

/// `CustomerSortField`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortBy {
    Name,
    Code,
    CreatedAt,
}

impl SortBy {
    /// Kunci kolom registry `customers` (juga bentuk di query string).
    pub fn key(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Code => "code",
            Self::CreatedAt => "createdAt",
        }
    }
}

/// `ListCustomersQueryDto` / `ExportCustomersQueryDto` (tanpa `page`/`limit`).
#[derive(Debug, Default)]
pub struct ListCustomersQuery {
    pub search: Option<String>,
    pub type_id: Option<Vec<String>>,
    pub group_id: Option<Vec<String>>,
    pub channel_id: Option<Vec<String>>,
    pub sub_channel_id: Option<Vec<String>>,
    pub is_active: Option<bool>,
    pub has_stores: Option<bool>,
    pub sort_by: Option<SortBy>,
    pub sort_dir: Option<SortDir>,
    pub page: i64,
    pub limit: i64,
}

/// `@Transform(toArray) @IsOptional() @IsArray() @ArrayMaxSize(100) @IsUUID('4', { each: true })`
/// (pesan: each-UUID, ArrayMaxSize, IsArray). Kosong/tidak ada = `None`.
fn uuid_array(m: &Map<String, Value>, key: &str, errs: &mut Vec<String>) -> Option<Vec<String>> {
    let items: Vec<String> = match m.get(key) {
        None | Some(Value::Null) => return None,
        Some(Value::String(s)) if s.is_empty() => return None,
        Some(Value::Array(a)) => a
            .iter()
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => mz_sales_geo::dto::js_to_string(other),
            })
            .collect(),
        Some(Value::String(s)) => s.split(',').map(str::to_owned).collect(),
        Some(other) => mz_sales_geo::dto::js_to_string(other)
            .split(',')
            .map(str::to_owned)
            .collect(),
    };
    if !items.iter().all(|s| is_uuid_v4(s)) {
        errs.push(format!("each value in {key} must be a UUID"));
    }
    if items.len() > 100 {
        errs.push(format!("{key} must contain no more than 100 elements"));
    }
    Some(items)
}

fn opt_query_bool(m: &Map<String, Value>, key: &str, errs: &mut Vec<String>) -> Option<bool> {
    present(m, key).and_then(|v| check_bool(errs, key, Some(&to_boolean(v))))
}

pub fn parse_list_customers(
    m: &Map<String, Value>,
    with_paging: bool,
) -> Result<ListCustomersQuery, Vec<String>> {
    let mut errs = Vec::new();
    let mut q = ListCustomersQuery {
        page: 1,
        limit: 20,
        ..ListCustomersQuery::default()
    };
    if let Some(v) = present(m, "search") {
        q.search = check_max_string(&mut errs, "search", Some(v), 120);
    }
    q.type_id = uuid_array(m, "typeId", &mut errs);
    q.group_id = uuid_array(m, "groupId", &mut errs);
    q.channel_id = uuid_array(m, "channelId", &mut errs);
    q.sub_channel_id = uuid_array(m, "subChannelId", &mut errs);
    q.is_active = opt_query_bool(m, "isActive", &mut errs);
    q.has_stores = opt_query_bool(m, "hasStores", &mut errs);
    if let Some(v) = present(m, "sortBy") {
        q.sort_by = match v.as_str() {
            Some("name") => Some(SortBy::Name),
            Some("code") => Some(SortBy::Code),
            Some("createdAt") => Some(SortBy::CreatedAt),
            _ => {
                errs.push(
                    "sortBy must be one of the following values: name, code, createdAt".to_owned(),
                );
                None
            }
        };
    }
    if let Some(v) = present(m, "sortDir") {
        q.sort_dir = match v.as_str() {
            Some("asc") => Some(SortDir::Asc),
            Some("desc") => Some(SortDir::Desc),
            _ => {
                errs.push("sortDir must be one of the following values: asc, desc".to_owned());
                None
            }
        };
    }
    if with_paging {
        (q.page, q.limit) = page_limit(m, &mut errs);
    }
    if errs.is_empty() { Ok(q) } else { Err(errs) }
}

// ───────────────────────── customer masters ─────────────────────────

/// `CreateCustomerMasterDto`.
#[derive(Debug)]
pub struct CreateMaster {
    pub code: String,
    pub name: String,
    pub description: Option<String>,
    /// Hanya `sub-channels`; `null` = tidak dikirim.
    pub channel_id: Option<String>,
}

pub fn parse_create_master(m: &Map<String, Value>) -> Result<CreateMaster, Vec<String>> {
    let mut errs = Vec::new();
    let code = check_code(&mut errs, m.get("code"));
    let name = check_length_string(&mut errs, "name", m.get("name"), 1, 120);
    let description = val(opt_max_string(m, "description", 255, &mut errs));
    let channel_id = val(opt_uuid(m, "channelId", &mut errs));
    match (code, name) {
        (Some(code), Some(name)) if errs.is_empty() => Ok(CreateMaster {
            code,
            name,
            description,
            channel_id,
        }),
        _ => Err(errs),
    }
}

/// `UpdateCustomerMasterDto`.
#[derive(Debug, Default)]
pub struct UpdateMaster {
    pub code: Field<String>,
    pub name: Field<String>,
    pub description: Field<String>,
    pub is_active: Field<bool>,
    pub channel_id: Field<String>,
}

pub fn parse_update_master(m: &Map<String, Value>) -> Result<UpdateMaster, Vec<String>> {
    let mut errs = Vec::new();
    let mut d = UpdateMaster::default();
    if let Some(v) = gate(m, "code", false) {
        d.code = check_code(&mut errs, v).map_or(Field::Absent, Field::Val);
    } else if m.get("code").is_some_and(Value::is_null) {
        d.code = Field::Null;
    }
    if let Some(v) = gate(m, "name", false) {
        d.name =
            check_length_string(&mut errs, "name", v, 1, 120).map_or(Field::Absent, Field::Val);
    } else if m.get("name").is_some_and(Value::is_null) {
        d.name = Field::Null;
    }
    d.description = opt_max_string(m, "description", 255, &mut errs);
    d.is_active = opt_bool(m, "isActive", &mut errs);
    d.channel_id = opt_uuid(m, "channelId", &mut errs);
    if errs.is_empty() { Ok(d) } else { Err(errs) }
}

/// `ListCustomerMastersQueryDto`.
#[derive(Debug)]
pub struct ListMastersQuery {
    pub search: Option<String>,
    pub is_active: Option<bool>,
    pub channel_id: Option<String>,
    pub page: i64,
    pub limit: i64,
}

pub fn parse_list_masters(m: &Map<String, Value>) -> Result<ListMastersQuery, Vec<String>> {
    let mut errs = Vec::new();
    let mut q = ListMastersQuery {
        search: None,
        is_active: None,
        channel_id: None,
        page: 1,
        limit: 20,
    };
    if let Some(v) = present(m, "search") {
        q.search = check_max_string(&mut errs, "search", Some(v), 120);
    }
    q.is_active = opt_query_bool(m, "isActive", &mut errs);
    if let Some(v) = present(m, "channelId") {
        match v.as_str().filter(|s| is_uuid_v4(s)) {
            Some(s) => q.channel_id = Some(s.to_owned()),
            None => errs.push("channelId must be a UUID".to_owned()),
        }
    }
    (q.page, q.limit) = page_limit(m, &mut errs);
    if errs.is_empty() { Ok(q) } else { Err(errs) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap_or_default()
    }

    const U4: &str = "9f1c2d3e-4a5b-4c7d-8e9f-0a1b2c3d4e5f";

    #[test]
    fn code_pattern() {
        assert!(code_pattern_ok("CUST-001"));
        assert!(code_pattern_ok("A"));
        assert!(code_pattern_ok(&"A".repeat(32)));
        assert!(!code_pattern_ok(&"A".repeat(33)));
        assert!(!code_pattern_ok(""));
        assert!(!code_pattern_ok("-A"));
        assert!(!code_pattern_ok("_A"));
        assert!(!code_pattern_ok("A B"));
        assert!(!code_pattern_ok("a"));
        assert!(!code_pattern_ok("A\n"));
    }

    #[test]
    fn create_customer_uppercases_code_and_accepts_minimal() {
        let d = parse_create_customer(&obj(json!({"code":"cust-001","name":"PT X"}))).unwrap();
        assert_eq!(d.code, "CUST-001");
        assert_eq!(d.name, "PT X");
        assert!(d.type_id.is_none() && d.is_active.is_none());
    }

    #[test]
    fn create_customer_empty_body_messages_in_decorator_order() {
        let e = parse_create_customer(&obj(json!({}))).unwrap_err();
        assert_eq!(
            e,
            vec![
                CODE_MESSAGE.to_owned(),
                "code must be a string".to_owned(),
                "name must be longer than or equal to 1 characters".to_owned(),
                "name must be a string".to_owned(),
            ]
        );
    }

    #[test]
    fn create_customer_field_validation() {
        let e = parse_create_customer(&obj(json!({
            "code":"X","name":"n","typeId":"nope","taxNumber":"x".repeat(33),
            "phone":5,"isActive":"yes"
        })))
        .unwrap_err();
        assert_eq!(
            e,
            vec![
                "typeId must be a UUID".to_owned(),
                "taxNumber must be shorter than or equal to 32 characters".to_owned(),
                "phone must be shorter than or equal to 20 characters".to_owned(),
                "phone must be a string".to_owned(),
                "isActive must be a boolean value".to_owned(),
            ]
        );
    }

    #[test]
    fn create_customer_null_optionals_count_as_absent() {
        let d = parse_create_customer(&obj(json!({
            "code":"A","name":"n","typeId":null,"isActive":null,"taxNumber":null
        })))
        .unwrap();
        assert!(d.type_id.is_none() && d.is_active.is_none() && d.tax_number.is_none());
    }

    #[test]
    fn create_customer_uuid_must_be_v4() {
        let e = parse_create_customer(&obj(json!({
            "code":"A","name":"n","groupId":"12345678-1234-1234-1234-123456789012"
        })))
        .unwrap_err();
        assert_eq!(e, vec!["groupId must be a UUID".to_owned()]);
        assert!(parse_create_customer(&obj(json!({"code":"A","name":"n","groupId":U4}))).is_ok());
    }

    #[test]
    fn update_customer_distinguishes_null_from_absent() {
        let d = parse_update_customer(&obj(json!({"typeId":null,"priceListId":U4,"name":null})))
            .unwrap();
        assert_eq!(d.type_id, Field::Null);
        assert_eq!(d.group_id, Field::Absent);
        assert_eq!(d.price_list_id, Field::Val(U4.to_owned()));
        assert_eq!(d.name, Field::Null);
    }

    #[test]
    fn update_customer_validates_partial_fields() {
        let e =
            parse_update_customer(&obj(json!({"code":"bad code","subChannelId":"x"}))).unwrap_err();
        assert_eq!(
            e,
            vec![
                CODE_MESSAGE.to_owned(),
                "subChannelId must be a UUID".to_owned()
            ]
        );
    }

    #[test]
    fn list_query_defaults_and_array_forms() {
        let q = parse_list_customers(&obj(json!({"typeId": format!("{U4},{U4}")})), true).unwrap();
        assert_eq!((q.page, q.limit), (1, 20));
        assert_eq!(q.type_id.as_deref().map(<[String]>::len), Some(2));
        let q = parse_list_customers(&obj(json!({"typeId": [U4]})), true).unwrap();
        assert_eq!(q.type_id, Some(vec![U4.to_owned()]));
        let q = parse_list_customers(&obj(json!({"typeId": ""})), true).unwrap();
        assert!(q.type_id.is_none());
    }

    #[test]
    fn list_query_errors() {
        let e = parse_list_customers(
            &obj(json!({"typeId":"x","isActive":"maybe","sortBy":"id","sortDir":"up","page":"0","limit":"101"})),
            true,
        )
        .unwrap_err();
        assert_eq!(
            e,
            vec![
                "each value in typeId must be a UUID".to_owned(),
                "isActive must be a boolean value".to_owned(),
                "sortBy must be one of the following values: name, code, createdAt".to_owned(),
                "sortDir must be one of the following values: asc, desc".to_owned(),
                "page must not be less than 1".to_owned(),
                "limit must not be greater than 100".to_owned(),
            ]
        );
    }

    #[test]
    fn list_query_booleans_and_sort() {
        let q = parse_list_customers(
            &obj(json!({"isActive":"false","hasStores":"true","sortBy":"createdAt","sortDir":"desc"})),
            true,
        )
        .unwrap();
        assert_eq!(q.is_active, Some(false));
        assert_eq!(q.has_stores, Some(true));
        assert_eq!(q.sort_by, Some(SortBy::CreatedAt));
        assert_eq!(q.sort_dir, Some(SortDir::Desc));
    }

    #[test]
    fn export_query_ignores_paging() {
        let q = parse_list_customers(&obj(json!({"page":"zzz","limit":"-4"})), false).unwrap();
        assert_eq!((q.page, q.limit), (1, 20));
    }

    #[test]
    fn master_create_messages() {
        let e = parse_create_master(&obj(
            json!({"code":"a b","name":"","description":"x".repeat(256),"channelId":"x"}),
        ))
        .unwrap_err();
        assert_eq!(
            e,
            vec![
                CODE_MESSAGE.to_owned(),
                "name must be longer than or equal to 1 characters".to_owned(),
                "description must be shorter than or equal to 255 characters".to_owned(),
                "channelId must be a UUID".to_owned(),
            ]
        );
        let d = parse_create_master(&obj(json!({"code":"dist","name":"Distributor"}))).unwrap();
        assert_eq!(d.code, "DIST");
        assert!(d.description.is_none() && d.channel_id.is_none());
    }

    #[test]
    fn master_update_and_list() {
        let d = parse_update_master(&obj(json!({"isActive":false,"description":null}))).unwrap();
        assert_eq!(d.is_active, Field::Val(false));
        assert_eq!(d.description, Field::Null);
        let e = parse_update_master(&obj(json!({"isActive":"x"}))).unwrap_err();
        assert_eq!(e, vec!["isActive must be a boolean value".to_owned()]);
        let q = parse_list_masters(&obj(json!({"isActive":"true","channelId":U4,"limit":"5"})))
            .unwrap();
        assert_eq!((q.is_active, q.limit), (Some(true), 5));
        assert_eq!(q.channel_id.as_deref(), Some(U4));
        assert!(parse_list_masters(&obj(json!({"channelId":"x"}))).is_err());
    }
}
