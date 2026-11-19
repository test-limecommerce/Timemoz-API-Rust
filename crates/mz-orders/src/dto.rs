//! DTO modul order (`src/orders/dto/**`) — validasi gaya class-validator (`ValidationPipe({ transform:
//! true })`): properti urut deklarasi, pesan per properti dari dekorator BAWAH ke ATAS, anak
//! `@ValidateNested` = `parent.index.child` (konstrain induk dibuang bila ada galat anak).

use std::sync::LazyLock;

use mz_core::qs::{self, QVal};
use mz_products::dto::{
    DATE_ONLY, Field, check_enum, check_matches, check_max_string, check_uuid4, page_limit_max,
    parse_int_value, present, re,
};
use mz_products::products::parse_cf;
use regex::Regex;
use serde_json::{Map, Value};

pub const ORDER_STATUSES: &[&str] = &[
    "draft",
    "pending_approval",
    "approved",
    "rejected",
    "cancelled",
    "delivered",
];
pub const ORDER_SOURCES: &[&str] = &["web", "mobile"];
pub const ORDER_SORT_FIELDS: &[&str] = &["orderDate", "grandTotal", "dueDate", "createdAt"];
pub const DELIVERY_LIST_STATUSES: &[&str] = &["approved", "delivered"];
/// `MAX_ORDER_ITEMS_PER_REQUEST`.
pub const MAX_ORDER_ITEMS_PER_REQUEST: usize = 200;

pub static ITEM_QTY: LazyLock<Regex> = LazyLock::new(|| re(r"^\d{1,14}\.\d{1,4}$|^\d{1,14}$"));
pub static ITEM_MONEY: LazyLock<Regex> = LazyLock::new(|| re(r"^\d{1,16}\.\d{1,2}$|^\d{1,16}$"));
pub static DISCOUNT_PERCENT: LazyLock<Regex> =
    LazyLock::new(|| re(r"^(100|100\.0|100\.00|\d{1,2}|\d{1,2}\.\d|\d{1,2}\.\d\d)$"));

// ───────────────────────── pembantu ─────────────────────────

/// `@IsOptional() @IsUUID('4')` (`null`/absen → `None`).
fn opt_uuid(
    m: &Map<String, Value>,
    e: &mut Vec<String>,
    prefix: &str,
    key: &str,
) -> Option<String> {
    present(m, key).and_then(|v| check_uuid4(e, &format!("{prefix}{key}"), Some(v)))
}

/// `@IsUUID('4')` wajib.
fn req_uuid(
    m: &Map<String, Value>,
    e: &mut Vec<String>,
    prefix: &str,
    key: &str,
) -> Option<String> {
    check_uuid4(e, &format!("{prefix}{key}"), m.get(key))
}

/// `@IsOptional() @IsString() @MaxLength(n)`.
fn opt_max(
    m: &Map<String, Value>,
    e: &mut Vec<String>,
    prefix: &str,
    key: &str,
    max: usize,
) -> Option<String> {
    present(m, key).and_then(|v| check_max_string(e, &format!("{prefix}{key}"), Some(v), max))
}

/// `@IsString() @Matches(re, { message })` (`message` diawali nama properti).
fn matches_field(
    v: Option<&Value>,
    e: &mut Vec<String>,
    prefix: &str,
    key: &str,
    pattern: &Regex,
    tail: &str,
) -> Option<String> {
    check_matches(
        e,
        &format!("{prefix}{key}"),
        v,
        pattern,
        &format!("{prefix}{key} {tail}"),
    )
}

/// `@IsOptional() @IsObject()`.
fn opt_object(
    m: &Map<String, Value>,
    e: &mut Vec<String>,
    prefix: &str,
    key: &str,
) -> Option<Map<String, Value>> {
    match present(m, key)? {
        Value::Object(o) => Some(o.clone()),
        _ => {
            e.push(format!("{prefix}{key} must be an object"));
            None
        }
    }
}

/// `@IsOptional() @IsBoolean()`.
fn opt_bool(m: &Map<String, Value>, e: &mut Vec<String>, prefix: &str, key: &str) -> Option<bool> {
    match present(m, key)? {
        Value::Bool(b) => Some(*b),
        _ => {
            e.push(format!("{prefix}{key} must be a boolean value"));
            None
        }
    }
}

