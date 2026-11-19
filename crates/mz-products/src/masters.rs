//! Port `products/admin-product-masters.controller.ts` + `product-masters.service.ts` +
//! `product-master.delegate.ts` + `product-master-kind.config.ts`: enam tabel master produk
//! (`/v1/admin/product-masters/:kind`) dan pembacaan master yang dipakai modul lain.

use std::collections::HashMap;

use axum::extract::{FromRequestParts, Path, State};
use axum::http::request::Parts;
use axum::{Json, http::StatusCode};
use chrono::NaiveDateTime;
use mz_audit::{AuditHandle, Lookup, Model};
use mz_core::company::require_company_id;
use mz_core::extract::{ValidatedJson, ValidatedQuery};
use mz_core::time::now_naive;
use mz_core::{ApiError, ApiResult, AppState, RequestCtx};
use rust_decimal::Decimal;
use serde_json::{Map, Value, json};
use sqlx::{PgExecutor, Postgres, QueryBuilder};

use crate::common::{
    conflict, internal, like_pattern, ms, ms_opt, not_found, offset, page_meta, row_uuid,
    unprocessable,
};
use crate::dto::{
    CONVERSION_RATE, Field, MASTER_CODE, Raw, TAX_RATE, check_bool, check_enum,
    check_length_string, check_matches, check_max_string, check_parse_int_min, check_string,
    check_uuid4, opt_max_string, page_limit_max, query_bool, to_boolean,
};
use crate::util::{UOM_OPERATORS, UOM_TYPES, compute_base_equivalent, invalid_factor_message};

/// `ProductMasterKind` (segmen `:kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MasterKind {
    Categories,
    SubCategories,
    Brands,
    Principals,
    Uoms,
    Taxes,
}

impl MasterKind {
    pub const ALL: [MasterKind; 6] = [
        Self::Categories,
        Self::SubCategories,
        Self::Brands,
        Self::Principals,
        Self::Uoms,
        Self::Taxes,
    ];

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "categories" => Self::Categories,
            "sub-categories" => Self::SubCategories,
            "brands" => Self::Brands,
            "principals" => Self::Principals,
            "uoms" => Self::Uoms,
            "taxes" => Self::Taxes,
            _ => return None,
        })
    }

    /// Nama tabel (sudah dikutip) — literal, tidak pernah dari input.
    pub fn table(self) -> &'static str {
        match self {
            Self::Categories => r#""productCategory""#,
            Self::SubCategories => r#""productSubCategory""#,
            Self::Brands => r#""productBrand""#,
            Self::Principals => r#""productPrincipal""#,
            Self::Uoms => "uom",
            Self::Taxes => r#""taxRate""#,
        }
    }

    /// `LABEL_BY_KIND`.
    pub fn label(self) -> &'static str {
        match self {
            Self::Categories => "Product category",
            Self::SubCategories => "Product sub category",
            Self::Brands => "Product brand",
            Self::Principals => "Product principal",
            Self::Uoms => "Unit of measure",
            Self::Taxes => "Tax rate",
        }
    }

    pub fn audit_model(self) -> Model {
        match self {
            Self::Categories => Model::ProductCategory,
            Self::SubCategories => Model::ProductSubCategory,
            Self::Brands => Model::ProductBrand,
            Self::Principals => Model::ProductPrincipal,
            Self::Uoms => Model::Uom,
            Self::Taxes => Model::TaxRate,
        }
    }

    fn has_description(self) -> bool {
        matches!(self, Self::Categories | Self::SubCategories | Self::Brands)
    }

    fn has_sort_order(self) -> bool {
        self.has_description()
    }

    fn name_max(self) -> usize {
        if self == Self::Principals { 200 } else { 120 }
    }

    fn extra_fields(self) -> &'static [&'static str] {
        match self {
            Self::SubCategories => &["categoryId"],
            Self::Principals => &["contactPerson", "phone", "email", "address"],
            Self::Uoms => &["uomType", "defaultOperator", "defaultFactor"],
            Self::Taxes => &["rate", "isInclusive"],
            Self::Categories | Self::Brands => &[],
        }
    }

    fn required_extra(self) -> &'static [&'static str] {
        match self {
            Self::SubCategories => &["categoryId"],
            Self::Taxes => &["rate"],
            _ => &[],
        }
    }

    /// Urutan bawaan (`sortOrder` dulu bila ada).
    fn order_by(self) -> &'static str {
        if self.has_sort_order() {
            r#""sortOrder" ASC, name ASC, id ASC"#
        } else {
            "name ASC, id ASC"
        }
    }

    /// Daftar kolom superset (kolom milik kind lain = NULL) agar satu [`MasterRow`] cukup.
    fn select_list(self) -> String {
        let desc = if self.has_description() {
            r#"description, "sortOrder""#
        } else {
            r#"NULL::varchar AS description, NULL::int AS "sortOrder""#
        };
        let cat = if self == Self::SubCategories {
            r#""categoryId""#
        } else {
            r#"NULL::int AS "categoryId""#
        };
        let princ = if self == Self::Principals {
            r#""contactPerson", phone, email, address"#
        } else {
            r#"NULL::varchar AS "contactPerson", NULL::varchar AS phone, NULL::varchar AS email, NULL::text AS address"#
        };
        let uom = if self == Self::Uoms {
            r#""uomType"::text AS "uomType", "defaultOperator"::text AS "defaultOperator", "defaultFactor""#
        } else {
            r#"NULL::text AS "uomType", NULL::text AS "defaultOperator", NULL::numeric AS "defaultFactor""#
        };
        let tax = if self == Self::Taxes {
            r#"rate, "isInclusive""#
        } else {
            r#"NULL::numeric AS rate, NULL::bool AS "isInclusive""#
        };
        format!(
            r#"id, uuid::text AS uuid, "companyId", code, name, "isActive", "createdAt", "updatedAt", {desc}, {cat}, {princ}, {uom}, {tax}"#
        )
    }
}

