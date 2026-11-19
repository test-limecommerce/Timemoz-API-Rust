//! DTO toko (`create-store`, `update-store`, `list-stores-query`, `export-stores-query`,
//! `bulk-group-stores`, `bulk-customer-stores`, `create-mobile-store`, `list-mobile-stores-query`,
//! `resolve-store-qr`) sebagai validator manual atas JSON mentah. Dekorator class-validator
//! dievaluasi dari BAWAH ke ATAS; properti `PartialType` turunan divalidasi sebelum yang
//! dideklarasi ulang.

use mz_core::extract::{check_int_range, validator_js_length};
use mz_core::uuid_rules::is_uuid_v4;
use mz_sales_geo::dto::{
    Field, as_number, check_bool, check_number, js_to_string, page_limit, parse_float, present,
    to_boolean,
};
use serde_json::{Map, Value};

use crate::enums::{
    AdminStoreSortField, ExportFormat, MobileStoreSortField, SortOrder, is_enum_message,
};

const CODE_MESSAGE: &str =
    "code must match format like MJ-001 (2-6 letters/digits, dash, 1-6 letters/digits)";
const PHONE_MESSAGE: &str = "phone may only contain digits, +, spaces, and -";
const QR_MESSAGE: &str = "qrToken must be 64 hexadecimal characters";

/// `STORE_CODE_REGEX = /^[A-Z0-9]{2,6}-[A-Z0-9]{1,6}$/`.
pub fn store_code_ok(s: &str) -> bool {
    let Some((a, b)) = s.split_once('-') else {
        return false;
    };
    let part = |p: &str, min: usize, max: usize| {
        (min..=max).contains(&p.len())
            && p.bytes()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
    };
    part(a, 2, 6) && part(b, 1, 6)
}

/// `\s` JavaScript.
fn is_js_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{0B}' | '\u{0C}' | '\r' | ' ' | '\u{A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

/// `PHONE_REGEX = /^[0-9+\-\s]{8,20}$/`.
pub fn phone_ok(s: &str) -> bool {
    let n = s.encode_utf16().count();
    (8..=20).contains(&n)
        && s.chars()
            .all(|c| c.is_ascii_digit() || c == '+' || c == '-' || is_js_space(c))
}

/// `@Transform(upper) @IsString() @Matches(STORE_CODE_REGEX)` (pesan: Matches, IsString).
fn check_code(errs: &mut Vec<String>, v: Option<&Value>) -> Option<String> {
    let upper = match v {
        Some(Value::String(s)) => Some(s.to_uppercase()),
        _ => None,
    };
    match upper {
        Some(s) if store_code_ok(&s) => Some(s),
        other => {
            errs.push(CODE_MESSAGE.to_owned());
            if other.is_none() {
                errs.push("code must be a string".to_owned());
            }
            None
        }
    }
}

/// `@IsString() @MinLength(min) @MaxLength(max)` (pesan: MaxLength, MinLength, IsString).
fn check_min_max(
    errs: &mut Vec<String>,
    field: &str,
    v: Option<&Value>,
    min: usize,
    max: usize,
) -> Option<String> {
    let s = match v {
        Some(Value::String(s)) => Some(s.as_str()),
        _ => None,
    };
    let len = s.map(validator_js_length);
    let before = errs.len();
    if len.is_none_or(|l| l > max) {
        errs.push(format!(
            "{field} must be shorter than or equal to {max} characters"
        ));
    }
    if len.is_none_or(|l| l < min) {
        errs.push(format!(
            "{field} must be longer than or equal to {min} characters"
        ));
    }
    if s.is_none() {
        errs.push(format!("{field} must be a string"));
    }
    s.filter(|_| errs.len() == before).map(str::to_owned)
}

/// `@IsString() @MinLength(8) @MaxLength(20) @Matches(PHONE_REGEX)` (Matches, MaxLength, MinLength, IsString).
fn check_phone(errs: &mut Vec<String>, v: Option<&Value>) -> Option<String> {
    let before = errs.len();
    let s = match v {
        Some(Value::String(s)) => Some(s.as_str()),
        _ => None,
    };
    if !s.is_some_and(phone_ok) {
        errs.push(PHONE_MESSAGE.to_owned());
    }
    let mut tmp = Vec::new();
    let ok = check_min_max(&mut tmp, "phone", v, 8, 20);
    errs.extend(tmp);
    ok.filter(|_| errs.len() == before)
}

/// `@IsUUID('4')` atas nilai (bukan `null`).
fn check_uuid(errs: &mut Vec<String>, field: &str, v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::String(s)) if is_uuid_v4(s) => Some(s.clone()),
        _ => {
            errs.push(format!("{field} must be a UUID"));
            None
        }
    }
}