/// `@IsInt()` (angka JSON bulat).
fn check_int(e: &mut Vec<String>, key: &str, v: Option<&Value>) -> Option<i64> {
    let n = match v {
        Some(Value::Number(n)) => n.as_f64().filter(|x| x.is_finite() && x.fract() == 0.0),
        _ => None,
    };
    if n.is_none() {
        e.push(format!("{key} must be an integer number"));
    }
    #[allow(clippy::cast_possible_truncation)]
    n.map(|x| x as i64)
}

/// `typeof v === 'string' ? v.trim() : v`.
fn trimmed(v: &Value) -> Value {
    match v {
        Value::String(s) => Value::String(mz_core::jsfmt::trim(s).to_owned()),
        other => other.clone(),
    }
}

/// `@IsString() @IsNotEmpty() @MaxLength(255)` (pesan MaxLength, IsNotEmpty, IsString).
fn required_note(e: &mut Vec<String>, v: Option<&Value>) -> Option<String> {
    let s = v.and_then(Value::as_str);
    let before = e.len();
    if s.is_none_or(|s| mz_core::extract::validator_js_length(s) > 255) {
        e.push("note must be shorter than or equal to 255 characters".into());
    }
    let empty = match v {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) => s.is_empty(),
        _ => false,
    };
    if empty {
        e.push("note should not be empty".into());
    }
    if s.is_none() {
        e.push("note must be a string".into());
    }
    s.filter(|_| e.len() == before).map(str::to_owned)
}

/// Array `@ValidateNested({ each })` dengan `@ArrayMaxSize(max)` (+ `min` opsional, + `@IsArray()`).
/// `Ok(None)` = absen/`null` dan opsional.
fn nested_array<T>(
    v: Option<&Value>,
    e: &mut Vec<String>,
    key: &str,
    optional: bool,
    is_array_check: bool,
    min: Option<usize>,
    mut each: impl FnMut(&Map<String, Value>, &mut Vec<String>, &str) -> Option<T>,
) -> Option<Vec<T>> {
    if optional && matches!(v, None | Some(Value::Null)) {
        return None;
    }
    let max_msg = format!("{key} must contain no more than {MAX_ORDER_ITEMS_PER_REQUEST} elements");
    let min_msg = min.map(|n| format!("{key} must contain at least {n} elements"));
    let Some(Value::Array(items)) = v else {
        e.push(max_msg);
        if let Some(m) = min_msg {
            e.push(m);
        }
        if is_array_check {
            e.push(format!("{key} must be an array"));
        }
        if !matches!(v, None | Some(Value::Object(_))) {
            e.push(format!(
                "nested property {key} must be either object or array"
            ));
        }
        return None;
    };
    let mut child = Vec::new();
    let mut out = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let Value::Object(o) = item else {
            child.push(format!(
                "{key}.{i}.nested property {i} must be either object or array"
            ));
            continue;
        };
        if let Some(t) = each(o, &mut child, &format!("{key}.{i}.")) {
            out.push(t);
        }
    }
    if !child.is_empty() {
        e.extend(child);
        return None;
    }
    let before = e.len();
    if items.len() > MAX_ORDER_ITEMS_PER_REQUEST {
        e.push(max_msg);
    }
    if let (Some(n), Some(m)) = (min, min_msg)
        && items.len() < n
    {
        e.push(m);
    }
    (e.len() == before).then_some(out)
}

// ───────────────────────── create / update order ─────────────────────────

/// `CreateOrderDto` (juga header `CreateMobileOrderDto`). `null` opsional = absen.
#[derive(Debug, Clone, Default)]
pub struct CreateOrder {
    pub client_request_id: String,
    pub order_type_id: String,
    pub customer_id: Option<String>,
    pub store_id: Option<String>,
    pub store_visit_id: Option<String>,
    pub owner_id: Option<String>,
    pub order_date: String,
    pub requested_delivery_date: Option<String>,
    pub note: Option<String>,
    pub order_payment_term_id: Option<String>,
    pub order_payment_method_id: Option<String>,
    pub payment_reference: Option<String>,
    pub custom_fields: Option<Map<String, Value>>,
}