/// `MasterRow`: superset kolom keenam tabel.
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
    pub description: Option<String>,
    #[sqlx(rename = "sortOrder")]
    pub sort_order: Option<i32>,
    #[sqlx(rename = "categoryId")]
    pub category_id: Option<i32>,
    #[sqlx(rename = "contactPerson")]
    pub contact_person: Option<String>,
    pub phone: Option<String>,
    pub email: Option<String>,
    pub address: Option<String>,
    #[sqlx(rename = "uomType")]
    pub uom_type: Option<String>,
    #[sqlx(rename = "defaultOperator")]
    pub default_operator: Option<String>,
    #[sqlx(rename = "defaultFactor")]
    pub default_factor: Option<Decimal>,
    pub rate: Option<Decimal>,
    #[sqlx(rename = "isInclusive")]
    pub is_inclusive: Option<bool>,
}

/// `{ id: uuid, code, name }` (ref master di entitas produk/price list).
pub fn ref_json(row: Option<&MasterRow>) -> Value {
    row.map_or(
        Value::Null,
        |r| json!({ "id": r.uuid, "code": r.code, "name": r.name }),
    )
}

// ───────────────────────── pembacaan ─────────────────────────

fn select(kind: MasterKind) -> String {
    format!("SELECT {} FROM {} WHERE ", kind.select_list(), kind.table())
}

/// `findFirst({ where: { uuid, companyId } })`.
pub async fn find_by_uuid<'e>(
    exec: impl PgExecutor<'e>,
    kind: MasterKind,
    company_id: i32,
    uuid: uuid::Uuid,
) -> ApiResult<Option<MasterRow>> {
    let sql = format!(r#"{}uuid = $1 AND "companyId" = $2 LIMIT 1"#, select(kind));
    Ok(sqlx::query_as(&sql)
        .bind(uuid)
        .bind(company_id)
        .fetch_optional(exec)
        .await?)
}

/// `findFirst({ where: { id, companyId } })`.
pub async fn find_by_id<'e>(
    exec: impl PgExecutor<'e>,
    kind: MasterKind,
    company_id: i32,
    id: i32,
) -> ApiResult<Option<MasterRow>> {
    let sql = format!(r#"{}id = $1 AND "companyId" = $2 LIMIT 1"#, select(kind));
    Ok(sqlx::query_as(&sql)
        .bind(id)
        .bind(company_id)
        .fetch_optional(exec)
        .await?)
}

/// `findFirst({ where: { companyId, code } })`.
pub async fn find_by_code<'e>(
    exec: impl PgExecutor<'e>,
    kind: MasterKind,
    company_id: i32,
    code: &str,
) -> ApiResult<Option<MasterRow>> {
    let sql = format!(
        r#"{}"companyId" = $1 AND code = $2 ORDER BY id LIMIT 1"#,
        select(kind)
    );
    Ok(sqlx::query_as(&sql)
        .bind(company_id)
        .bind(code)
        .fetch_optional(exec)
        .await?)
}

/// `findMany({ where: { companyId, id: { in } } })`.
pub async fn find_by_ids<'e>(
    exec: impl PgExecutor<'e>,
    kind: MasterKind,
    company_id: i32,
    ids: &[i32],
) -> ApiResult<Vec<MasterRow>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!(
        r#"{}"companyId" = $1 AND id = ANY($2) ORDER BY id"#,
        select(kind)
    );
    Ok(sqlx::query_as(&sql)
        .bind(company_id)
        .bind(ids)
        .fetch_all(exec)
        .await?)
}

/// `findMany({ where: { companyId, uuid: { in } } })`.
pub async fn find_by_uuids<'e>(
    exec: impl PgExecutor<'e>,
    kind: MasterKind,
    company_id: i32,
    uuids: &[uuid::Uuid],
) -> ApiResult<Vec<MasterRow>> {
    if uuids.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!(
        r#"{}"companyId" = $1 AND uuid = ANY($2) ORDER BY id"#,
        select(kind)
    );
    Ok(sqlx::query_as(&sql)
        .bind(company_id)
        .bind(uuids)
        .fetch_all(exec)
        .await?)
}

