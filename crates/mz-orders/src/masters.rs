//! Port `orders/admin-order-masters.controller.ts`, `order-masters.service.ts`,
//! `order-master.delegate.ts`, `order-master-kind.config.ts`: CRUD empat tabel master order
//! (`orderType`, `orderReason`, `orderPaymentTerm`, `orderPaymentMethod`) di
//! `/v1/admin/order-masters/{kind}`.

use axum::extract::{FromRequestParts, Path, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use chrono::NaiveDateTime;
use mz_audit::{AuditHandle, Lookup, Model, Row};
use mz_core::company::require_company_id;
use mz_core::extract::{ValidatedJson, ValidatedQuery};
use mz_core::gate::guard;
use mz_core::time::now_naive;
use mz_core::{ApiError, ApiResult, AppState, RequestCtx};
use serde_json::{Map, Value, json};
use sqlx::{PgConnection, PgExecutor, Postgres, QueryBuilder};

use crate::common::{
    conflict, internal, like_pattern, ms, ms_opt, not_found, offset, page_meta, row_uuid,
    unprocessable,
};
use mz_products::dto::{
    Field, Raw, check_bool, check_enum, check_length_string, check_matches, check_parse_int_min,
    opt_max_string, page_limit_max, query_bool, re, to_boolean,
};

/// `OrderReasonType`.
pub const ORDER_REASON_TYPES: &[&str] = &["no_order", "order_cancel", "delivery_adjustment"];

/// `:kind` (`OrderMasterKind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MasterKind {
    Types,
    Reasons,
    PaymentTerms,
    PaymentMethods,
}

impl MasterKind {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "types" => Self::Types,
            "reasons" => Self::Reasons,
            "payment-terms" => Self::PaymentTerms,
            "payment-methods" => Self::PaymentMethods,
            _ => return None,
        })
    }
    /// `MODEL_BY_KIND`.
    pub fn table(self) -> &'static str {
        match self {
            Self::Types => "orderType",
            Self::Reasons => "orderReason",
            Self::PaymentTerms => "orderPaymentTerm",
            Self::PaymentMethods => "orderPaymentMethod",
        }
    }
    /// `LABEL_BY_KIND`.
    pub fn label(self) -> &'static str {
        match self {
            Self::Types => "Order type",
            Self::Reasons => "Order reason",
            Self::PaymentTerms => "Order payment term",
            Self::PaymentMethods => "Order payment method",
        }
    }
    pub fn model(self) -> Model {
        match self {
            Self::Types => Model::OrderType,
            Self::Reasons => Model::OrderReason,
            Self::PaymentTerms => Model::OrderPaymentTerm,
            Self::PaymentMethods => Model::OrderPaymentMethod,
        }
    }
    fn has_is_default(self) -> bool {
        !matches!(self, Self::Reasons)
    }
    fn has_sort_order(self) -> bool {
        matches!(self, Self::Reasons)
    }
    fn name_max_length(self) -> usize {
        if matches!(self, Self::Reasons) {
            150
        } else {
            120
        }
    }
    fn extra_fields(self) -> &'static [&'static str] {
        match self {
            Self::Types => &[
                "requiresApproval",
                "allowDiscount",
                "allowFreeItem",
                "numberPrefix",
            ],
            Self::Reasons => &["reasonType"],
            Self::PaymentTerms => &["dueDays"],
            Self::PaymentMethods => &["requiresReference"],
        }
    }
    fn order_by(self) -> &'static str {
        if matches!(self, Self::Reasons) {
            r#""sortOrder" ASC, name ASC, id ASC"#
        } else {
            "name ASC, id ASC"
        }
    }
    fn cols(self) -> &'static str {
        match self {
            Self::Types => concat!(
                r#"id, uuid::text AS uuid, "companyId", code, name, "isActive", "createdAt", "updatedAt", "#,
                r#""requiresApproval", "allowDiscount", "allowFreeItem", "numberPrefix", "isDefault", "#,
                r#"NULL::text AS "reasonType", NULL::int AS "sortOrder", NULL::int AS "dueDays", NULL::bool AS "requiresReference""#
            ),
            Self::Reasons => concat!(
                r#"id, uuid::text AS uuid, "companyId", code, name, "isActive", "createdAt", "updatedAt", "#,
                r#"NULL::bool AS "requiresApproval", NULL::bool AS "allowDiscount", NULL::bool AS "allowFreeItem", NULL::text AS "numberPrefix", NULL::bool AS "isDefault", "#,
                r#""reasonType"::text AS "reasonType", "sortOrder", NULL::int AS "dueDays", NULL::bool AS "requiresReference""#
            ),
            Self::PaymentTerms => concat!(
                r#"id, uuid::text AS uuid, "companyId", code, name, "isActive", "createdAt", "updatedAt", "#,
                r#"NULL::bool AS "requiresApproval", NULL::bool AS "allowDiscount", NULL::bool AS "allowFreeItem", NULL::text AS "numberPrefix", "isDefault", "#,
                r#"NULL::text AS "reasonType", NULL::int AS "sortOrder", "dueDays", NULL::bool AS "requiresReference""#
            ),
            Self::PaymentMethods => {
                concat!(
                    r#"id, uuid::text AS uuid, "companyId", code, name, "isActive", "createdAt", "updatedAt", "#,
                    r#"NULL::bool AS "requiresApproval", NULL::bool AS "allowDiscount", NULL::bool AS "allowFreeItem", NULL::text AS "numberPrefix", "isDefault", "#,
                    r#"NULL::text AS "reasonType", NULL::int AS "sortOrder", NULL::int AS "dueDays", "requiresReference""#
                )
            }
        }
    }
}