fn date_field(
    m: &Map<String, Value>,
    e: &mut Vec<String>,
    key: &str,
    required: bool,
) -> Option<String> {
    let v = if required {
        m.get(key)
    } else {
        present(m, key)
    };
    if !required && v.is_none() {
        return None;
    }
    check_matches(
        e,
        key,
        v,
        &DATE_ONLY,
        &format!("{key} must be a YYYY-MM-DD date"),
    )
}

fn parse_create_header(
    m: &Map<String, Value>,
    e: &mut Vec<String>,
    with_owner: bool,
) -> CreateOrder {
    let client_request_id = req_uuid(m, e, "", "clientRequestId");
    let order_type_id = req_uuid(m, e, "", "orderTypeId");
    let customer_id = opt_uuid(m, e, "", "customerId");
    let store_id = opt_uuid(m, e, "", "storeId");
    let store_visit_id = opt_uuid(m, e, "", "storeVisitId");
    let owner_id = if with_owner {
        opt_uuid(m, e, "", "ownerId")
    } else {
        None
    };
    let order_date = date_field(m, e, "orderDate", true);
    let requested_delivery_date = date_field(m, e, "requestedDeliveryDate", false);
    let note = opt_max(m, e, "", "note", 2000);
    let order_payment_term_id = opt_uuid(m, e, "", "orderPaymentTermId");
    let order_payment_method_id = opt_uuid(m, e, "", "orderPaymentMethodId");
    let payment_reference = opt_max(m, e, "", "paymentReference", 64);
    let custom_fields = opt_object(m, e, "", "customFields");
    CreateOrder {
        client_request_id: client_request_id.unwrap_or_default(),
        order_type_id: order_type_id.unwrap_or_default(),
        customer_id,
        store_id,
        store_visit_id,
        owner_id,
        order_date: order_date.unwrap_or_default(),
        requested_delivery_date,
        note,
        order_payment_term_id,
        order_payment_method_id,
        payment_reference,
        custom_fields,
    }
}

/// `CreateOrderDto`.
pub fn parse_create(m: &Map<String, Value>) -> Result<CreateOrder, Vec<String>> {
    let mut e = Vec::new();
    let d = parse_create_header(m, &mut e, true);
    if e.is_empty() { Ok(d) } else { Err(e) }
}

/// `UpdateOrderDto` — tiga keadaan per properti (absen / `null` / nilai).
#[derive(Debug, Clone, Default)]
pub struct UpdateOrder {
    pub customer_id: Field<String>,
    pub store_id: Field<String>,
    pub store_visit_id: Field<String>,
    pub order_type_id: Field<String>,
    pub owner_id: Field<String>,
    pub order_date: Field<String>,
    pub requested_delivery_date: Field<String>,
    pub note: Field<String>,
    pub order_payment_term_id: Field<String>,
    pub order_payment_method_id: Field<String>,
    pub payment_reference: Field<String>,
    /// `customFields`: `Val(Value)` (objek) / `Null`.
    pub custom_fields: Field<Map<String, Value>>,
}

fn tri<T>(m: &Map<String, Value>, key: &str, check: impl FnOnce(&Value) -> Option<T>) -> Field<T> {
    match m.get(key) {
        None => Field::Absent,
        Some(Value::Null) => Field::Null,
        Some(v) => check(v).map_or(Field::Absent, Field::Val),
    }
}

/// `UpdateOrderDto`.
pub fn parse_update(m: &Map<String, Value>) -> Result<UpdateOrder, Vec<String>> {
    let mut e = Vec::new();
    let mut u = UpdateOrder::default();
    let uuid = |e: &mut Vec<String>, key: &str| tri(m, key, |v| check_uuid4(e, key, Some(v)));
    u.customer_id = uuid(&mut e, "customerId");
    u.store_id = uuid(&mut e, "storeId");
    u.store_visit_id = uuid(&mut e, "storeVisitId");
    u.order_type_id = uuid(&mut e, "orderTypeId");
    u.owner_id = uuid(&mut e, "ownerId");
    u.order_date = tri(m, "orderDate", |v| {
        check_matches(
            &mut e,
            "orderDate",
            Some(v),
            &DATE_ONLY,
            "orderDate must be a YYYY-MM-DD date",
        )
    });
    u.requested_delivery_date = tri(m, "requestedDeliveryDate", |v| {
        check_matches(
            &mut e,
            "requestedDeliveryDate",
            Some(v),
            &DATE_ONLY,
            "requestedDeliveryDate must be a YYYY-MM-DD date",
        )
    });
    u.note = tri(m, "note", |v| {
        check_max_string(&mut e, "note", Some(v), 2000)
    });
    u.order_payment_term_id = uuid(&mut e, "orderPaymentTermId");
    u.order_payment_method_id = uuid(&mut e, "orderPaymentMethodId");
    u.payment_reference = tri(m, "paymentReference", |v| {
        check_max_string(&mut e, "paymentReference", Some(v), 64)
    });
    u.custom_fields = tri(m, "customFields", |v| match v {
        Value::Object(o) => Some(o.clone()),
        _ => {
            e.push("customFields must be an object".into());
            None
        }
    });
    if e.is_empty() { Ok(u) } else { Err(e) }
}

