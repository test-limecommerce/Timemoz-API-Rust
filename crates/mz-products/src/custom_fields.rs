//! Port `products/admin-product-custom-fields.controller.ts`, `product-custom-fields.service.ts`,
//! dan `product-custom-field.delegate.ts`: DEFINISI custom field produk di route
//! `/v1/admin/product-custom-fields`, dan pembaca definisi untuk validasi nilai.

use std::collections::HashMap;

use axum::extract::{Path, State};
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router, http::StatusCode};
use chrono::NaiveDateTime;
use mz_audit::{AuditHandle, Lookup, Model};
use mz_core::company::require_company_id;
use mz_core::extract::{ValidatedJson, ValidatedQuery};
use mz_core::gate::guard;
use mz_core::time::now_naive;
use mz_core::{ApiError, ApiResult, AppState, RequestCtx};
use serde_json::{Map, Value, json};
use sqlx::{PgConnection, PgExecutor, Postgres, QueryBuilder};

use crate::common::{
    conflict, like_pattern, ms, ms_opt, not_found, offset, page_meta, row_uuid, unprocessable,
};
use crate::custom_fields_validator::{CUSTOM_FIELD_TYPES, CfDefinition, CfOption};
use crate::dto::{
    CUSTOM_FIELD_KEY, Field, Raw, check_bool, check_enum, check_length_string, check_matches,
    check_parse_int_min, opt_max_string, page_limit_max, query_bool,
};

/// Baris `productCustomField` (semua kolom skalar).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CustomFieldRow {
    pub id: i32,
    pub uuid: String,
    #[sqlx(rename = "companyId")]
    pub company_id: i32,
    pub key: String,
    pub label: String,
    #[sqlx(rename = "type")]
    pub field_type: String,
    pub options: Option<Value>,
    #[sqlx(rename = "isRequired")]
    pub is_required: bool,
    #[sqlx(rename = "isSensitive")]
    pub is_sensitive: bool,
    #[sqlx(rename = "sortOrder")]
    pub sort_order: i32,
    #[sqlx(rename = "isActive")]
    pub is_active: bool,
    #[sqlx(rename = "createdAt")]
    pub created_at: NaiveDateTime,
    #[sqlx(rename = "updatedAt")]
    pub updated_at: Option<NaiveDateTime>,
}

const COLS: &str = r#"id, uuid::text AS uuid, "companyId", key, label, type::text AS type, options,
    "isRequired", "isSensitive", "sortOrder", "isActive", "createdAt", "updatedAt""#;

impl CustomFieldRow {
    pub fn definition(&self) -> CfDefinition {
        CfDefinition {
            key: self.key.clone(),
            field_type: self.field_type.clone(),
            is_required: self.is_required,
            is_active: self.is_active,
            options: CfDefinition::options_from_json(self.options.as_ref()),
        }
    }
}

/// `toEntity(row)`.
pub fn to_entity(row: &CustomFieldRow) -> Value {
    json!({
        "id": row.uuid,
        "key": row.key,
        "label": row.label,
        "type": row.field_type,
        "options": row.options.clone().unwrap_or(Value::Null),
        "isRequired": row.is_required,
        "isSensitive": row.is_sensitive,
        "sortOrder": row.sort_order,
        "isActive": row.is_active,
        "createdAt": ms(row.created_at),
        "updatedAt": ms_opt(row.updated_at),
    })
}

// ───────────────────────── pembaca untuk modul lain ─────────────────────────

/// Semua definisi `entity = product` company (aktif + terarsip), urut id — bentuk
/// `activeAndArchivedDefinitions` (products.service.ts:520) untuk `validate_custom_field_values`.
pub async fn definitions<'e>(
    exec: impl PgExecutor<'e>,
    company_id: i32,
) -> ApiResult<Vec<CfDefinition>> {
    let rows: Vec<CustomFieldRow> = sqlx::query_as(&format!(
        r#"SELECT {COLS} FROM "productCustomField"
           WHERE "companyId" = $1 AND entity = 'product' ORDER BY id LIMIT 1000"#
    ))
    .bind(company_id)
    .fetch_all(exec)
    .await?;
    Ok(rows.iter().map(CustomFieldRow::definition).collect())
}