/// Semua baris company (maks `take`) — peta master import (§9.2).
pub async fn list_company<'e>(
    exec: impl PgExecutor<'e>,
    kind: MasterKind,
    company_id: i32,
    take: i64,
) -> ApiResult<Vec<MasterRow>> {
    let sql = format!(r#"{}"companyId" = $1 ORDER BY id LIMIT $2"#, select(kind));
    Ok(sqlx::query_as(&sql)
        .bind(company_id)
        .bind(take)
        .fetch_all(exec)
        .await?)
}

/// `refsByIds`: id → baris.
pub async fn map_by_ids<'e>(
    exec: impl PgExecutor<'e>,
    kind: MasterKind,
    company_id: i32,
    ids: impl IntoIterator<Item = i32>,
) -> ApiResult<HashMap<i32, MasterRow>> {
    let mut v: Vec<i32> = ids.into_iter().collect();
    v.sort_unstable();
    v.dedup();
    Ok(find_by_ids(exec, kind, company_id, &v)
        .await?
        .into_iter()
        .map(|r| (r.id, r))
        .collect())
}

// ───────────────────────── entitas ─────────────────────────

/// `toEntity(row, kind, categoryRef)`; `category = None` = kunci tidak dikirim.
pub fn to_entity(row: &MasterRow, kind: MasterKind, category: Option<Option<&MasterRow>>) -> Value {
    let mut m = Map::new();
    m.insert("id".into(), json!(row.uuid));
    m.insert("code".into(), json!(row.code));
    m.insert("name".into(), json!(row.name));
    m.insert("isActive".into(), json!(row.is_active));
    if kind.has_description() {
        m.insert("description".into(), json!(row.description));
    }
    if kind.has_sort_order() {
        m.insert("sortOrder".into(), json!(row.sort_order.unwrap_or(0)));
    }
    if let Some(cat) = category {
        m.insert("category".into(), ref_json(cat));
    }
    match kind {
        MasterKind::Principals => {
            m.insert("contactPerson".into(), json!(row.contact_person));
            m.insert("phone".into(), json!(row.phone));
            m.insert("email".into(), json!(row.email));
            m.insert("address".into(), json!(row.address));
        }
        MasterKind::Uoms => {
            if let Some(t) = &row.uom_type {
                m.insert("uomType".into(), json!(t));
            }
            if let Some(o) = &row.default_operator {
                m.insert("defaultOperator".into(), json!(o));
            }
            if let Some(f) = &row.default_factor {
                m.insert("defaultFactor".into(), json!(crate::common::dec_str(f)));
            }
        }
        MasterKind::Taxes => {
            if let Some(r) = &row.rate {
                m.insert("rate".into(), json!(crate::common::dec_str(r)));
            }
            if let Some(i) = row.is_inclusive {
                m.insert("isInclusive".into(), json!(i));
            }
        }
        _ => {}
    }
    m.insert("createdAt".into(), json!(ms(row.created_at)));
    m.insert("updatedAt".into(), ms_opt(row.updated_at));
    Value::Object(m)
}

// ───────────────────────── extractor `:kind` ─────────────────────────

fn kind_pipe(raw: &str) -> ApiResult<MasterKind> {
    MasterKind::parse(raw)
        .ok_or_else(|| ApiError::bad_request("Validation failed (enum string is expected)"))
}

/// `@Param('kind', new ParseEnumPipe(ProductMasterKind))`.
pub struct KindPath(pub MasterKind);

impl FromRequestParts<AppState> for KindPath {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let Path(raw): Path<String> = Path::from_request_parts(parts, state)
            .await
            .map_err(|e| internal(&e.to_string()))?;
        Ok(Self(kind_pipe(&raw)?))
    }
}

/// `:kind` (ParseEnumPipe) + `:id` (tanpa pipe).
pub struct KindIdPath(pub MasterKind, pub String);

impl FromRequestParts<AppState> for KindIdPath {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let Path((kind, id)): Path<(String, String)> = Path::from_request_parts(parts, state)
            .await
            .map_err(|e| internal(&e.to_string()))?;
        Ok(Self(kind_pipe(&kind)?, id))
    }
}

// ───────────────────────── DTO ─────────────────────────

/// `CreateProductMasterDto` / `UpdateProductMasterDto` (properti tiga-keadaan).
#[derive(Debug, Default)]
pub struct MasterDto {
    pub code: Field<String>,
    pub name: Field<String>,
    pub is_active: Field<bool>,
    pub description: Field<String>,
    pub sort_order: Field<i64>,
    pub category_id: Field<String>,
    pub contact_person: Field<String>,
    pub phone: Field<String>,
    pub email: Field<String>,
    pub address: Field<String>,
    pub uom_type: Field<String>,
    pub default_operator: Field<String>,
    pub default_factor: Field<String>,
    pub rate: Field<String>,
    pub is_inclusive: Field<bool>,
}

impl MasterDto {
    /// `record[field] !== undefined` untuk nama field ekstra/opsional.
    fn defined(&self, field: &str) -> bool {
        match field {
            "description" => self.description.is_defined(),
            "sortOrder" => self.sort_order.is_defined(),
            "categoryId" => self.category_id.is_defined(),
            "contactPerson" => self.contact_person.is_defined(),
            "phone" => self.phone.is_defined(),
            "email" => self.email.is_defined(),
            "address" => self.address.is_defined(),
            "uomType" => self.uom_type.is_defined(),
            "defaultOperator" => self.default_operator.is_defined(),
            "defaultFactor" => self.default_factor.is_defined(),
            "rate" => self.rate.is_defined(),
            "isInclusive" => self.is_inclusive.is_defined(),
            _ => false,
        }
    }
}