// ───────────────────────── items ─────────────────────────

/// `OrderItemInputDto` (+ `unitPrice` dari `MobileOrderItemInputDto`).
#[derive(Debug, Clone, Default)]
pub struct ItemInput {
    pub product_id: String,
    pub uom_id: String,
    pub qty: String,
    pub discount_percent: Option<String>,
    pub discount_amount: Option<String>,
    pub is_free_good: Option<bool>,
    pub note: Option<String>,
    pub custom_fields: Option<Map<String, Value>>,
    pub unit_price: Option<String>,
}

fn parse_item(
    o: &Map<String, Value>,
    e: &mut Vec<String>,
    p: &str,
    mobile: bool,
) -> Option<ItemInput> {
    let before = e.len();
    let product_id = req_uuid(o, e, p, "productId");
    let uom_id = req_uuid(o, e, p, "uomId");
    let qty = matches_field(
        o.get("qty"),
        e,
        p,
        "qty",
        &ITEM_QTY,
        "must be a positive number with up to 4 decimal places",
    );
    let discount_percent = present(o, "discountPercent").and_then(|v| {
        matches_field(
            Some(v),
            e,
            p,
            "discountPercent",
            &DISCOUNT_PERCENT,
            "must be 0-100 with up to 2 decimal places",
        )
    });
    let discount_amount = present(o, "discountAmount").and_then(|v| {
        matches_field(
            Some(v),
            e,
            p,
            "discountAmount",
            &ITEM_MONEY,
            "must be a non-negative number with up to 2 decimal places",
        )
    });
    let is_free_good = opt_bool(o, e, p, "isFreeGood");
    let note = opt_max(o, e, p, "note", 255);
    let custom_fields = opt_object(o, e, p, "customFields");
    let unit_price = if mobile {
        present(o, "unitPrice").and_then(|v| {
            matches_field(
                Some(v),
                e,
                p,
                "unitPrice",
                &ITEM_MONEY,
                "must be a non-negative number with up to 2 decimal places",
            )
        })
    } else {
        None
    };
    (e.len() == before).then(|| ItemInput {
        product_id: product_id.unwrap_or_default(),
        uom_id: uom_id.unwrap_or_default(),
        qty: qty.unwrap_or_default(),
        discount_percent,
        discount_amount,
        is_free_good,
        note,
        custom_fields,
        unit_price,
    })
}

/// `ReplaceOrderItemsDto`.
pub fn parse_replace_items(m: &Map<String, Value>) -> Result<Vec<ItemInput>, Vec<String>> {
    let mut e = Vec::new();
    let items = nested_array(
        m.get("items"),
        &mut e,
        "items",
        false,
        false,
        None,
        |o, e, p| parse_item(o, e, p, false),
    );
    match items {
        Some(items) if e.is_empty() => Ok(items),
        _ => Err(e),
    }
}

/// `CreateMobileOrderDto` → header (tanpa `ownerId`) + item.
pub fn parse_create_mobile(
    m: &Map<String, Value>,
) -> Result<(CreateOrder, Vec<ItemInput>), Vec<String>> {
    let mut e = Vec::new();
    let header = parse_create_header(m, &mut e, false);
    let items = nested_array(
        m.get("items"),
        &mut e,
        "items",
        false,
        false,
        Some(1),
        |o, e, p| parse_item(o, e, p, true),
    );
    match items {
        Some(items) if e.is_empty() => Ok((header, items)),
        _ => Err(e),
    }
}

// ───────────────────────── list / export ─────────────────────────