/// `activeCustomFieldTypeByKey` (products.service.ts:538): key → type definisi AKTIF.
pub async fn active_types<'e>(
    exec: impl PgExecutor<'e>,
    company_id: i32,
) -> ApiResult<HashMap<String, String>> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        r#"SELECT key, type::text FROM "productCustomField"
           WHERE "companyId" = $1 AND entity = 'product' AND "isActive" ORDER BY id LIMIT 1000"#,
    )
    .bind(company_id)
    .fetch_all(exec)
    .await?;
    Ok(rows.into_iter().collect())
}

/// `bumpCatalogEpoch(tx, companyId)` — `company.catalogEpoch += 1` di transaksi yang sama dengan
/// penulisan definisi (§4.2). `company` ber-audit (Prisma middleware Node ikut mencatatnya).
pub async fn bump_catalog_epoch(
    conn: &mut PgConnection,
    audit: &AuditHandle,
    company_id: i32,
) -> ApiResult<()> {
    let pre = audit.pre_read(Model::Company, Lookup::Id(company_id)).await;
    sqlx::query(r#"UPDATE company SET "catalogEpoch" = "catalogEpoch" + 1 WHERE id = $1"#)
        .bind(company_id)
        .execute(&mut *conn)
        .await?;
    if let Some(r) = audit
        .result_row(&mut *conn, Model::Company, Lookup::Id(company_id))
        .await
    {
        audit.record_update(Model::Company, pre, &r).await;
    }
    Ok(())
}

// ───────────────────────── DTO ─────────────────────────

#[derive(Debug)]
pub struct CfDto {
    pub key: Field<String>,
    pub label: Field<String>,
    pub field_type: Field<String>,
    pub options: Field<Vec<CfOption>>,
    pub is_required: Field<bool>,
    pub is_sensitive: Field<bool>,
    pub sort_order: Field<i64>,
    pub is_active: Field<bool>,
}

const KEY_MSG: &str = "key must start with a lowercase letter, followed by up to 39 lowercase letters, digits, or underscores";
const KEY_DEFAULT_MSG: &str = "key must match /^[a-z][a-z0-9_]{0,39}$/ regular expression";

fn tri<T>(
    m: &Map<String, Value>,
    key: &str,
    required: bool,
    check: impl FnOnce(Option<&Value>) -> Option<T>,
) -> Field<T> {
    match m.get(key) {
        None if !required => Field::Absent,
        Some(Value::Null) if !required => Field::Null,
        v => check(v).map_or(Field::Absent, Field::Val),
    }
}

/// `@IsArray() @ArrayMaxSize(200) @ValidateNested({each}) @Type(CustomFieldOptionDto)`.
pub fn check_options(e: &mut Vec<String>, v: Option<&Value>) -> Option<Vec<CfOption>> {
    let Some(Value::Array(items)) = v else {
        e.push("options must contain no more than 200 elements".into());
        e.push("options must be an array".into());
        if !matches!(v, Some(Value::Object(_))) {
            e.push(crate::dto::nested_not_object("options"));
        }
        return None;
    };
    let mut child = Vec::new();
    let mut out = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let Value::Object(o) = item else {
            child.push(format!(
                "options.{i}.nested property {i} must be either object or array"
            ));
            continue;
        };
        let value = check_length_string(
            &mut child,
            &format!("options.{i}.value"),
            o.get("value"),
            1,
            120,
        );
        let label = check_length_string(
            &mut child,
            &format!("options.{i}.label"),
            o.get("label"),
            1,
            120,
        );
        if let (Some(value), Some(label)) = (value, label) {
            out.push(CfOption { value, label });
        }
    }
    // Nest: konstrain milik properti induk dibuang bila ada galat anak.
    if !child.is_empty() {
        e.extend(child);
        return None;
    }
    if items.len() > 200 {
        e.push("options must contain no more than 200 elements".into());
        return None;
    }
    Some(out)
}