const ALL_EXTRA: &[&str] = &[
    "categoryId",
    "contactPerson",
    "phone",
    "email",
    "address",
    "uomType",
    "defaultOperator",
    "defaultFactor",
    "rate",
    "isInclusive",
];

const CODE_MSG: &str =
    "code must be 1-32 characters of uppercase letters, digits, underscore, or hyphen";

fn opt<T>(
    m: &Map<String, Value>,
    key: &str,
    required: bool,
    mut check: impl FnMut(Option<&Value>) -> Option<T>,
) -> Field<T> {
    match m.get(key) {
        None if !required => Field::Absent,
        Some(Value::Null) if !required => Field::Null,
        v => check(v).map_or(Field::Absent, Field::Val),
    }
}

/// Validasi body master (`create = true` → `code`/`name` wajib, tanpa `isActive`).
pub fn parse_master(m: &Map<String, Value>, create: bool) -> Result<MasterDto, Vec<String>> {
    let mut e = Vec::new();
    let mut d = MasterDto {
        code: opt(m, "code", create, |v| {
            let v = v.map(|v| match v {
                Value::String(s) => Value::String(s.to_uppercase()),
                o => o.clone(),
            });
            check_matches(&mut e, "code", v.as_ref(), &MASTER_CODE, CODE_MSG)
        }),
        ..MasterDto::default()
    };
    d.name = opt(m, "name", create, |v| {
        check_length_string(&mut e, "name", v, 1, 200)
    });
    if !create {
        d.is_active = opt(m, "isActive", false, |v| check_bool(&mut e, "isActive", v));
    }
    d.description = opt(m, "description", false, |v| {
        check_max_string(&mut e, "description", v, 255)
    });
    d.sort_order = opt(m, "sortOrder", false, |v| {
        v.and_then(|v| check_parse_int_min(&mut e, "sortOrder", v, 0))
    });
    // `@Transform(parseInt)` membuat `null` jadi NaN → tetap divalidasi.
    if matches!(m.get("sortOrder"), Some(Value::Null)) {
        check_parse_int_min(&mut e, "sortOrder", &Value::Null, 0);
    }
    d.category_id = opt(m, "categoryId", false, |v| {
        check_uuid4(&mut e, "categoryId", v)
    });
    d.contact_person = opt(m, "contactPerson", false, |v| {
        check_max_string(&mut e, "contactPerson", v, 150)
    });
    d.phone = opt(m, "phone", false, |v| {
        check_max_string(&mut e, "phone", v, 50)
    });
    d.email = opt(m, "email", false, |v| {
        let before = e.len();
        let s = v.and_then(Value::as_str);
        if s.is_none_or(|s| mz_core::extract::validator_js_length(s) > 150) {
            e.push("email must be shorter than or equal to 150 characters".into());
        }
        if s.is_none_or(|s| !mz_core::extract::is_email(s)) {
            e.push("email must be an email".into());
        }
        s.filter(|_| e.len() == before).map(str::to_owned)
    });
    d.address = opt(m, "address", false, |v| check_string(&mut e, "address", v));
    d.uom_type = opt(m, "uomType", false, |v| {
        check_enum(&mut e, "uomType", v, UOM_TYPES)
    });
    d.default_operator = opt(m, "defaultOperator", false, |v| {
        check_enum(&mut e, "defaultOperator", v, UOM_OPERATORS)
    });
    d.default_factor = opt(m, "defaultFactor", false, |v| {
        check_matches(
            &mut e,
            "defaultFactor",
            v,
            &CONVERSION_RATE,
            "defaultFactor must be a positive number with up to 6 decimal places",
        )
    });
    d.rate = opt(m, "rate", false, |v| {
        check_matches(
            &mut e,
            "rate",
            v,
            &TAX_RATE,
            "rate must be a non-negative number with up to 4 decimal places",
        )
    });
    d.is_inclusive = opt(m, "isInclusive", false, |v| {
        let v = v.map(to_boolean);
        check_bool(&mut e, "isInclusive", v.as_ref())
    });
    if e.is_empty() { Ok(d) } else { Err(e) }
}

// ───────────────────────── service ─────────────────────────

fn invalid_field(field: &str, kind: MasterKind) -> ApiError {
    unprocessable(
        "PRODUCT_MASTER_INVALID_FIELD",
        format!("{field} is not accepted for {}", kind.label()),
    )
}

fn assert_fields_allowed(kind: MasterKind, dto: &MasterDto) -> ApiResult<()> {
    if !kind.has_description() && dto.description.is_defined() {
        return Err(invalid_field("description", kind));
    }
    if !kind.has_sort_order() && dto.sort_order.is_defined() {
        return Err(invalid_field("sortOrder", kind));
    }
    for f in ALL_EXTRA {
        if dto.defined(f) && !kind.extra_fields().contains(f) {
            return Err(invalid_field(f, kind));
        }
    }
    Ok(())
}