/// `@IsOptional()` + validator: `Absent` / `Null` / nilai (gagal → `Absent`, galat dicatat).
fn opt<T>(
    m: &Map<String, Value>,
    key: &str,
    errs: &mut Vec<String>,
    f: impl FnOnce(&mut Vec<String>, Option<&Value>) -> Option<T>,
) -> Field<T> {
    match m.get(key) {
        None => Field::Absent,
        Some(Value::Null) => Field::Null,
        Some(v) => f(errs, Some(v)).map_or(Field::Absent, Field::Val),
    }
}

/// Wajib (create) atau opsional (`PartialType`, update).
fn field<T>(
    m: &Map<String, Value>,
    key: &str,
    required: bool,
    errs: &mut Vec<String>,
    f: impl FnOnce(&mut Vec<String>, Option<&Value>) -> Option<T>,
) -> Field<T> {
    if required {
        f(errs, m.get(key)).map_or(Field::Absent, Field::Val)
    } else {
        opt(m, key, errs, f)
    }
}

fn num_field(
    m: &Map<String, Value>,
    key: &str,
    required: bool,
    lim: f64,
    errs: &mut Vec<String>,
) -> Field<f64> {
    field(m, key, required, errs, |e, v| {
        check_number(e, key, as_number(v), Some(-lim), Some(lim))
    })
}

// ───────────────────────── admin create / update ─────────────────────────

/// `CreateStoreDto` (`required = true`) / `UpdateStoreDto` (`required = false`, semua `Field`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StoreInput {
    pub code: Field<String>,
    pub name: Field<String>,
    pub owner_name: Field<String>,
    pub phone: Field<String>,
    pub address: Field<String>,
    pub lat: Field<f64>,
    pub lng: Field<f64>,
    pub radius_meters: Field<i64>,
    pub is_active: Field<bool>,
    pub group_id: Field<String>,
    pub customer_id: Field<String>,
}

fn parse_store_input(m: &Map<String, Value>, required: bool) -> Result<StoreInput, Vec<String>> {
    let mut errs = Vec::new();
    let r = required;
    let input = StoreInput {
        code: field(m, "code", r, &mut errs, check_code),
        name: field(m, "name", r, &mut errs, |e, v| {
            check_min_max(e, "name", v, 3, 120)
        }),
        owner_name: field(m, "ownerName", r, &mut errs, |e, v| {
            check_min_max(e, "ownerName", v, 3, 80)
        }),
        phone: opt(m, "phone", &mut errs, check_phone),
        address: field(m, "address", r, &mut errs, |e, v| {
            check_min_max(e, "address", v, 10, 255)
        }),
        lat: num_field(m, "lat", r, 90.0, &mut errs),
        lng: num_field(m, "lng", r, 180.0, &mut errs),
        radius_meters: opt(m, "radiusMeters", &mut errs, |e, v| {
            check_int_range(e, "radiusMeters", as_number(v), 20, 5000)
        }),
        is_active: opt(m, "isActive", &mut errs, |e, v| {
            check_bool(e, "isActive", v)
        }),
        group_id: opt(m, "groupId", &mut errs, |e, v| check_uuid(e, "groupId", v)),
        customer_id: opt(m, "customerId", &mut errs, |e, v| {
            check_uuid(e, "customerId", v)
        }),
    };
    if errs.is_empty() {
        Ok(input)
    } else {
        Err(errs)
    }
}