/// Filter bersama `ListOrdersQueryDto` / `ExportOrdersQueryDto` (`buildWhere`).
#[derive(Debug, Clone, Default)]
pub struct OrderFilter {
    pub status: Option<Vec<String>>,
    pub customer_id: Option<String>,
    pub store_id: Option<String>,
    pub store_visit_id: Option<String>,
    pub owner_id: Option<String>,
    pub order_type_id: Option<String>,
    pub store_group_id: Option<String>,
    pub date_from: Option<String>,
    pub date_to: Option<String>,
    pub due_before: Option<String>,
    pub q: Option<String>,
    pub source: Option<String>,
    /// `cf[key]=value` (urutan `Object.keys`); nilai bukan string tunggal = `None`.
    pub cf: Vec<(String, Option<String>)>,
}

/// `ListOrdersQueryDto` (+ `format`/`level` untuk export).
#[derive(Debug, Clone)]
pub struct ListQuery {
    pub filter: OrderFilter,
    pub sort_by: String,
    pub sort_dir: String,
    pub page: i64,
    pub limit: i64,
    pub format: String,
    pub level: String,
}

fn qv_to_json(v: &QVal) -> Value {
    match v {
        QVal::One(s) => Value::String(s.clone()),
        QVal::Many(items) => Value::Array(items.iter().cloned().map(Value::String).collect()),
        QVal::Obj => Value::Object(Map::new()),
    }
}

/// `@Transform(wrap) @IsIn/IsEnum(each) @ArrayMaxSize(max) @IsOptional()`.
fn status_array(
    m: &Map<String, Value>,
    e: &mut Vec<String>,
    allowed: &[&str],
    max: usize,
) -> Option<Vec<String>> {
    let v = present(m, "status")?;
    let arr: Vec<Value> = match v {
        Value::Array(a) => a.clone(),
        other => vec![other.clone()],
    };
    let before = e.len();
    if arr.len() > max {
        e.push(format!("status must contain no more than {max} elements"));
    }
    if !arr
        .iter()
        .all(|x| x.as_str().is_some_and(|s| allowed.contains(&s)))
    {
        e.push(format!(
            "each value in status must be one of the following values: {}",
            allowed.join(", ")
        ));
    }
    (e.len() == before).then(|| {
        arr.iter()
            .filter_map(|x| x.as_str().map(str::to_owned))
            .collect()
    })
}

/// Query list (`export = false`) atau export (`true`: tanpa page/limit, + `format`/`level`).
pub fn parse_list(raw: &str, export: bool) -> Result<ListQuery, Vec<String>> {
    let m: Map<String, Value> = qs::parse(raw)
        .iter()
        .map(|(k, v)| (k.clone(), qv_to_json(v)))
        .collect();
    let mut e = Vec::new();
    let mut f = OrderFilter {
        status: status_array(&m, &mut e, ORDER_STATUSES, 6),
        ..Default::default()
    };
    f.customer_id = opt_uuid(&m, &mut e, "", "customerId");
    f.store_id = opt_uuid(&m, &mut e, "", "storeId");
    f.store_visit_id = opt_uuid(&m, &mut e, "", "storeVisitId");
    f.owner_id = opt_uuid(&m, &mut e, "", "ownerId");
    f.order_type_id = opt_uuid(&m, &mut e, "", "orderTypeId");
    f.store_group_id = opt_uuid(&m, &mut e, "", "storeGroupId");
    f.date_from = date_field(&m, &mut e, "dateFrom", false);
    f.date_to = date_field(&m, &mut e, "dateTo", false);
    f.due_before = date_field(&m, &mut e, "dueBefore", false);
    f.q = opt_max(&m, &mut e, "", "q", 120);
    f.source =
        present(&m, "source").and_then(|v| check_enum(&mut e, "source", Some(v), ORDER_SOURCES));
    match parse_cf(raw) {
        (_, Ok(pairs)) => f.cf = pairs,
        (_, Err(())) => e.push("cf must be an object".into()),
    }
    let sort_by = present(&m, "sortBy")
        .and_then(|v| check_enum(&mut e, "sortBy", Some(v), ORDER_SORT_FIELDS))
        .unwrap_or_else(|| "orderDate".into());
    let sort_dir = present(&m, "sortDir")
        .and_then(|v| check_enum(&mut e, "sortDir", Some(v), &["asc", "desc"]))
        .unwrap_or_else(|| "desc".into());
    let (mut page, mut limit) = (1, 20);
    let (mut format, mut level) = ("xlsx".to_owned(), "order".to_owned());
    if export {
        if let Some(v) = present(&m, "format") {
            format = check_enum(&mut e, "format", Some(v), &["csv", "xlsx"]).unwrap_or(format);
        }
        if let Some(v) = present(&m, "level") {
            level = check_enum(&mut e, "level", Some(v), &["order", "item"]).unwrap_or(level);
        }
    } else {
        (page, limit) = page_limit_max(&m, &mut e, 100);
    }
    if e.is_empty() {
        Ok(ListQuery {
            filter: f,
            sort_by,
            sort_dir,
            page,
            limit,
            format,
            level,
        })
    } else {
        Err(e)
    }
}