fn assert_required_fields(kind: MasterKind, dto: &MasterDto) -> ApiResult<()> {
    for f in kind.required_extra() {
        if !dto.defined(f) {
            let code = match *f {
                "categoryId" => "SUBCATEGORY_WITHOUT_CATEGORY",
                "rate" => "TAX_RATE_REQUIRED",
                _ => "PRODUCT_MASTER_FIELD_REQUIRED",
            };
            return Err(unprocessable(
                code,
                format!("{f} is required to create {}", kind.label()),
            ));
        }
    }
    Ok(())
}

fn assert_name_length(kind: MasterKind, name: &str) -> ApiResult<()> {
    if name.encode_utf16().count() > kind.name_max() {
        return Err(unprocessable(
            "PRODUCT_MASTER_NAME_TOO_LONG",
            format!(
                "name must be at most {} characters for {}",
                kind.name_max(),
                kind.label()
            ),
        ));
    }
    Ok(())
}

fn assert_default_conversion(
    kind: MasterKind,
    dto: &MasterDto,
    current: Option<&MasterRow>,
) -> ApiResult<()> {
    if kind != MasterKind::Uoms
        || (!dto.default_operator.is_defined() && !dto.default_factor.is_defined())
    {
        return Ok(());
    }
    let operator = match &dto.default_operator {
        Field::Val(o) => o.clone(),
        _ => current
            .and_then(|c| c.default_operator.clone())
            .unwrap_or_else(|| "multiply".into()),
    };
    let factor = match &dto.default_factor {
        Field::Val(f) => f.clone(),
        _ => current
            .and_then(|c| c.default_factor.as_ref())
            .map_or_else(|| "1".into(), crate::common::dec_str),
    };
    if compute_base_equivalent(&operator, &factor).is_none() {
        return Err(unprocessable(
            "UOM_INVALID_DEFAULT_FACTOR",
            invalid_factor_message(&operator, "defaultFactor"),
        ));
    }
    Ok(())
}

fn code_taken(kind: MasterKind, code: &str) -> ApiError {
    conflict(
        "PRODUCT_MASTER_CODE_TAKEN",
        format!(
            "{} code {code} is already used in this company",
            kind.label()
        ),
    )
}

/// `resolveActiveCategoryId`: 404 / 422 archived.
async fn resolve_active_category_id<'e>(
    exec: impl PgExecutor<'e>,
    company_id: i32,
    raw: &str,
) -> ApiResult<i32> {
    let row = find_by_uuid(exec, MasterKind::Categories, company_id, row_uuid(raw)?)
        .await?
        .ok_or_else(|| not_found("PRODUCT_MASTER_NOT_FOUND", "Product category not found"))?;
    if !row.is_active {
        return Err(unprocessable(
            "PRODUCT_MASTER_ARCHIVED",
            "Cannot assign an archived product category",
        ));
    }
    Ok(row.id)
}

async fn find_row_or_throw(
    state: &AppState,
    kind: MasterKind,
    company_id: i32,
    raw: &str,
) -> ApiResult<MasterRow> {
    find_by_uuid(&state.db, kind, company_id, row_uuid(raw)?)
        .await?
        .ok_or_else(|| {
            not_found(
                "PRODUCT_MASTER_NOT_FOUND",
                format!("{} not found", kind.label()),
            )
        })
}

/// `buildExtraData` → kolom yang ditulis (nama, nilai, cast SQL).
fn push_extra(
    kind: MasterKind,
    dto: &MasterDto,
    category_id: Option<i32>,
) -> Vec<(&'static str, Value, &'static str)> {
    let mut out: Vec<(&'static str, Value, &'static str)> = Vec::new();
    let val = |f: &Field<String>| match f {
        Field::Val(s) => Some(Value::String(s.clone())),
        Field::Null => Some(Value::Null),
        Field::Absent => None,
    };
    if kind.has_description()
        && let Some(v) = val(&dto.description)
    {
        out.push(("description", v, ""));
    }
    if kind.has_sort_order() {
        match dto.sort_order {
            Field::Val(n) => out.push(("sortOrder", json!(n), "int")),
            Field::Null => out.push(("sortOrder", Value::Null, "int")),
            Field::Absent => {}
        }
    }
    let extra = kind.extra_fields();
    if extra.contains(&"categoryId")
        && let Some(id) = category_id
    {
        out.push(("categoryId", json!(id), "int"));
    }
    for (name, f) in [
        ("contactPerson", &dto.contact_person),
        ("phone", &dto.phone),
        ("email", &dto.email),
        ("address", &dto.address),
    ] {
        if extra.contains(&name)
            && let Some(v) = val(f)
        {
            out.push((name, v, ""));
        }
    }
    if extra.contains(&"uomType")
        && let Some(v) = val(&dto.uom_type)
    {
        out.push(("uomType", v, "\"UomType\""));
    }
    if extra.contains(&"defaultOperator")
        && let Some(v) = val(&dto.default_operator)
    {
        out.push(("defaultOperator", v, "\"UomOperator\""));
    }
    if extra.contains(&"defaultFactor")
        && let Some(v) = val(&dto.default_factor)
    {
        out.push(("defaultFactor", v, "numeric"));
    }
    if extra.contains(&"rate")
        && let Some(v) = val(&dto.rate)
    {
        out.push(("rate", v, "numeric"));
    }
    if extra.contains(&"isInclusive") {
        match dto.is_inclusive {
            Field::Val(b) => out.push(("isInclusive", json!(b), "bool")),
            Field::Null => out.push(("isInclusive", Value::Null, "bool")),
            Field::Absent => {}
        }
    }
    out
}