/// `CreateStoreDto`.
pub fn parse_create_store(m: &Map<String, Value>) -> Result<StoreInput, Vec<String>> {
    parse_store_input(m, true)
}

/// `UpdateStoreDto` (`PartialType(OmitType(Create, [groupId, customerId]))` + `groupId`/`customerId`
/// nullable — urutan properti tetap sama).
pub fn parse_update_store(m: &Map<String, Value>) -> Result<StoreInput, Vec<String>> {
    parse_store_input(m, false)
}

// ───────────────────────── admin list / export ─────────────────────────

/// `ListStoresQueryDto` / `ExportStoresQueryDto`.
#[derive(Debug, Clone, PartialEq)]
pub struct ListStoresQuery {
    pub search: Option<String>,
    pub is_active: Option<bool>,
    pub group_id: Option<Vec<String>>,
    pub sales_id: Option<String>,
    pub unassigned: Option<bool>,
    pub customer_id: Option<Vec<String>>,
    pub unassigned_customer: Option<bool>,
    pub sort: AdminStoreSortField,
    pub order: SortOrder,
    pub page: i64,
    pub limit: i64,
    pub format: ExportFormat,
}

/// `@Transform(toArray) @IsOptional() @IsArray() @ArrayMaxSize(max) @IsUUID('4', { each: true })`.
pub fn uuid_array(
    m: &Map<String, Value>,
    key: &str,
    max: usize,
    errs: &mut Vec<String>,
) -> Option<Vec<String>> {
    let items: Vec<String> = match m.get(key) {
        None | Some(Value::Null) => return None,
        Some(Value::String(s)) if s.is_empty() => return None,
        Some(Value::Array(a)) => a
            .iter()
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => js_to_string(other),
            })
            .collect(),
        Some(other) => js_to_string(other).split(',').map(str::to_owned).collect(),
    };
    let before = errs.len();
    if !items.iter().all(|s| is_uuid_v4(s)) {
        errs.push(format!("each value in {key} must be a UUID"));
    }
    if items.len() > max {
        errs.push(format!("{key} must contain no more than {max} elements"));
    }
    (errs.len() == before).then_some(items)
}

fn opt_query_bool(m: &Map<String, Value>, key: &str, errs: &mut Vec<String>) -> Option<bool> {
    present(m, key).and_then(|v| check_bool(errs, key, Some(&to_boolean(v))))
}

fn opt_string(m: &Map<String, Value>, key: &str, errs: &mut Vec<String>) -> Option<String> {
    present(m, key).and_then(|v| mz_sales_geo::dto::check_string(errs, key, Some(v)))
}

fn opt_enum<E: std::str::FromStr + Copy>(
    m: &Map<String, Value>,
    key: &str,
    values: &[&str],
    default: E,
    errs: &mut Vec<String>,
) -> E {
    match present(m, key) {
        None => default,
        Some(v) => match v.as_str().and_then(|s| s.parse::<E>().ok()) {
            Some(e) => e,
            None => {
                errs.push(is_enum_message(key, values));
                default
            }
        },
    }
}

/// `ListStoresQueryDto` (`export = false`) atau `ExportStoresQueryDto` (`export = true`: tanpa
/// page/limit, tambah `format`).
pub fn parse_list_stores(
    m: &Map<String, Value>,
    export: bool,
) -> Result<ListStoresQuery, Vec<String>> {
    let mut errs = Vec::new();
    let search = opt_string(m, "search", &mut errs);
    let is_active = opt_query_bool(m, "isActive", &mut errs);
    let group_id = uuid_array(m, "groupId", 50, &mut errs);
    let sales_id = present(m, "salesId").and_then(|v| check_uuid(&mut errs, "salesId", Some(v)));
    let unassigned = opt_query_bool(m, "unassigned", &mut errs);
    let customer_id = uuid_array(m, "customerId", 50, &mut errs);
    let unassigned_customer = opt_query_bool(m, "unassignedCustomer", &mut errs);
    let sort = opt_enum(
        m,
        "sort",
        &crate::enum_values!(AdminStoreSortField),
        AdminStoreSortField::name,
        &mut errs,
    );
    let order = opt_enum(
        m,
        "order",
        &crate::enum_values!(SortOrder),
        SortOrder::asc,
        &mut errs,
    );
    let (mut page, mut limit) = (1, 20);
    let mut format = ExportFormat::csv;
    if export {
        format = opt_enum(
            m,
            "format",
            &crate::enum_values!(ExportFormat),
            ExportFormat::csv,
            &mut errs,
        );
    } else {
        (page, limit) = page_limit(m, &mut errs);
    }
    if !errs.is_empty() {
        return Err(errs);
    }
    Ok(ListStoresQuery {
        search,
        is_active,
        group_id,
        sales_id,
        unassigned,
        customer_id,
        unassigned_customer,
        sort,
        order,
        page,
        limit,
        format,
    })
}