/// `ListOrderOwnersQueryDto` → `(search, page, limit)`.
pub fn parse_owners(m: &Map<String, Value>) -> Result<(Option<String>, i64, i64), Vec<String>> {
    let mut e = Vec::new();
    let search = opt_max(m, &mut e, "", "search", 120);
    let (page, limit) = page_limit_max(m, &mut e, 100);
    if e.is_empty() {
        Ok((search, page, limit))
    } else {
        Err(e)
    }
}

// ───────────────────────── transisi ─────────────────────────

/// `CancelOrderDto` → `(reasonId, note)`.
pub fn parse_cancel(
    m: &Map<String, Value>,
) -> Result<(Option<String>, Option<String>), Vec<String>> {
    let mut e = Vec::new();
    let reason = opt_uuid(m, &mut e, "", "reasonId");
    let note = opt_max(m, &mut e, "", "note", 255);
    if e.is_empty() {
        Ok((reason, note))
    } else {
        Err(e)
    }
}

/// `RejectOrderDto` → `note`.
pub fn parse_reject(m: &Map<String, Value>) -> Result<String, Vec<String>> {
    let mut e = Vec::new();
    let note = required_note(&mut e, m.get("note"));
    match note {
        Some(n) if e.is_empty() => Ok(n),
        _ => Err(e),
    }
}

/// `CreateStoreVisitNoOrderDto` → `(reasonId, note)`.
pub fn parse_no_order(m: &Map<String, Value>) -> Result<(String, Option<String>), Vec<String>> {
    let mut e = Vec::new();
    let reason = req_uuid(m, &mut e, "", "reasonId");
    let note = opt_max(m, &mut e, "", "note", 255);
    match reason {
        Some(r) if e.is_empty() => Ok((r, note)),
        _ => Err(e),
    }
}

/// `cursor` + `limit` (`@Max(200)`, bawaan 50) mobile.
fn cursor_limit(m: &Map<String, Value>, e: &mut Vec<String>) -> (Option<String>, i64) {
    let cursor = opt_max(m, e, "", "cursor", 64);
    let mut limit = 50;
    if let Some(v) = m.get("limit") {
        #[allow(clippy::cast_precision_loss)]
        let n = parse_int_value(v).map(|n| n as f64);
        if let Some(i) = mz_core::extract::check_int_range(e, "limit", n, 1, 200) {
            limit = i;
        }
    }
    (cursor, limit)
}

/// `MobileOrdersQueryDto` → `(cursor, limit)`.
pub fn parse_mobile_list(m: &Map<String, Value>) -> Result<(Option<String>, i64), Vec<String>> {
    let mut e = Vec::new();
    let r = cursor_limit(m, &mut e);
    if e.is_empty() { Ok(r) } else { Err(e) }
}

/// `MobileOrderDeliveriesQueryDto` → `(cursor, limit, status)`.
#[allow(clippy::type_complexity)]
pub fn parse_mobile_deliveries(
    m: &Map<String, Value>,
) -> Result<(Option<String>, i64, Option<Vec<String>>), Vec<String>> {
    let mut e = Vec::new();
    let (cursor, limit) = cursor_limit(m, &mut e);
    let status = status_array(m, &mut e, DELIVERY_LIST_STATUSES, 2);
    if e.is_empty() {
        Ok((cursor, limit, status))
    } else {
        Err(e)
    }
}

// ───────────────────────── delivery ─────────────────────────