/// Bind satu nilai JSON polos ke query dinamis, dengan cast eksplisit.
pub(crate) fn push_value(qb: &mut QueryBuilder<'_, Postgres>, v: &Value, cast: &str) {
    match v {
        Value::Null => {
            qb.push("NULL");
        }
        Value::Bool(b) => {
            qb.push_bind(*b);
        }
        Value::Number(n) if cast == "int" => {
            qb.push_bind(n.as_i64().and_then(|n| i32::try_from(n).ok()));
        }
        Value::Number(n) => {
            qb.push_bind(n.to_string());
        }
        Value::String(s) => {
            qb.push_bind(s.clone());
        }
        other => {
            qb.push_bind(other.clone());
        }
    }
    if !cast.is_empty() && cast != "int" && cast != "bool" {
        qb.push("::").push(cast);
    }
}

async fn category_ref(state: &AppState, company_id: i32, id: i32) -> ApiResult<Option<MasterRow>> {
    find_by_id(&state.db, MasterKind::Categories, company_id, id).await
}

/// Node: src/products/product-masters.service.ts:368 `create`
pub async fn create(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    kind: MasterKind,
    dto: &MasterDto,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    assert_fields_allowed(kind, dto)?;
    assert_required_fields(kind, dto)?;
    let (Field::Val(code), Field::Val(name)) = (&dto.code, &dto.name) else {
        return Err(internal("code/name wajib"));
    };
    assert_name_length(kind, name)?;
    assert_default_conversion(kind, dto, None)?;
    let category_id = match &dto.category_id {
        Field::Val(c) if !c.is_empty() => {
            Some(resolve_active_category_id(&state.db, company_id, c).await?)
        }
        _ => None,
    };
    let extra = push_extra(kind, dto, category_id);
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(format!(
        r#"INSERT INTO {} ("companyId", code, name, "createdById""#,
        kind.table()
    ));
    for (col, _, _) in &extra {
        qb.push(format!(r#", "{col}""#));
    }
    qb.push(") VALUES (");
    qb.push_bind(company_id)
        .push(", ")
        .push_bind(code.clone())
        .push(", ")
        .push_bind(name.clone())
        .push(", ")
        .push_bind(user_id);
    for (_, v, cast) in &extra {
        qb.push(", ");
        push_value(&mut qb, v, cast);
    }
    qb.push(") RETURNING id");
    let id: i32 = match qb.build_query_scalar().fetch_one(&state.db).await {
        Ok(id) => id,
        Err(e) if mz_core::db::is_unique_violation(&e) => return Err(code_taken(kind, code)),
        Err(e) => return Err(e.into()),
    };
    if let Some(r) = audit
        .result_row(&state.db, kind.audit_model(), Lookup::Id(id))
        .await
    {
        audit.record_create(kind.audit_model(), &r).await;
    }
    let row = find_by_id(&state.db, kind, company_id, id)
        .await?
        .ok_or_else(|| internal("baris master baru hilang"))?;
    let cat = match category_id {
        Some(cid) => Some(category_ref(state, company_id, cid).await?),
        None => None,
    };
    Ok(to_entity(&row, kind, cat.as_ref().map(Option::as_ref)))
}

/// Query `ListProductMastersQueryDto`.
pub struct ListQuery {
    pub search: Option<String>,
    pub is_active: Option<bool>,
    pub category_id: Option<String>,
    pub page: i64,
    pub limit: i64,
}

pub fn parse_list(m: &Map<String, Value>) -> Result<ListQuery, Vec<String>> {
    let mut e = Vec::new();
    let search = opt_max_string(m, &mut e, "search", 120);
    let is_active = query_bool(m, &mut e, "isActive");
    let category_id = crate::dto::present(m, "categoryId")
        .and_then(|v| check_uuid4(&mut e, "categoryId", Some(v)));
    let (page, limit) = page_limit_max(m, &mut e, 100);
    if e.is_empty() {
        Ok(ListQuery {
            search,
            is_active,
            category_id,
            page,
            limit,
        })
    } else {
        Err(e)
    }
}

/// Node: src/products/product-masters.service.ts:403 `findAll`
pub async fn find_all(
    state: &AppState,
    user_id: i32,
    kind: MasterKind,
    q: &ListQuery,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let is_sub = kind == MasterKind::SubCategories;
    let category_id = match (&q.category_id, is_sub) {
        (Some(c), true) => Some(resolve_active_category_id(&state.db, company_id, c).await?),
        _ => None,
    };
    let filter = |qb: &mut QueryBuilder<'_, Postgres>| {
        qb.push(r#" WHERE "companyId" = "#).push_bind(company_id);
        if let Some(a) = q.is_active {
            qb.push(r#" AND "isActive" = "#).push_bind(a);
        }
        if let Some(c) = category_id {
            qb.push(r#" AND "categoryId" = "#).push_bind(c);
        }
        if let Some(s) = q.search.as_deref().filter(|s| !s.is_empty()) {
            let p = like_pattern(s);
            qb.push(" AND (code ILIKE ")
                .push_bind(p.clone())
                .push(" OR name ILIKE ")
                .push_bind(p.clone());
            if kind.has_description() {
                qb.push(" OR description ILIKE ").push_bind(p);
            }
            qb.push(")");
        }
    };
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(format!(
        "SELECT {} FROM {}",
        kind.select_list(),
        kind.table()
    ));
    filter(&mut qb);
    qb.push(format!(" ORDER BY {} LIMIT ", kind.order_by()))
        .push_bind(q.limit)
        .push(" OFFSET ")
        .push_bind(offset(q.page, q.limit));
    let rows: Vec<MasterRow> = qb.build_query_as().fetch_all(&state.db).await?;
    let mut cq: QueryBuilder<Postgres> =
        QueryBuilder::new(format!("SELECT COUNT(*) FROM {}", kind.table()));
    filter(&mut cq);
    let total: i64 = cq.build_query_scalar().fetch_one(&state.db).await?;
    let refs = if is_sub {
        map_by_ids(
            &state.db,
            MasterKind::Categories,
            company_id,
            rows.iter().filter_map(|r| r.category_id),
        )
        .await?
    } else {
        HashMap::new()
    };
    let data: Vec<Value> = rows
        .iter()
        .map(|r| {
            let cat = is_sub.then(|| r.category_id.and_then(|c| refs.get(&c)));
            to_entity(r, kind, cat)
        })
        .collect();
    Ok(json!({ "data": data, "meta": page_meta(total, q.page, q.limit) }))
}

/// Node: src/products/product-masters.service.ts:459 `findOne`
pub async fn find_one(
    state: &AppState,
    user_id: i32,
    kind: MasterKind,
    id: &str,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let row = find_row_or_throw(state, kind, company_id, id).await?;
    let cat = match (kind, row.category_id) {
        (MasterKind::SubCategories, Some(c)) => Some(category_ref(state, company_id, c).await?),
        _ => None,
    };
    Ok(to_entity(&row, kind, cat.as_ref().map(Option::as_ref)))
}

/// Node: src/products/product-masters.service.ts:469 `update`
pub async fn update(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    kind: MasterKind,
    id: &str,
    dto: &MasterDto,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let row = find_row_or_throw(state, kind, company_id, id).await?;
    assert_fields_allowed(kind, dto)?;
    if let Field::Val(n) = &dto.name {
        assert_name_length(kind, n)?;
    }
    assert_default_conversion(kind, dto, Some(&row))?;
    let is_sub = kind == MasterKind::SubCategories;
    let code_for_err = match &dto.code {
        Field::Val(c) => c.clone(),
        _ => row.code.clone(),
    };

    let mut tx = state.db.begin().await?;
    let category_id = match (&dto.category_id, is_sub) {
        (Field::Val(c), true) => Some(resolve_active_category_id(&mut *tx, company_id, c).await?),
        (Field::Null, true) => return Err(internal("categoryId null tidak bisa dicari")),
        _ => None,
    };
    let mut sets: Vec<(&'static str, Value, &'static str)> = Vec::new();
    match &dto.code {
        Field::Val(c) => sets.push(("code", json!(c), "")),
        Field::Null => sets.push(("code", Value::Null, "")),
        Field::Absent => {}
    }
    match &dto.name {
        Field::Val(c) => sets.push(("name", json!(c), "")),
        Field::Null => sets.push(("name", Value::Null, "")),
        Field::Absent => {}
    }
    match dto.is_active {
        Field::Val(b) => sets.push(("isActive", json!(b), "bool")),
        Field::Null => sets.push(("isActive", Value::Null, "bool")),
        Field::Absent => {}
    }
    sets.extend(push_extra(kind, dto, category_id));
    let pre = audit.pre_read(kind.audit_model(), Lookup::Id(row.id)).await;
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(format!("UPDATE {} SET ", kind.table()));
    for (col, v, cast) in &sets {
        qb.push(format!(r#""{col}" = "#));
        push_value(&mut qb, v, cast);
        qb.push(", ");
    }
    qb.push(r#""updatedAt" = "#).push_bind(now_naive());
    qb.push(" WHERE id = ").push_bind(row.id);
    match qb.build().execute(&mut *tx).await {
        Ok(_) => {}
        Err(e) if mz_core::db::is_unique_violation(&e) => {
            return Err(code_taken(kind, &code_for_err));
        }
        Err(e) => return Err(e.into()),
    }
    if let Some(r) = audit
        .result_row(&mut *tx, kind.audit_model(), Lookup::Id(row.id))
        .await
    {
        audit.record_update(kind.audit_model(), pre, &r).await;
    }
    let updated = find_by_id(&mut *tx, kind, company_id, row.id)
        .await?
        .ok_or_else(|| internal("baris master hilang"))?;
    tx.commit().await?;

    let final_cat = updated.category_id.or(row.category_id);
    let cat = match (is_sub, final_cat) {
        (true, Some(c)) => Some(category_ref(state, company_id, c).await?),
        _ => None,
    };
    Ok(to_entity(&updated, kind, cat.as_ref().map(Option::as_ref)))
}

/// Node: src/products/product-masters.service.ts:503 `remove` — arsip, bukan hapus.
pub async fn remove(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    kind: MasterKind,
    id: &str,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let row = find_row_or_throw(state, kind, company_id, id).await?;
    let pre = audit.pre_read(kind.audit_model(), Lookup::Id(row.id)).await;
    sqlx::query(&format!(
        r#"UPDATE {} SET "isActive" = false, "updatedAt" = $2 WHERE id = $1"#,
        kind.table()
    ))
    .bind(row.id)
    .bind(now_naive())
    .execute(&state.db)
    .await?;
    if let Some(r) = audit
        .result_row(&state.db, kind.audit_model(), Lookup::Id(row.id))
        .await
    {
        audit.record_update(kind.audit_model(), pre, &r).await;
    }
    Ok(json!({ "id": row.uuid, "archived": true }))
}

// ───────────────────────── handler ─────────────────────────

/// Node: src/products/admin-product-masters.controller.ts:100 `findAll`
pub async fn h_find_all(
    State(state): State<AppState>,
    ctx: RequestCtx,
    KindPath(kind): KindPath,
    ValidatedQuery(q): ValidatedQuery<Raw>,
) -> ApiResult<Json<Value>> {
    let q = parse_list(&q.0).map_err(ApiError::Validation)?;
    Ok(Json(
        find_all(&state, ctx.require_user_id()?, kind, &q).await?,
    ))
}

/// Node: src/products/admin-product-masters.controller.ts:127 `create`
pub async fn h_create(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    KindPath(kind): KindPath,
    ValidatedJson(body): ValidatedJson<Raw>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let dto = parse_master(&body.0, true).map_err(ApiError::Validation)?;
    let data = create(&state, &audit, ctx.require_user_id()?, kind, &dto).await?;
    Ok((StatusCode::CREATED, Json(json!({ "data": data }))))
}

/// Node: src/products/admin-product-masters.controller.ts:164 `findOne`
pub async fn h_find_one(
    State(state): State<AppState>,
    ctx: RequestCtx,
    KindIdPath(kind, id): KindIdPath,
) -> ApiResult<Json<Value>> {
    let data = find_one(&state, ctx.require_user_id()?, kind, &id).await?;
    Ok(Json(json!({ "data": data })))
}

/// Node: src/products/admin-product-masters.controller.ts:180 `update`
pub async fn h_update(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    KindIdPath(kind, id): KindIdPath,
    ValidatedJson(body): ValidatedJson<Raw>,
) -> ApiResult<Json<Value>> {
    let dto = parse_master(&body.0, false).map_err(ApiError::Validation)?;
    let data = update(&state, &audit, ctx.require_user_id()?, kind, &id, &dto).await?;
    Ok(Json(json!({ "data": data })))
}

/// Node: src/products/admin-product-masters.controller.ts:211 `remove`
pub async fn h_remove(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    KindIdPath(kind, id): KindIdPath,
) -> ApiResult<Json<Value>> {
    let data = remove(&state, &audit, ctx.require_user_id()?, kind, &id).await?;
    Ok(Json(json!({ "data": data })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_parse_dan_tolak_field() {
        assert_eq!(
            MasterKind::parse("sub-categories"),
            Some(MasterKind::SubCategories)
        );
        assert_eq!(MasterKind::parse("subCategories"), None);
        let m = json!({"code": "bev", "name": "Beverages", "rate": "11"});
        let dto = parse_master(m.as_object().unwrap(), true).unwrap();
        assert_eq!(dto.code, Field::Val("BEV".into()));
        let err = assert_fields_allowed(MasterKind::Categories, &dto).unwrap_err();
        assert_eq!(err.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert!(assert_fields_allowed(MasterKind::Taxes, &dto).is_ok());
        let m = json!({"code": "PPN", "name": "PPN"});
        let dto = parse_master(m.as_object().unwrap(), true).unwrap();
        assert!(assert_required_fields(MasterKind::Taxes, &dto).is_err());
    }

    #[test]
    fn pesan_validasi_create() {
        let m = json!({"code": 5, "sortOrder": "x", "email": "bad"});
        let e = parse_master(m.as_object().unwrap(), true).unwrap_err();
        assert_eq!(
            e,
            vec![
                CODE_MSG.to_owned(),
                "code must be a string".into(),
                "name must be longer than or equal to 1 characters".into(),
                "name must be a string".into(),
                "sortOrder must not be less than 0".into(),
                "sortOrder must be an integer number".into(),
                "email must be an email".into(),
            ]
        );
    }

    #[test]
    fn konversi_default_uom() {
        let m = json!({"defaultOperator": "divide", "defaultFactor": "3000000"});
        let dto = parse_master(m.as_object().unwrap(), false).unwrap();
        assert!(assert_default_conversion(MasterKind::Uoms, &dto, None).is_err());
        assert!(assert_default_conversion(MasterKind::Taxes, &dto, None).is_ok());
    }
}