// ───────────────────────── bulk ─────────────────────────

/// `@IsArray() @ArrayMinSize(1) @ArrayMaxSize(500) @IsUUID('4', { each: true })`
/// (pesan: each-UUID, ArrayMaxSize, ArrayMinSize, IsArray).
fn check_uuid_list(errs: &mut Vec<String>, key: &str, v: Option<&Value>) -> Option<Vec<String>> {
    let before = errs.len();
    let each_ok = match v {
        Some(Value::Array(a)) => a.iter().all(|x| x.as_str().is_some_and(is_uuid_v4)),
        Some(Value::String(s)) => is_uuid_v4(s),
        _ => false,
    };
    if !each_ok {
        errs.push(format!("each value in {key} must be a UUID"));
    }
    let len = v.and_then(Value::as_array).map(Vec::len);
    if len.is_none_or(|l| l > 500) {
        errs.push(format!("{key} must contain no more than 500 elements"));
    }
    if len.is_none_or(|l| l < 1) {
        errs.push(format!("{key} must contain at least 1 elements"));
    }
    if len.is_none() {
        errs.push(format!("{key} must be an array"));
    }
    if errs.len() != before {
        return None;
    }
    v.and_then(Value::as_array).map(|a| {
        a.iter()
            .filter_map(|x| x.as_str().map(str::to_owned))
            .collect()
    })
}

/// `@ValidateIf(o => o.x !== null) @IsUUID('4')`: `null` → `None` (lepas), selain itu wajib UUID.
fn nullable_uuid(
    m: &Map<String, Value>,
    key: &str,
    errs: &mut Vec<String>,
) -> Option<Option<String>> {
    match m.get(key) {
        Some(Value::Null) => Some(None),
        v => check_uuid(errs, key, v).map(Some),
    }
}

/// `BulkGroupStoresDto` / `BulkCustomerStoresDto`: `(uuid toko, target nullable)`.
#[derive(Debug, Clone, PartialEq)]
pub struct BulkInput {
    pub store_uuids: Vec<String>,
    pub target: Option<String>,
}

/// `list_key` = `storeUuids`/`storeIds`, `target_key` = `groupId`/`customerId`.
pub fn parse_bulk(
    m: &Map<String, Value>,
    list_key: &str,
    target_key: &str,
) -> Result<BulkInput, Vec<String>> {
    let mut errs = Vec::new();
    let list = check_uuid_list(&mut errs, list_key, m.get(list_key));
    let target = nullable_uuid(m, target_key, &mut errs);
    match (list, target) {
        (Some(store_uuids), Some(target)) if errs.is_empty() => Ok(BulkInput {
            store_uuids,
            target,
        }),
        _ => Err(errs),
    }
}

// ───────────────────────── mobile ─────────────────────────

/// `CreateMobileStoreDto` (kode, radius, isActive TIDAK diterima dari klien).
#[derive(Debug, Clone, PartialEq)]
pub struct CreateMobileStore {
    pub name: String,
    pub owner_name: String,
    pub phone: Option<String>,
    pub address: String,
    pub lat: f64,
    pub lng: f64,
    /// `Field::Null` = `groupId: null` dikirim eksplisit.
    pub group_id: Field<String>,
}