/// `DeliveryLineInputDto`.
#[derive(Debug, Clone)]
pub struct DeliveryLine {
    pub item_id: String,
    pub delivered_qty: String,
    pub reason_id: Option<String>,
    /// `Absent` = tidak dikirim; `Null`/`Val` = eksplisit (menang atas nilai tersimpan).
    pub note: Field<String>,
}

fn parse_line(o: &Map<String, Value>, e: &mut Vec<String>, p: &str) -> Option<DeliveryLine> {
    let before = e.len();
    let item_id = req_uuid(o, e, p, "itemId");
    let delivered_qty = matches_field(
        o.get("deliveredQty"),
        e,
        p,
        "deliveredQty",
        &ITEM_QTY,
        "must be a non-negative number with up to 4 decimal places",
    );
    let reason_id = opt_uuid(o, e, p, "reasonId");
    let note = match o.get("note") {
        None => Field::Absent,
        Some(Value::Null) => Field::Null,
        Some(v) => check_max_string(e, &format!("{p}note"), Some(&trimmed(v)), 255)
            .map_or(Field::Absent, Field::Val),
    };
    (e.len() == before).then(|| DeliveryLine {
        item_id: item_id.unwrap_or_default(),
        delivered_qty: delivered_qty.unwrap_or_default(),
        reason_id,
        note,
    })
}

/// `DeliverOrderDto`.
#[derive(Debug, Clone)]
pub struct DeliverOrder {
    pub client_request_id: String,
    pub delivered_at: i64,
    pub note: Option<String>,
    pub lines: Vec<DeliveryLine>,
}

pub fn parse_deliver(m: &Map<String, Value>) -> Result<DeliverOrder, Vec<String>> {
    let mut e = Vec::new();
    let client_request_id = req_uuid(m, &mut e, "", "clientRequestId");
    let delivered_at = check_int(&mut e, "deliveredAt", m.get("deliveredAt"));
    let note =
        present(m, "note").and_then(|v| check_max_string(&mut e, "note", Some(&trimmed(v)), 255));
    let lines = nested_array(
        m.get("lines"),
        &mut e,
        "lines",
        true,
        true,
        None,
        parse_line,
    );
    match (client_request_id, delivered_at) {
        (Some(c), Some(d)) if e.is_empty() => Ok(DeliverOrder {
            client_request_id: c,
            delivered_at: d,
            note,
            lines: lines.unwrap_or_default(),
        }),
        _ => Err(e),
    }
}

/// `CorrectOrderDeliveryDto`.
#[derive(Debug, Clone)]
pub struct CorrectDelivery {
    pub expected_revision: i64,
    pub note: String,
    pub delivered_at: Option<i64>,
    pub lines: Vec<DeliveryLine>,
}