/// `ALL_EXTRA_FIELD_NAMES` (urutan config).
const ALL_EXTRA_FIELD_NAMES: [&str; 7] = [
    "requiresApproval",
    "allowDiscount",
    "allowFreeItem",
    "numberPrefix",
    "reasonType",
    "dueDays",
    "requiresReference",
];

/// `OrderMasterRow` (superset kolom; kolom milik kind lain `None`).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct MasterRow {
    pub id: i32,
    pub uuid: String,
    #[sqlx(rename = "companyId")]
    pub company_id: i32,
    pub code: String,
    pub name: String,
    #[sqlx(rename = "isActive")]
    pub is_active: bool,
    #[sqlx(rename = "createdAt")]
    pub created_at: NaiveDateTime,
    #[sqlx(rename = "updatedAt")]
    pub updated_at: Option<NaiveDateTime>,
    #[sqlx(rename = "requiresApproval")]
    pub requires_approval: Option<bool>,
    #[sqlx(rename = "allowDiscount")]
    pub allow_discount: Option<bool>,
    #[sqlx(rename = "allowFreeItem")]
    pub allow_free_item: Option<bool>,
    #[sqlx(rename = "numberPrefix")]
    pub number_prefix: Option<String>,
    #[sqlx(rename = "isDefault")]
    pub is_default: Option<bool>,
    #[sqlx(rename = "reasonType")]
    pub reason_type: Option<String>,
    #[sqlx(rename = "sortOrder")]
    pub sort_order: Option<i32>,
    #[sqlx(rename = "dueDays")]
    pub due_days: Option<i32>,
    #[sqlx(rename = "requiresReference")]
    pub requires_reference: Option<bool>,
}

/// Node: src/orders/order-masters.service.ts:59 `toEntity`.
pub fn to_entity(row: &MasterRow, kind: MasterKind) -> Value {
    let mut m = Map::new();
    m.insert("id".into(), json!(row.uuid));
    m.insert("code".into(), json!(row.code));
    m.insert("name".into(), json!(row.name));
    m.insert("isActive".into(), json!(row.is_active));
    if kind.has_is_default() {
        m.insert("isDefault".into(), json!(row.is_default.unwrap_or(false)));
    }
    match kind {
        MasterKind::Types => {
            m.insert(
                "requiresApproval".into(),
                json!(row.requires_approval.unwrap_or(false)),
            );
            m.insert(
                "allowDiscount".into(),
                json!(row.allow_discount.unwrap_or(true)),
            );
            m.insert(
                "allowFreeItem".into(),
                json!(row.allow_free_item.unwrap_or(false)),
            );
            m.insert("numberPrefix".into(), json!(row.number_prefix));
        }
        MasterKind::Reasons => {
            m.insert("reasonType".into(), json!(row.reason_type));
            m.insert("sortOrder".into(), json!(row.sort_order.unwrap_or(0)));
        }
        MasterKind::PaymentTerms => {
            m.insert("dueDays".into(), json!(row.due_days.unwrap_or(0)));
        }
        MasterKind::PaymentMethods => {
            m.insert(
                "requiresReference".into(),
                json!(row.requires_reference.unwrap_or(false)),
            );
        }
    }
    m.insert("createdAt".into(), json!(ms(row.created_at)));
    m.insert("updatedAt".into(), ms_opt(row.updated_at));
    Value::Object(m)
}

// ───────────────────────── pembaca untuk modul lain ─────────────────────────