pub fn parse_create_mobile_store(m: &Map<String, Value>) -> Result<CreateMobileStore, Vec<String>> {
    let mut errs = Vec::new();
    let name = check_min_max(&mut errs, "name", m.get("name"), 3, 120);
    let owner_name = check_min_max(&mut errs, "ownerName", m.get("ownerName"), 3, 80);
    let phone = opt(m, "phone", &mut errs, check_phone);
    let address = check_min_max(&mut errs, "address", m.get("address"), 10, 255);
    let lat = check_number(
        &mut errs,
        "lat",
        as_number(m.get("lat")),
        Some(-90.0),
        Some(90.0),
    );
    let lng = check_number(
        &mut errs,
        "lng",
        as_number(m.get("lng")),
        Some(-180.0),
        Some(180.0),
    );
    let group_id = opt(m, "groupId", &mut errs, |e, v| check_uuid(e, "groupId", v));
    match (name, owner_name, address, lat, lng) {
        (Some(name), Some(owner_name), Some(address), Some(lat), Some(lng)) if errs.is_empty() => {
            Ok(CreateMobileStore {
                name,
                owner_name,
                phone: match phone {
                    Field::Val(p) => Some(p),
                    _ => None,
                },
                address,
                lat,
                lng,
                group_id,
            })
        }
        _ => Err(errs),
    }
}

/// `ListMobileStoresQueryDto`.
#[derive(Debug, Clone, PartialEq)]
pub struct ListMobileStoresQuery {
    pub search: Option<String>,
    pub group_id: Option<String>,
    pub lat: Option<f64>,
    pub lng: Option<f64>,
    pub radius_km: Option<f64>,
    pub sort: MobileStoreSortField,
    pub page: i64,
    pub limit: i64,
}

fn is_blank(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) => s.is_empty(),
        _ => false,
    }
}

/// `@Transform(toFloat) @IsOptional() @IsNumber() @Min(min) @Max(max)`.
fn query_float(
    m: &Map<String, Value>,
    key: &str,
    min: f64,
    max: f64,
    errs: &mut Vec<String>,
) -> Option<f64> {
    let v = m.get(key);
    if is_blank(v) {
        return None;
    }
    let n = v
        .map(|v| parse_float(&js_to_string(v)))
        .filter(|x| x.is_finite());
    check_number(errs, key, n, Some(min), Some(max))
}

pub fn parse_list_mobile_stores(
    m: &Map<String, Value>,
) -> Result<ListMobileStoresQuery, Vec<String>> {
    let mut errs = Vec::new();
    let search = opt_string(m, "search", &mut errs);
    let group_id = present(m, "groupId").and_then(|v| check_uuid(&mut errs, "groupId", Some(v)));
    let lat = query_float(m, "lat", -90.0, 90.0, &mut errs);
    let lng = query_float(m, "lng", -180.0, 180.0, &mut errs);
    let radius_km = query_float(m, "radiusKm", 0.1, 50.0, &mut errs);
    let sort = opt_enum(
        m,
        "sort",
        &crate::enum_values!(MobileStoreSortField),
        MobileStoreSortField::name,
        &mut errs,
    );
    // `toInt(params, fallback)`: kosong → bawaan, selain itu `parseInt`.
    let mut paging = m.clone();
    for k in ["page", "limit"] {
        if is_blank(paging.get(k)) {
            paging.remove(k);
        }
    }
    let (page, limit) = page_limit(&paging, &mut errs);
    if !errs.is_empty() {
        return Err(errs);
    }
    Ok(ListMobileStoresQuery {
        search,
        group_id,
        lat,
        lng,
        radius_km,
        sort,
        page,
        limit,
    })
}

/// `ResolveStoreQrDto`.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolveStoreQr {
    pub qr_token: String,
    pub lat: Option<f64>,
    pub lng: Option<f64>,
}