pub fn parse_correct(m: &Map<String, Value>) -> Result<CorrectDelivery, Vec<String>> {
    let mut e = Vec::new();
    let rev_raw = m.get("expectedRevision");
    let rev = match rev_raw {
        Some(Value::Number(n)) => n.as_f64().filter(|x| x.is_finite()),
        _ => None,
    };
    if rev.is_none_or(|r| r < 1.0) {
        e.push("expectedRevision must not be less than 1".into());
    }
    let expected = check_int(&mut e, "expectedRevision", rev_raw);
    let note_v = m.get("note").map(trimmed);
    let note = required_note(&mut e, note_v.as_ref());
    let delivered_at =
        present(m, "deliveredAt").and_then(|v| check_int(&mut e, "deliveredAt", Some(v)));
    let lines = nested_array(
        m.get("lines"),
        &mut e,
        "lines",
        true,
        true,
        None,
        parse_line,
    );
    match (expected, note) {
        (Some(r), Some(n)) if e.is_empty() && r >= 1 => Ok(CorrectDelivery {
            expected_revision: r,
            note: n,
            delivered_at,
            lines: lines.unwrap_or_default(),
        }),
        _ => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    const U: &str = "11111111-1111-4111-8111-111111111111";

    // Node: src/orders/dto/list-orders-query.dto.spec.ts
    #[test]
    fn list_status_rules() {
        assert_eq!(
            parse_list("status=draft", false).unwrap().filter.status,
            Some(vec!["draft".into()])
        );
        assert_eq!(
            parse_list("status=draft&status=approved", false)
                .unwrap()
                .filter
                .status,
            Some(vec!["draft".into(), "approved".into()])
        );
        assert_eq!(parse_list("", false).unwrap().filter.status, None);
        assert!(parse_list("status=not-a-status", false).is_err());
        let seven = "status=draft&status=pending_approval&status=approved&status=rejected&status=cancelled&status=delivered&status=draft";
        assert_eq!(
            parse_list(seven, false).unwrap_err(),
            vec!["status must contain no more than 6 elements".to_owned()]
        );
        assert!(parse_list("status=approved&status=delivered", false).is_ok());
        assert!(parse_list(&format!("storeVisitId={U}"), false).is_ok());
        assert_eq!(
            parse_list("storeVisitId=not-a-uuid", false).unwrap_err(),
            vec!["storeVisitId must be a UUID".to_owned()]
        );
        assert_eq!(
            parse_list("source=mobile", false)
                .unwrap()
                .filter
                .source
                .as_deref(),
            Some("mobile")
        );
        assert!(parse_list("source=walk-in", false).is_err());
        let q = parse_list("cf%5Bpo%5D=PO-1&cf[x][y]=1", false).unwrap();
        assert_eq!(
            q.filter.cf,
            vec![("po".into(), Some("PO-1".into())), ("x".into(), None)]
        );
        assert_eq!(
            parse_list("cf=abc", false).unwrap_err(),
            vec!["cf must be an object".to_owned()]
        );
        let ex = parse_list("format=csv&level=item&page=0", true).unwrap();
        assert_eq!((ex.format.as_str(), ex.level.as_str()), ("csv", "item"));
    }

    #[test]
    fn create_messages() {
        let e = parse_create(&obj(json!({"orderDate": "2026-1-1", "note": 5}))).unwrap_err();
        assert_eq!(
            e,
            vec![
                "clientRequestId must be a UUID",
                "orderTypeId must be a UUID",
                "orderDate must be a YYYY-MM-DD date",
                "note must be shorter than or equal to 2000 characters",
                "note must be a string",
            ]
        );
    }

    #[test]
    fn items_messages() {
        assert_eq!(
            parse_replace_items(&obj(json!({}))).unwrap_err(),
            vec!["items must contain no more than 200 elements"]
        );
        assert_eq!(
            parse_replace_items(&obj(json!({"items": "x"}))).unwrap_err(),
            vec![
                "items must contain no more than 200 elements",
                "nested property items must be either object or array"
            ]
        );
        assert!(
            parse_replace_items(&obj(json!({"items": []})))
                .unwrap()
                .is_empty()
        );
        let e = parse_replace_items(&obj(json!({"items": [{"productId": U, "uomId": U, "qty": "0.12345", "discountPercent": "101"}, 3]})))
            .unwrap_err();
        assert_eq!(
            e,
            vec![
                "items.0.qty must be a positive number with up to 4 decimal places",
                "items.0.discountPercent must be 0-100 with up to 2 decimal places",
                "items.1.nested property 1 must be either object or array",
            ]
        );
        let e = parse_create_mobile(&obj(
            json!({"clientRequestId": U, "orderTypeId": U, "orderDate": "2026-01-01", "items": []}),
        ))
        .unwrap_err();
        assert_eq!(e, vec!["items must contain at least 1 elements"]);
    }

    #[test]
    fn transition_dtos() {
        assert_eq!(
            parse_reject(&obj(json!({}))).unwrap_err(),
            vec![
                "note must be shorter than or equal to 255 characters",
                "note should not be empty",
                "note must be a string"
            ]
        );
        assert_eq!(
            parse_reject(&obj(json!({"note": ""}))).unwrap_err(),
            vec!["note should not be empty"]
        );
        assert_eq!(parse_cancel(&obj(json!({}))).unwrap(), (None, None));
        let d = parse_deliver(&obj(json!({"clientRequestId": U, "deliveredAt": 5, "note": "  x ", "lines": [{"itemId": U, "deliveredQty": "1", "note": null}]})))
            .unwrap();
        assert_eq!(d.note.as_deref(), Some("x"));
        assert!(matches!(d.lines[0].note, Field::Null));
        let e = parse_correct(&obj(json!({"expectedRevision": 0, "note": "   "}))).unwrap_err();
        assert_eq!(
            e,
            vec![
                "expectedRevision must not be less than 1",
                "note should not be empty"
            ]
        );
    }
}