/// `findFirst({ where: { uuid, companyId } })` pada tabel `kind` (uuid non-UUID → 500 P2023).
pub async fn find_by_uuid<'e>(
    exec: impl PgExecutor<'e>,
    kind: MasterKind,
    company_id: i32,
    uuid: &str,
) -> ApiResult<Option<MasterRow>> {
    Ok(sqlx::query_as(&format!(
        r#"SELECT {} FROM "{}" WHERE uuid = $1 AND "companyId" = $2 LIMIT 1"#,
        kind.cols(),
        kind.table()
    ))
    .bind(row_uuid(uuid)?)
    .bind(company_id)
    .fetch_optional(exec)
    .await?)
}

/// `findUniqueOrThrow({ where: { id } })`.
pub async fn find_by_id<'e>(
    exec: impl PgExecutor<'e>,
    kind: MasterKind,
    id: i32,
) -> ApiResult<MasterRow> {
    let row: Option<MasterRow> = sqlx::query_as(&format!(
        r#"SELECT {} FROM "{}" WHERE id = $1"#,
        kind.cols(),
        kind.table()
    ))
    .bind(id)
    .fetch_optional(exec)
    .await?;
    row.ok_or_else(|| internal("No record found"))
}

/// `findAll(..., { isActive: true, limit: 100 })` (config order).
pub async fn list_active(
    state: &AppState,
    kind: MasterKind,
    company_id: i32,
) -> ApiResult<Vec<Value>> {
    let rows: Vec<MasterRow> = sqlx::query_as(&format!(
        r#"SELECT {} FROM "{}" WHERE "companyId" = $1 AND "isActive" = true ORDER BY {} LIMIT 100"#,
        kind.cols(),
        kind.table(),
        kind.order_by()
    ))
    .bind(company_id)
    .fetch_all(&state.db)
    .await?;
    Ok(rows.iter().map(|r| to_entity(r, kind)).collect())
}

// ───────────────────────── DTO ─────────────────────────

/// Nilai body yang lolos validasi (create + update).
#[derive(Debug, Default)]
pub struct MasterDto {
    pub code: Field<String>,
    pub name: Field<String>,
    pub is_active: Field<bool>,
    pub requires_approval: Field<bool>,
    pub allow_discount: Field<bool>,
    pub allow_free_item: Field<bool>,
    pub number_prefix: Field<String>,
    pub is_default: Field<bool>,
    pub reason_type: Field<String>,
    pub sort_order: Field<i64>,
    pub due_days: Field<i64>,
    pub requires_reference: Field<bool>,
}

const CODE_MSG: &str = "code must be 1-32 characters of uppercase letters, digits, or underscore";
const PREFIX_MSG: &str = "numberPrefix must be 1-8 uppercase letters or digits";

fn upper(v: &Value) -> Value {
    match v {
        Value::String(s) => Value::String(s.to_uppercase()),
        other => other.clone(),
    }
}

fn opt<T>(m: &Map<String, Value>, key: &str, check: impl FnOnce(&Value) -> Option<T>) -> Field<T> {
    match m.get(key) {
        None => Field::Absent,
        Some(Value::Null) => Field::Null,
        Some(v) => check(v).map_or(Field::Absent, Field::Val),
    }
}