/// `CreateProductCustomFieldDto` (`create`) / `UpdateProductCustomFieldDto`.
pub fn parse_dto(m: &Map<String, Value>, create: bool) -> Result<CfDto, Vec<String>> {
    let mut e = Vec::new();
    let key = tri(m, "key", create, |v| {
        check_matches(
            &mut e,
            "key",
            v,
            &CUSTOM_FIELD_KEY,
            if create { KEY_MSG } else { KEY_DEFAULT_MSG },
        )
    });
    let label = tri(m, "label", create, |v| {
        check_length_string(&mut e, "label", v, 1, 120)
    });
    let field_type = tri(m, "type", create, |v| {
        check_enum(&mut e, "type", v, CUSTOM_FIELD_TYPES)
    });
    let options = tri(m, "options", false, |v| check_options(&mut e, v));
    let is_required = tri(m, "isRequired", false, |v| {
        check_bool(&mut e, "isRequired", v)
    });
    let is_sensitive = tri(m, "isSensitive", false, |v| {
        check_bool(&mut e, "isSensitive", v)
    });
    let sort_order = match m.get("sortOrder") {
        None => Field::Absent,
        Some(v) => check_parse_int_min(&mut e, "sortOrder", v, 0).map_or(Field::Absent, Field::Val),
    };
    let is_active = if create {
        Field::Absent
    } else {
        tri(m, "isActive", false, |v| check_bool(&mut e, "isActive", v))
    };
    if e.is_empty() {
        Ok(CfDto {
            key,
            label,
            field_type,
            options,
            is_required,
            is_sensitive,
            sort_order,
            is_active,
        })
    } else {
        Err(e)
    }
}

// ───────────────────────── service ─────────────────────────

fn is_option_type(t: &str) -> bool {
    matches!(t, "select" | "multiselect")
}

/// `assertOptionsForType`.
fn assert_options_for_type(field_type: &str, options: &Field<Vec<CfOption>>) -> ApiResult<()> {
    let needs = is_option_type(field_type);
    let empty = match options {
        Field::Val(v) => v.is_empty(),
        _ => true,
    };
    if needs && empty {
        return Err(unprocessable(
            "CUSTOM_FIELD_OPTIONS_REQUIRED",
            "options is required for select/multiselect fields",
        ));
    }
    if !needs && options.is_defined() {
        return Err(unprocessable(
            "CUSTOM_FIELD_OPTIONS_NOT_ALLOWED",
            "options is only accepted for select/multiselect fields",
        ));
    }
    Ok(())
}

/// `assertOptionsAppendOnly` (§0.8).
fn assert_options_append_only(existing: Option<&Value>, next: &[CfOption]) -> ApiResult<()> {
    let by_value: HashMap<&str, &str> = next
        .iter()
        .map(|o| (o.value.as_str(), o.label.as_str()))
        .collect();
    for o in CfDefinition::options_from_json(existing).unwrap_or_default() {
        if by_value.get(o.value.as_str()) != Some(&o.label.as_str()) {
            return Err(unprocessable(
                "CUSTOM_FIELD_OPTIONS_IMMUTABLE",
                format!(
                    "existing option \"{}\" cannot be removed or relabeled — options can only be appended (§0.8)",
                    o.value
                ),
            ));
        }
    }
    Ok(())
}

fn options_json(o: &Field<Vec<CfOption>>) -> Option<Value> {
    match o {
        Field::Val(v) => Some(json!(v)),
        _ => None,
    }
}

async fn find_row_or_throw(
    state: &AppState,
    company_id: i32,
    raw: &str,
) -> ApiResult<CustomFieldRow> {
    let row: Option<CustomFieldRow> = sqlx::query_as(&format!(
        r#"SELECT {COLS} FROM "productCustomField" WHERE uuid = $1 AND "companyId" = $2 LIMIT 1"#
    ))
    .bind(row_uuid(raw)?)
    .bind(company_id)
    .fetch_optional(&state.db)
    .await?;
    row.ok_or_else(|| {
        not_found(
            "CUSTOM_FIELD_NOT_FOUND",
            "Custom field definition not found",
        )
    })
}