pub fn parse_resolve_qr(m: &Map<String, Value>) -> Result<ResolveStoreQr, Vec<String>> {
    let mut errs = Vec::new();
    let token = match m.get("qrToken") {
        Some(Value::String(s)) => Some(s.trim().to_lowercase()),
        _ => None,
    };
    let ok = token.as_deref().is_some_and(|t| {
        t.len() == 64
            && t.bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    });
    if !ok {
        errs.push(QR_MESSAGE.to_owned());
    }
    if token.is_none() {
        errs.push("qrToken must be a string".to_owned());
    }
    let mut coord = |key: &str, lim: f64| -> Option<f64> {
        let v = present(m, key)?;
        check_number(&mut errs, key, as_number(Some(v)), Some(-lim), Some(lim))
    };
    let lat = coord("lat", 90.0);
    let lng = coord("lng", 180.0);
    match token {
        Some(qr_token) if errs.is_empty() => Ok(ResolveStoreQr { qr_token, lat, lng }),
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
    fn code_and_phone_patterns() {
        assert!(store_code_ok("MJ-001"));
        assert!(store_code_ok("ABCDEF-ZZZZZ9"));
        assert!(!store_code_ok("M-001"));
        assert!(!store_code_ok("MJ001"));
        assert!(!store_code_ok("mj-001"));
        assert!(phone_ok("0812-3456 7890"));
        assert!(!phone_ok("0812abc4567"));
        assert!(!phone_ok("0812"));
    }

    #[test]
    fn create_store_messages_in_decorator_order() {
        let e = parse_create_store(&obj(json!({}))).unwrap_err();
        assert_eq!(
            e,
            vec![
                CODE_MESSAGE,
                "code must be a string",
                "name must be shorter than or equal to 120 characters",
                "name must be longer than or equal to 3 characters",
                "name must be a string",
                "ownerName must be shorter than or equal to 80 characters",
                "ownerName must be longer than or equal to 3 characters",
                "ownerName must be a string",
                "address must be shorter than or equal to 255 characters",
                "address must be longer than or equal to 10 characters",
                "address must be a string",
                "lat must not be greater than 90",
                "lat must not be less than -90",
                "lat must be a number conforming to the specified constraints",
                "lng must not be greater than 180",
                "lng must not be less than -180",
                "lng must be a number conforming to the specified constraints",
            ]
        );
    }

    #[test]
    fn create_store_uppercases_code_and_accepts_minimal() {
        let ok = parse_create_store(&obj(json!({
            "code": "mj-001", "name": "Toko", "ownerName": "Pak", "address": "Jl. Kaliurang 5",
            "lat": -7.75, "lng": 110.29, "qrToken": "ignored", "totalVisits": 9
        })))
        .unwrap();
        assert_eq!(ok.code, Field::Val("MJ-001".into()));
        assert_eq!(ok.group_id, Field::Absent);
        let e = parse_create_store(&obj(json!({
            "code": "MJ-001", "name": "Toko", "ownerName": "Pak", "address": "Jl. Kaliurang 5",
            "lat": 91, "lng": 0, "phone": "08xx-abc", "radiusMeters": 10, "groupId": "nope"
        })))
        .unwrap_err();
        assert_eq!(
            e,
            vec![
                PHONE_MESSAGE,
                "lat must not be greater than 90",
                "radiusMeters must not be less than 20",
                "groupId must be a UUID",
            ]
        );
    }

    #[test]
    fn update_store_is_partial_and_nullable() {
        let u = parse_update_store(&obj(json!({ "groupId": null, "radiusMeters": null }))).unwrap();
        assert_eq!(u.group_id, Field::Null);
        assert_eq!(u.radius_meters, Field::Null);
        assert_eq!(u.name, Field::Absent);
        let e = parse_update_store(&obj(json!({ "code": "bad" , "lat": 100}))).unwrap_err();
        assert_eq!(e, vec![CODE_MESSAGE, "lat must not be greater than 90"]);
    }

    #[test]
    fn list_query() {
        let q =
            parse_list_stores(&obj(json!({ "groupId": "", "isActive": "false" })), false).unwrap();
        assert_eq!(q.group_id, None);
        assert_eq!(q.is_active, Some(false));
        assert_eq!(q.sort, AdminStoreSortField::name);
        assert_eq!((q.page, q.limit), (1, 20));
        let ids = (0..51)
            .map(|_| "4d1a2b3c-5e6f-4a7b-8c9d-0e1f2a3b4c5d")
            .collect::<Vec<_>>()
            .join(",");
        let e = parse_list_stores(
            &obj(json!({ "groupId": ids, "sort": "x", "isActive": "yes" })),
            false,
        )
        .unwrap_err();
        assert_eq!(
            e,
            vec![
                "isActive must be a boolean value",
                "groupId must contain no more than 50 elements",
                "sort must be one of the following values: name, code, createdAt, lastVisitAt",
            ]
        );
        let e = parse_list_stores(&obj(json!({ "format": "pdf", "page": "x" })), true).unwrap_err();
        assert_eq!(
            e,
            vec!["format must be one of the following values: csv, xlsx"]
        );
    }

    #[test]
    fn bulk_dto() {
        let u = "4d1a2b3c-5e6f-4a7b-8c9d-0e1f2a3b4c5d";
        let ok = parse_bulk(
            &obj(json!({ "storeUuids": [u], "groupId": null })),
            "storeUuids",
            "groupId",
        )
        .unwrap();
        assert_eq!(ok.target, None);
        let e = parse_bulk(&obj(json!({ "storeUuids": [] })), "storeUuids", "groupId").unwrap_err();
        assert_eq!(
            e,
            vec![
                "storeUuids must contain at least 1 elements",
                "groupId must be a UUID"
            ]
        );
        let e = parse_bulk(&obj(json!({ "customerId": u })), "storeIds", "customerId").unwrap_err();
        assert_eq!(
            e,
            vec![
                "each value in storeIds must be a UUID",
                "storeIds must contain no more than 500 elements",
                "storeIds must contain at least 1 elements",
                "storeIds must be an array",
            ]
        );
    }

    #[test]
    fn mobile_create_and_list() {
        let ok = parse_create_mobile_store(&obj(json!({
            "name": "Toko Makmur", "ownerName": "Pak", "address": "Jl. Kaliurang 5",
            "lat": -7.7, "lng": 110.2, "code": "X", "radiusMeters": 9, "isActive": false
        })))
        .unwrap();
        assert_eq!(ok.phone, None);
        let e = parse_create_mobile_store(&obj(json!({
            "name": "To", "ownerName": "Pak", "address": "Jl. Kaliurang 5", "lat": 99, "lng": 0, "phone": "abcdefgh"
        })))
        .unwrap_err();
        assert_eq!(
            e,
            vec![
                "name must be longer than or equal to 3 characters",
                PHONE_MESSAGE,
                "lat must not be greater than 90"
            ]
        );
        let q =
            parse_list_mobile_stores(&obj(json!({ "lat": "", "page": "", "limit": "5" }))).unwrap();
        assert_eq!((q.lat, q.page, q.limit), (None, 1, 5));
        let e = parse_list_mobile_stores(&obj(
            json!({ "radiusKm": "51", "limit": "101", "sort": "far" }),
        ))
        .unwrap_err();
        assert_eq!(
            e,
            vec![
                "radiusKm must not be greater than 50",
                "sort must be one of the following values: name, code, lastVisit, distance",
                "limit must not be greater than 100",
            ]
        );
        assert!(parse_list_mobile_stores(&obj(json!({ "radiusKm": "50" }))).is_ok());
        let e = parse_list_mobile_stores(&obj(json!({ "lat": "abc" }))).unwrap_err();
        assert_eq!(e.len(), 3);
    }

    #[test]
    fn resolve_qr_dto() {
        let t = "A".repeat(64);
        let ok = parse_resolve_qr(&obj(json!({ "qrToken": format!("  {t} ") }))).unwrap();
        assert_eq!(ok.qr_token, "a".repeat(64));
        let e = parse_resolve_qr(&obj(json!({ "qrToken": 5, "lat": 100 }))).unwrap_err();
        assert_eq!(
            e,
            vec![
                QR_MESSAGE,
                "qrToken must be a string",
                "lat must not be greater than 90"
            ]
        );
        let e = parse_resolve_qr(&obj(json!({ "qrToken": "g".repeat(64) }))).unwrap_err();
        assert_eq!(e, vec![QR_MESSAGE]);
    }
}