/// `CreateOrderMasterDto` (`create`) / `UpdateOrderMasterDto`.
pub fn parse_dto(m: &Map<String, Value>, create: bool) -> Result<MasterDto, Vec<String>> {
    let code_re = re(r"^[A-Z0-9_]{1,32}$");
    let prefix_re = re(r"^[A-Z0-9]{1,8}$");
    let mut e = Vec::new();
    let code = if create {
        let v = m.get("code").map(upper);
        check_matches(&mut e, "code", v.as_ref(), &code_re, CODE_MSG)
            .map_or(Field::Absent, Field::Val)
    } else {
        opt(m, "code", |v| {
            check_matches(&mut e, "code", Some(&upper(v)), &code_re, CODE_MSG)
        })
    };
    let name = if create {
        check_length_string(&mut e, "name", m.get("name"), 1, 150).map_or(Field::Absent, Field::Val)
    } else {
        opt(m, "name", |v| {
            check_length_string(&mut e, "name", Some(v), 1, 150)
        })
    };
    let is_active = if create {
        Field::Absent
    } else {
        opt(m, "isActive", |v| check_bool(&mut e, "isActive", Some(v)))
    };
    let boolean =
        |e: &mut Vec<String>, key: &str| opt(m, key, |v| check_bool(e, key, Some(&to_boolean(v))));
    let requires_approval = boolean(&mut e, "requiresApproval");
    let allow_discount = boolean(&mut e, "allowDiscount");
    let allow_free_item = boolean(&mut e, "allowFreeItem");
    let number_prefix = opt(m, "numberPrefix", |v| {
        check_matches(
            &mut e,
            "numberPrefix",
            Some(&upper(v)),
            &prefix_re,
            PREFIX_MSG,
        )
    });
    let is_default = boolean(&mut e, "isDefault");
    let reason_type = opt(m, "reasonType", |v| {
        check_enum(&mut e, "reasonType", Some(v), ORDER_REASON_TYPES)
    });
    // `@IsOptional() @Transform(parseInt)`: `null` → NaN (gagal), bukan dilewati.
    let sort_order = match m.get("sortOrder") {
        None => Field::Absent,
        Some(v) => check_parse_int_min(&mut e, "sortOrder", v, 0).map_or(Field::Absent, Field::Val),
    };
    let due_days = match m.get("dueDays") {
        None => Field::Absent,
        Some(v) => {
            let n = mz_products::dto::parse_int_value(v);
            if n.is_none_or(|n| n > 3650) {
                e.push("dueDays must not be greater than 3650".into());
            }
            if n.is_none_or(|n| n < 0) {
                e.push("dueDays must not be less than 0".into());
            }
            if n.is_none() {
                e.push("dueDays must be an integer number".into());
            }
            n.filter(|n| (0..=3650).contains(n))
                .map_or(Field::Absent, Field::Val)
        }
    };
    let requires_reference = boolean(&mut e, "requiresReference");
    if e.is_empty() {
        Ok(MasterDto {
            code,
            name,
            is_active,
            requires_approval,
            allow_discount,
            allow_free_item,
            number_prefix,
            is_default,
            reason_type,
            sort_order,
            due_days,
            requires_reference,
        })
    } else {
        Err(e)
    }
}

fn defined(d: &MasterDto, field: &str) -> bool {
    match field {
        "requiresApproval" => d.requires_approval.is_defined(),
        "allowDiscount" => d.allow_discount.is_defined(),
        "allowFreeItem" => d.allow_free_item.is_defined(),
        "numberPrefix" => d.number_prefix.is_defined(),
        "reasonType" => d.reason_type.is_defined(),
        "dueDays" => d.due_days.is_defined(),
        "requiresReference" => d.requires_reference.is_defined(),
        _ => false,
    }
}

fn invalid_field(field: &str, kind: MasterKind) -> ApiError {
    unprocessable(
        "ORDER_MASTER_INVALID_FIELD",
        format!("{field} is not accepted for {}", kind.label()),
    )
}

/// `assertFieldsAllowed`.
fn assert_fields_allowed(kind: MasterKind, d: &MasterDto) -> ApiResult<()> {
    if !kind.has_is_default() && d.is_default.is_defined() {
        return Err(invalid_field("isDefault", kind));
    }
    if !kind.has_sort_order() && d.sort_order.is_defined() {
        return Err(invalid_field("sortOrder", kind));
    }
    for f in ALL_EXTRA_FIELD_NAMES {
        if defined(d, f) && !kind.extra_fields().contains(&f) {
            return Err(invalid_field(f, kind));
        }
    }
    Ok(())
}

/// `assertNameLength` (panjang JS = unit UTF-16).
fn assert_name_length(kind: MasterKind, name: &str) -> ApiResult<()> {
    let max = kind.name_max_length();
    if name.encode_utf16().count() > max {
        return Err(unprocessable(
            "ORDER_MASTER_NAME_TOO_LONG",
            format!("name must be at most {max} characters for {}", kind.label()),
        ));
    }
    Ok(())
}

/// `(kolom, nilai)` ekstra yang ditulis (`buildExtraData`), urutan config.
fn extra_data(kind: MasterKind, d: &MasterDto) -> Vec<(&'static str, Value)> {
    fn val<T: serde::Serialize>(f: &Field<T>) -> Option<Value> {
        match f {
            Field::Absent => None,
            Field::Null => Some(Value::Null),
            Field::Val(v) => Some(json!(v)),
        }
    }
    let mut out = Vec::new();
    let mut push = |col: &'static str, ok: bool, v: Option<Value>| {
        if let (true, Some(v)) = (ok, v) {
            out.push((col, v));
        }
    };
    let extra = kind.extra_fields();
    push("isDefault", kind.has_is_default(), val(&d.is_default));
    push(
        "requiresApproval",
        extra.contains(&"requiresApproval"),
        val(&d.requires_approval),
    );
    push(
        "allowDiscount",
        extra.contains(&"allowDiscount"),
        val(&d.allow_discount),
    );
    push(
        "allowFreeItem",
        extra.contains(&"allowFreeItem"),
        val(&d.allow_free_item),
    );
    push(
        "numberPrefix",
        extra.contains(&"numberPrefix"),
        val(&d.number_prefix),
    );
    push(
        "reasonType",
        extra.contains(&"reasonType"),
        val(&d.reason_type),
    );
    push("sortOrder", kind.has_sort_order(), val(&d.sort_order));
    push("dueDays", extra.contains(&"dueDays"), val(&d.due_days));
    push(
        "requiresReference",
        extra.contains(&"requiresReference"),
        val(&d.requires_reference),
    );
    out
}