async fn find_by_id<'e>(exec: impl PgExecutor<'e>, id: i32) -> ApiResult<CustomFieldRow> {
    Ok(sqlx::query_as(&format!(
        r#"SELECT {COLS} FROM "productCustomField" WHERE id = $1"#
    ))
    .bind(id)
    .fetch_one(exec)
    .await?)
}

/// Node: src/products/product-custom-fields.service.ts:146 `create`
pub async fn create(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    dto: &CfDto,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let (Field::Val(key), Field::Val(label), Field::Val(field_type)) =
        (&dto.key, &dto.label, &dto.field_type)
    else {
        return Err(crate::common::internal("key/label/type wajib"));
    };
    assert_options_for_type(field_type, &dto.options)?;
    let mut tx = state.db.begin().await?;
    let inserted = sqlx::query_scalar::<_, i32>(
        r#"INSERT INTO "productCustomField"
             ("companyId", entity, key, label, type, options, "isRequired", "isSensitive", "sortOrder", "createdById")
           VALUES ($1, 'product', $2, $3, $4::"CustomFieldType", $5, $6, $7, $8, $9) RETURNING id"#,
    )
    .bind(company_id)
    .bind(key)
    .bind(label)
    .bind(field_type)
    .bind(options_json(&dto.options))
    .bind(matches!(dto.is_required, Field::Val(true)))
    .bind(matches!(dto.is_sensitive, Field::Val(true)))
    .bind(match dto.sort_order {
        Field::Val(n) => i32::try_from(n).unwrap_or(i32::MAX),
        _ => 0,
    })
    .bind(user_id)
    .fetch_one(&mut *tx)
    .await;
    let id = match inserted {
        Ok(id) => id,
        Err(e) if mz_core::db::is_unique_violation(&e) => {
            return Err(conflict(
                "CUSTOM_FIELD_KEY_TAKEN",
                format!("key \"{key}\" is already used in this company"),
            ));
        }
        Err(e) => return Err(e.into()),
    };
    if let Some(r) = audit
        .result_row(&mut *tx, Model::ProductCustomField, Lookup::Id(id))
        .await
    {
        audit.record_create(Model::ProductCustomField, &r).await;
    }
    bump_catalog_epoch(&mut tx, audit, company_id).await?;
    let row = find_by_id(&mut *tx, id).await?;
    tx.commit().await?;
    Ok(to_entity(&row))
}

pub struct ListQuery {
    pub search: Option<String>,
    pub is_active: Option<bool>,
    pub page: i64,
    pub limit: i64,
}

pub fn parse_list(m: &Map<String, Value>) -> Result<ListQuery, Vec<String>> {
    let mut e = Vec::new();
    let search = opt_max_string(m, &mut e, "search", 120);
    let is_active = query_bool(m, &mut e, "isActive");
    let (page, limit) = page_limit_max(m, &mut e, 100);
    if e.is_empty() {
        Ok(ListQuery {
            search,
            is_active,
            page,
            limit,
        })
    } else {
        Err(e)
    }
}

/// Node: src/products/product-custom-fields.service.ts:183 `findAll`
pub async fn find_all(state: &AppState, user_id: i32, q: &ListQuery) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let filter = |qb: &mut QueryBuilder<'_, Postgres>| {
        qb.push(r#" WHERE "companyId" = "#)
            .push_bind(company_id)
            .push(" AND entity = 'product'");
        if let Some(a) = q.is_active {
            qb.push(r#" AND "isActive" = "#).push_bind(a);
        }
        if let Some(s) = q.search.as_deref().filter(|s| !s.is_empty()) {
            let p = like_pattern(s);
            qb.push(" AND (key ILIKE ")
                .push_bind(p.clone())
                .push(" OR label ILIKE ")
                .push_bind(p)
                .push(")");
        }
    };
    let mut qb: QueryBuilder<Postgres> =
        QueryBuilder::new(format!(r#"SELECT {COLS} FROM "productCustomField""#));
    filter(&mut qb);
    qb.push(r#" ORDER BY "sortOrder" ASC, label ASC, id ASC LIMIT "#)
        .push_bind(q.limit)
        .push(" OFFSET ")
        .push_bind(offset(q.page, q.limit));
    let rows: Vec<CustomFieldRow> = qb.build_query_as().fetch_all(&state.db).await?;
    let mut cq: QueryBuilder<Postgres> =
        QueryBuilder::new(r#"SELECT COUNT(*) FROM "productCustomField""#);
    filter(&mut cq);
    let total: i64 = cq.build_query_scalar().fetch_one(&state.db).await?;
    Ok(json!({
        "data": rows.iter().map(to_entity).collect::<Vec<_>>(),
        "meta": page_meta(total, q.page, q.limit),
    }))
}

/// Node: src/products/product-custom-fields.service.ts:226 `update`
pub async fn update(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    id: &str,
    dto: &CfDto,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let row = find_row_or_throw(state, company_id, id).await?;
    if matches!(&dto.key, Field::Val(k) if *k != row.key) || matches!(dto.key, Field::Null) {
        return Err(unprocessable(
            "CUSTOM_FIELD_IMMUTABLE",
            "key cannot be changed after the field is created",
        ));
    }
    if matches!(&dto.field_type, Field::Val(t) if *t != row.field_type)
        || matches!(dto.field_type, Field::Null)
    {
        return Err(unprocessable(
            "CUSTOM_FIELD_IMMUTABLE",
            "type cannot be changed after the field is created",
        ));
    }
    if dto.options.is_defined() {
        assert_options_for_type(&row.field_type, &dto.options)?;
        if let Field::Val(next) = &dto.options {
            assert_options_append_only(row.options.as_ref(), next)?;
        }
    }
    let mut tx = state.db.begin().await?;
    let pre = audit
        .pre_read(Model::ProductCustomField, Lookup::Id(row.id))
        .await;
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(r#"UPDATE "productCustomField" SET "#);
    if let Field::Val(l) = &dto.label {
        qb.push("label = ").push_bind(l.clone()).push(", ");
    } else if matches!(dto.label, Field::Null) {
        qb.push("label = NULL, ");
    }
    if let Field::Val(_) = &dto.options {
        qb.push("options = ")
            .push_bind(options_json(&dto.options))
            .push(", ");
    }
    for (col, f) in [
        ("isRequired", &dto.is_required),
        ("isSensitive", &dto.is_sensitive),
        ("isActive", &dto.is_active),
    ] {
        match f {
            Field::Val(b) => {
                qb.push(format!(r#""{col}" = "#)).push_bind(*b).push(", ");
            }
            Field::Null => {
                qb.push(format!(r#""{col}" = NULL, "#));
            }
            Field::Absent => {}
        }
    }
    if let Field::Val(n) = dto.sort_order {
        qb.push(r#""sortOrder" = "#)
            .push_bind(i32::try_from(n).unwrap_or(i32::MAX))
            .push(", ");
    }
    qb.push(r#""updatedAt" = "#)
        .push_bind(now_naive())
        .push(" WHERE id = ")
        .push_bind(row.id);
    qb.build().execute(&mut *tx).await?;
    if let Some(r) = audit
        .result_row(&mut *tx, Model::ProductCustomField, Lookup::Id(row.id))
        .await
    {
        audit
            .record_update(Model::ProductCustomField, pre, &r)
            .await;
    }
    bump_catalog_epoch(&mut tx, audit, company_id).await?;
    let updated = find_by_id(&mut *tx, row.id).await?;
    tx.commit().await?;
    Ok(to_entity(&updated))
}

/// Node: src/products/product-custom-fields.service.ts:262 `remove` — arsip.
pub async fn remove(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    id: &str,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let row = find_row_or_throw(state, company_id, id).await?;
    let mut tx = state.db.begin().await?;
    let pre = audit
        .pre_read(Model::ProductCustomField, Lookup::Id(row.id))
        .await;
    sqlx::query(
        r#"UPDATE "productCustomField" SET "isActive" = false, "updatedAt" = $2 WHERE id = $1"#,
    )
    .bind(row.id)
    .bind(now_naive())
    .execute(&mut *tx)
    .await?;
    if let Some(r) = audit
        .result_row(&mut *tx, Model::ProductCustomField, Lookup::Id(row.id))
        .await
    {
        audit
            .record_update(Model::ProductCustomField, pre, &r)
            .await;
    }
    bump_catalog_epoch(&mut tx, audit, company_id).await?;
    tx.commit().await?;
    Ok(json!({ "id": row.uuid, "archived": true }))
}

// ───────────────────────── handler ─────────────────────────

/// Node: src/products/admin-product-custom-fields.controller.ts:84 `findAll`
async fn h_find_all(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedQuery(q): ValidatedQuery<Raw>,
) -> ApiResult<Json<Value>> {
    let q = parse_list(&q.0).map_err(ApiError::Validation)?;
    Ok(Json(find_all(&state, ctx.require_user_id()?, &q).await?))
}

/// Node: src/products/admin-product-custom-fields.controller.ts:102 `create`
async fn h_create(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    ValidatedJson(body): ValidatedJson<Raw>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let dto = parse_dto(&body.0, true).map_err(ApiError::Validation)?;
    let data = create(&state, &audit, ctx.require_user_id()?, &dto).await?;
    Ok((StatusCode::CREATED, Json(json!({ "data": data }))))
}

/// Node: src/products/admin-product-custom-fields.controller.ts:129 `findOne`
async fn h_find_one(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let company_id = require_company_id(&state.db, ctx.require_user_id()?).await?;
    let row = find_row_or_throw(&state, company_id, &id).await?;
    Ok(Json(json!({ "data": to_entity(&row) })))
}

/// Node: src/products/admin-product-custom-fields.controller.ts:140 `update`
async fn h_update(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    Path(id): Path<String>,
    ValidatedJson(body): ValidatedJson<Raw>,
) -> ApiResult<Json<Value>> {
    let dto = parse_dto(&body.0, false).map_err(ApiError::Validation)?;
    let data = update(&state, &audit, ctx.require_user_id()?, &id, &dto).await?;
    Ok(Json(json!({ "data": data })))
}

/// Node: src/products/admin-product-custom-fields.controller.ts:168 `remove`
async fn h_remove(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let data = remove(&state, &audit, ctx.require_user_id()?, &id).await?;
    Ok(Json(json!({ "data": data })))
}

/// 5 route `/v1/admin/product-custom-fields`.
pub fn routes(state: &AppState) -> Router<AppState> {
    const P: &str = "/v1/admin/product-custom-fields";
    const PI: &str = "/v1/admin/product-custom-fields/{id}";
    let spec = crate::PRODUCTS;
    Router::new()
        .route(P, guard(state, spec, get(h_find_all)))
        .route(P, guard(state, spec, post(h_create)))
        .route(PI, guard(state, spec, get(h_find_one)))
        .route(PI, guard(state, spec, patch(h_update)))
        .route(PI, guard(state, spec, delete(h_remove)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dto_create() {
        let m = json!({"key": "Bad", "label": "", "type": "x", "options": [{"value": ""}]});
        let e = parse_dto(m.as_object().unwrap(), true).unwrap_err();
        assert_eq!(
            e,
            vec![
                KEY_MSG.to_owned(),
                "label must be longer than or equal to 1 characters".into(),
                "type must be one of the following values: text, number, date, boolean, select, multiselect".into(),
                "options.0.value must be longer than or equal to 1 characters".into(),
                "options.0.label must be longer than or equal to 1 characters".into(),
                "options.0.label must be a string".into(),
            ]
        );
    }

    #[test]
    fn opsi_hanya_menambah() {
        let existing = json!([{"value": "dry", "label": "Dry"}]);
        let ok = vec![
            CfOption {
                value: "dry".into(),
                label: "Dry".into(),
            },
            CfOption {
                value: "cold".into(),
                label: "Cold".into(),
            },
        ];
        assert!(assert_options_append_only(Some(&existing), &ok).is_ok());
        let bad = vec![CfOption {
            value: "dry".into(),
            label: "DRY".into(),
        }];
        assert!(assert_options_append_only(Some(&existing), &bad).is_err());
        assert!(assert_options_for_type("text", &Field::Absent).is_ok());
        assert!(assert_options_for_type("select", &Field::Absent).is_err());
        assert!(assert_options_for_type("text", &Field::Null).is_err());
    }
}