fn push_value(qb: &mut QueryBuilder<'_, Postgres>, col: &str, v: &Value) {
    match (col, v) {
        (_, Value::Null) => {
            qb.push("NULL");
        }
        ("reasonType", Value::String(s)) => {
            qb.push_bind(s.clone()).push(r#"::"OrderReasonType""#);
        }
        (_, Value::Bool(b)) => {
            qb.push_bind(*b);
        }
        (_, Value::Number(n)) => {
            qb.push_bind(i32::try_from(n.as_i64().unwrap_or(0)).unwrap_or(i32::MAX));
        }
        (_, Value::String(s)) => {
            qb.push_bind(s.clone());
        }
        _ => {
            qb.push("NULL");
        }
    }
}

fn code_taken(kind: MasterKind, code: &str) -> ApiError {
    conflict(
        "ORDER_MASTER_CODE_TAKEN",
        format!(
            "{} code {code} is already used in this company",
            kind.label()
        ),
    )
}

/// `rethrowP2002`: target `code` → 409 `ORDER_MASTER_CODE_TAKEN`; lainnya (indeks default parsial,
/// target `companyId`) → galat P2002 mentah (filter global 409).
fn map_write_error(e: sqlx::Error, kind: MasterKind, code: &str) -> ApiError {
    match crate::common::unique_constraint(&e) {
        Some(c) if c.ends_with("_code_unique_per_company") => code_taken(kind, code),
        _ => e.into(),
    }
}

/// `writeWithDefaultSwitch`: nyalakan `isDefault` → matikan default lain di transaksi yang sama.
async fn unset_defaults(
    conn: &mut PgConnection,
    audit: &AuditHandle,
    kind: MasterKind,
    company_id: i32,
) -> ApiResult<()> {
    let pre = audit
        .pre_read_many(
            kind.model(),
            Lookup::filter(move |qb| {
                qb.push(r#""companyId" = "#)
                    .push_bind(company_id)
                    .push(r#" AND "isDefault" = true"#);
            }),
        )
        .await;
    let n = sqlx::query(&format!(
        r#"UPDATE "{}" SET "isDefault" = false WHERE "companyId" = $1 AND "isDefault" = true"#,
        kind.table()
    ))
    .bind(company_id)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    audit
        .record_update_many(
            kind.model(),
            pre,
            n,
            &Row::new().with("isDefault", false),
            json!({ "companyId": company_id, "isDefault": true }),
        )
        .await;
    Ok(())
}

async fn find_row_or_throw(
    state: &AppState,
    kind: MasterKind,
    company_id: i32,
    uuid: &str,
) -> ApiResult<MasterRow> {
    find_by_uuid(&state.db, kind, company_id, uuid)
        .await?
        .ok_or_else(|| {
            not_found(
                "ORDER_MASTER_NOT_FOUND",
                format!("{} not found", kind.label()),
            )
        })
}

/// Node: src/orders/order-masters.service.ts:245 `create`
pub async fn create(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    kind: MasterKind,
    d: &MasterDto,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    assert_fields_allowed(kind, d)?;
    if kind == MasterKind::Reasons && !d.reason_type.is_defined() {
        return Err(unprocessable(
            "ORDER_REASON_TYPE_REQUIRED",
            format!("reasonType is required to create {}", kind.label()),
        ));
    }
    let (Field::Val(code), Field::Val(name)) = (&d.code, &d.name) else {
        return Err(internal("code/name wajib"));
    };
    assert_name_length(kind, name)?;
    let extra = extra_data(kind, d);
    let mut tx = state.db.begin().await?;
    if kind.has_is_default() && matches!(d.is_default, Field::Val(true)) {
        unset_defaults(&mut tx, audit, kind, company_id).await?;
    }
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(format!(
        r#"INSERT INTO "{}" ("companyId", code, name, "createdById""#,
        kind.table()
    ));
    for (col, _) in &extra {
        qb.push(format!(r#", "{col}""#));
    }
    qb.push(") VALUES (")
        .push_bind(company_id)
        .push(", ")
        .push_bind(code.clone())
        .push(", ")
        .push_bind(name.clone())
        .push(", ")
        .push_bind(user_id);
    for (col, v) in &extra {
        qb.push(", ");
        push_value(&mut qb, col, v);
    }
    qb.push(") RETURNING id");
    let id: i32 = qb
        .build_query_scalar()
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| map_write_error(e, kind, code))?;
    if let Some(r) = audit
        .result_row(&mut *tx, kind.model(), Lookup::Id(id))
        .await
    {
        audit.record_create(kind.model(), &r).await;
    }
    let row = find_by_id(&mut *tx, kind, id).await?;
    tx.commit().await?;
    Ok(to_entity(&row, kind))
}

/// `ListOrderMastersQueryDto`.
pub struct ListQuery {
    pub search: Option<String>,
    pub is_active: Option<bool>,
    pub is_default: Option<bool>,
    pub reason_type: Option<String>,
    pub page: i64,
    pub limit: i64,
}

pub fn parse_list(m: &Map<String, Value>) -> Result<ListQuery, Vec<String>> {
    let mut e = Vec::new();
    let search = opt_max_string(m, &mut e, "search", 120);
    let is_active = query_bool(m, &mut e, "isActive");
    let is_default = query_bool(m, &mut e, "isDefault");
    let reason_type = mz_products::dto::present(m, "reasonType")
        .and_then(|v| check_enum(&mut e, "reasonType", Some(v), ORDER_REASON_TYPES));
    let (page, limit) = page_limit_max(m, &mut e, 100);
    if e.is_empty() {
        Ok(ListQuery {
            search,
            is_active,
            is_default,
            reason_type,
            page,
            limit,
        })
    } else {
        Err(e)
    }
}

/// Node: src/orders/order-masters.service.ts:270 `findAll`
pub async fn find_all(
    state: &AppState,
    user_id: i32,
    kind: MasterKind,
    q: &ListQuery,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    if q.is_default.is_some() && !kind.has_is_default() {
        return Err(invalid_field("isDefault", kind));
    }
    if q.reason_type.is_some() && kind != MasterKind::Reasons {
        return Err(invalid_field("reasonType", kind));
    }
    let filter = |qb: &mut QueryBuilder<'_, Postgres>| {
        qb.push(r#" WHERE "companyId" = "#).push_bind(company_id);
        if let Some(a) = q.is_active {
            qb.push(r#" AND "isActive" = "#).push_bind(a);
        }
        if let Some(d) = q.is_default {
            qb.push(r#" AND "isDefault" = "#).push_bind(d);
        }
        if let Some(r) = &q.reason_type {
            qb.push(r#" AND "reasonType" = "#)
                .push_bind(r.clone())
                .push(r#"::"OrderReasonType""#);
        }
        if let Some(s) = q.search.as_deref().filter(|s| !s.is_empty()) {
            let p = like_pattern(s);
            qb.push(" AND (code ILIKE ")
                .push_bind(p.clone())
                .push(" OR name ILIKE ")
                .push_bind(p)
                .push(")");
        }
    };
    let mut qb: QueryBuilder<Postgres> =
        QueryBuilder::new(format!(r#"SELECT {} FROM "{}""#, kind.cols(), kind.table()));
    filter(&mut qb);
    qb.push(format!(" ORDER BY {} LIMIT ", kind.order_by()))
        .push_bind(q.limit)
        .push(" OFFSET ")
        .push_bind(offset(q.page, q.limit));
    let rows: Vec<MasterRow> = qb.build_query_as().fetch_all(&state.db).await?;
    let mut cq: QueryBuilder<Postgres> =
        QueryBuilder::new(format!(r#"SELECT COUNT(*) FROM "{}""#, kind.table()));
    filter(&mut cq);
    let total: i64 = cq.build_query_scalar().fetch_one(&state.db).await?;
    Ok(json!({
        "data": rows.iter().map(|r| to_entity(r, kind)).collect::<Vec<_>>(),
        "meta": page_meta(total, q.page, q.limit),
    }))
}

/// Node: src/orders/order-masters.service.ts:318 `update`
pub async fn update(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    kind: MasterKind,
    uuid: &str,
    d: &MasterDto,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let row = find_row_or_throw(state, kind, company_id, uuid).await?;
    assert_fields_allowed(kind, d)?;
    if let Field::Val(name) = &d.name {
        assert_name_length(kind, name)?;
    }
    let extra = extra_data(kind, d);
    let code_for_error = match &d.code {
        Field::Val(c) => c.clone(),
        _ => row.code.clone(),
    };
    let mut tx = state.db.begin().await?;
    if kind.has_is_default() && matches!(d.is_default, Field::Val(true)) {
        unset_defaults(&mut tx, audit, kind, company_id).await?;
    }
    let pre = audit.pre_read(kind.model(), Lookup::Id(row.id)).await;
    let mut qb: QueryBuilder<Postgres> =
        QueryBuilder::new(format!(r#"UPDATE "{}" SET "#, kind.table()));
    let mut sets: Vec<(&str, Value)> = Vec::new();
    for (col, f) in [("code", &d.code), ("name", &d.name)] {
        match f {
            Field::Val(v) => sets.push((col, json!(v))),
            Field::Null => sets.push((col, Value::Null)),
            Field::Absent => {}
        }
    }
    match &d.is_active {
        Field::Val(v) => sets.push(("isActive", json!(v))),
        Field::Null => sets.push(("isActive", Value::Null)),
        Field::Absent => {}
    }
    sets.extend(extra);
    for (col, v) in &sets {
        qb.push(format!(r#""{col}" = "#));
        push_value(&mut qb, col, v);
        qb.push(", ");
    }
    qb.push(r#""updatedAt" = "#)
        .push_bind(now_naive())
        .push(" WHERE id = ")
        .push_bind(row.id);
    qb.build()
        .execute(&mut *tx)
        .await
        .map_err(|e| map_write_error(e, kind, &code_for_error))?;
    if let Some(r) = audit
        .result_row(&mut *tx, kind.model(), Lookup::Id(row.id))
        .await
    {
        audit.record_update(kind.model(), pre, &r).await;
    }
    let updated = find_by_id(&mut *tx, kind, row.id).await?;
    tx.commit().await?;
    Ok(to_entity(&updated, kind))
}

/// Node: src/orders/order-masters.service.ts:350 `remove` — arsip.
pub async fn remove(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    kind: MasterKind,
    uuid: &str,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let row = find_row_or_throw(state, kind, company_id, uuid).await?;
    let pre = audit.pre_read(kind.model(), Lookup::Id(row.id)).await;
    sqlx::query(&format!(
        r#"UPDATE "{}" SET "isActive" = false, "updatedAt" = $2 WHERE id = $1"#,
        kind.table()
    ))
    .bind(row.id)
    .bind(now_naive())
    .execute(&state.db)
    .await?;
    if let Some(r) = audit
        .result_row(&state.db, kind.model(), Lookup::Id(row.id))
        .await
    {
        audit.record_update(kind.model(), pre, &r).await;
    }
    Ok(json!({ "id": row.uuid, "archived": true }))
}

// ───────────────────────── handler ─────────────────────────

/// `:kind` (`ParseEnumPipe`, divalidasi sebelum body/query) + `:id` opsional.
pub struct KindParams(pub MasterKind, pub Option<String>);

impl FromRequestParts<AppState> for KindParams {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let Path(map): Path<std::collections::HashMap<String, String>> =
            Path::from_request_parts(parts, state)
                .await
                .map_err(|e| internal(&e.to_string()))?;
        let kind = map
            .get("kind")
            .and_then(|k| MasterKind::parse(k))
            .ok_or_else(|| ApiError::bad_request("Validation failed (enum string is expected)"))?;
        Ok(Self(kind, map.get("id").cloned()))
    }
}

/// Node: src/orders/admin-order-masters.controller.ts:92 `findAll`
async fn h_find_all(
    State(state): State<AppState>,
    ctx: RequestCtx,
    KindParams(kind, _): KindParams,
    ValidatedQuery(q): ValidatedQuery<Raw>,
) -> ApiResult<Json<Value>> {
    let q = parse_list(&q.0).map_err(ApiError::Validation)?;
    Ok(Json(
        find_all(&state, ctx.require_user_id()?, kind, &q).await?,
    ))
}

/// Node: src/orders/admin-order-masters.controller.ts:120 `create` (201)
async fn h_create(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    KindParams(kind, _): KindParams,
    ValidatedJson(body): ValidatedJson<Raw>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let d = parse_dto(&body.0, true).map_err(ApiError::Validation)?;
    let data = create(&state, &audit, ctx.require_user_id()?, kind, &d).await?;
    Ok((StatusCode::CREATED, Json(json!({ "data": data }))))
}

/// Node: src/orders/admin-order-masters.controller.ts:149 `findOne`
async fn h_find_one(
    State(state): State<AppState>,
    ctx: RequestCtx,
    KindParams(kind, id): KindParams,
) -> ApiResult<Json<Value>> {
    let company_id = require_company_id(&state.db, ctx.require_user_id()?).await?;
    let row =
        find_row_or_throw(&state, kind, company_id, id.as_deref().unwrap_or_default()).await?;
    Ok(Json(json!({ "data": to_entity(&row, kind) })))
}

/// Node: src/orders/admin-order-masters.controller.ts:165 `update`
async fn h_update(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    KindParams(kind, id): KindParams,
    ValidatedJson(body): ValidatedJson<Raw>,
) -> ApiResult<Json<Value>> {
    let d = parse_dto(&body.0, false).map_err(ApiError::Validation)?;
    let data = update(
        &state,
        &audit,
        ctx.require_user_id()?,
        kind,
        id.as_deref().unwrap_or_default(),
        &d,
    )
    .await?;
    Ok(Json(json!({ "data": data })))
}

/// Node: src/orders/admin-order-masters.controller.ts:193 `remove`
async fn h_remove(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    KindParams(kind, id): KindParams,
) -> ApiResult<Json<Value>> {
    let data = remove(
        &state,
        &audit,
        ctx.require_user_id()?,
        kind,
        id.as_deref().unwrap_or_default(),
    )
    .await?;
    Ok(Json(json!({ "data": data })))
}

/// 5 route `/v1/admin/order-masters/{kind}` (`@RequireFeature('order-masters')`).
pub fn routes(state: &AppState) -> Router<AppState> {
    const M: &str = "/v1/admin/order-masters/{kind}";
    const MI: &str = "/v1/admin/order-masters/{kind}/{id}";
    let spec = crate::ORDER_MASTERS;
    Router::new()
        .route(M, guard(state, spec, get(h_find_all)))
        .route(M, guard(state, spec, post(h_create)))
        .route(MI, guard(state, spec, get(h_find_one)))
        .route(MI, guard(state, spec, patch(h_update)))
        .route(MI, guard(state, spec, delete(h_remove)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dto_messages() {
        let m = json!({"code": "bad-code", "name": "", "dueDays": "x", "requiresApproval": "true"});
        let e = parse_dto(m.as_object().unwrap(), true).unwrap_err();
        assert_eq!(
            e,
            vec![
                CODE_MSG.to_owned(),
                "name must be longer than or equal to 1 characters".into(),
                "dueDays must not be greater than 3650".into(),
                "dueDays must not be less than 0".into(),
                "dueDays must be an integer number".into(),
            ]
        );
        let ok = parse_dto(
            json!({"code": "reg_1", "name": "Regular", "requiresApproval": "true"})
                .as_object()
                .unwrap(),
            true,
        )
        .unwrap();
        assert_eq!(ok.code, Field::Val("REG_1".into()));
        assert_eq!(ok.requires_approval, Field::Val(true));
    }

    #[test]
    fn fields_allowed_per_kind() {
        let d = MasterDto {
            due_days: Field::Val(3),
            ..Default::default()
        };
        assert!(assert_fields_allowed(MasterKind::PaymentTerms, &d).is_ok());
        assert!(crate::common::has_code(
            &assert_fields_allowed(MasterKind::Types, &d).unwrap_err(),
            "ORDER_MASTER_INVALID_FIELD"
        ));
        let d = MasterDto {
            is_default: Field::Val(true),
            ..Default::default()
        };
        assert!(assert_fields_allowed(MasterKind::Reasons, &d).is_err());
        assert!(assert_name_length(MasterKind::Types, &"x".repeat(121)).is_err());
        assert!(assert_name_length(MasterKind::Reasons, &"x".repeat(150)).is_ok());
    }

    #[test]
    fn entity_shape() {
        let row = MasterRow {
            id: 1,
            uuid: "u".into(),
            company_id: 2,
            code: "C".into(),
            name: "N".into(),
            is_active: true,
            created_at: NaiveDateTime::default(),
            updated_at: None,
            requires_approval: Some(true),
            allow_discount: Some(true),
            allow_free_item: Some(false),
            number_prefix: Some("ORD".into()),
            is_default: Some(false),
            reason_type: None,
            sort_order: None,
            due_days: None,
            requires_reference: None,
        };
        let v = to_entity(&row, MasterKind::Types);
        let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            [
                "id",
                "code",
                "name",
                "isActive",
                "isDefault",
                "requiresApproval",
                "allowDiscount",
                "allowFreeItem",
                "numberPrefix",
                "createdAt",
                "updatedAt"
            ]
        );
    }
}
