//! Port bagian order dari `src/custom-fields/**` (`custom-fields.service.ts`,
//! `order-custom-field.delegate.ts`) + `orders/admin-order-custom-fields.controller.ts`:
//! DEFINISI custom field `order`/`orderItem` (`orderCustomFieldDefinition`) di
//! `/v1/admin/order-custom-fields`, dan pembaca definisi untuk validasi nilai.
//! Validator NILAI = `mz_products::custom_fields_validator` (generik, `CfEntity::{Order,OrderItem}`).

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
use mz_products::custom_fields::bump_catalog_epoch;
// `check_options` (`OrderCustomFieldOptionDto`): aturan sama dengan produk.
use mz_products::custom_fields::check_options;
use mz_products::custom_fields_validator::{CUSTOM_FIELD_TYPES, CfDefinition, CfEntity, CfOption};
use mz_products::dto::{
    CUSTOM_FIELD_KEY, Field, Raw, check_bool, check_enum, check_length_string, check_matches,
    check_parse_int_min, opt_max_string, page_limit_max, query_bool,
};
use serde_json::{Map, Value, json};
use sqlx::{PgExecutor, Postgres, QueryBuilder};

use crate::common::{
    conflict, internal, like_pattern, ms, ms_opt, not_found, offset, page_meta, row_uuid,
    unprocessable,
};

/// `OrderCustomFieldEntity`.
pub const ORDER_CUSTOM_FIELD_ENTITIES: &[&str] = &["order", "orderItem"];

/// Baris `orderCustomFieldDefinition` (semua kolom skalar).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct OrderCfRow {
    pub id: i32,
    pub uuid: String,
    #[sqlx(rename = "companyId")]
    pub company_id: i32,
    pub entity: String,
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

pub const COLS: &str = r#"id, uuid::text AS uuid, "companyId", entity::text AS entity, key, label, type::text AS type,
    options, "isRequired", "isSensitive", "sortOrder", "isActive", "createdAt", "updatedAt""#;

impl OrderCfRow {
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

/// `toEntity(row)` — waktu epoch-ms (admin CRUD + config mobile).
pub fn to_entity(row: &OrderCfRow) -> Value {
    json!({
        "id": row.uuid,
        "entity": row.entity,
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

/// Entity `CfEntity` untuk diskriminator.
pub fn cf_entity(entity: &str) -> CfEntity {
    if entity == "orderItem" {
        CfEntity::OrderItem
    } else {
        CfEntity::Order
    }
}

// ───────────────────────── pembaca untuk modul lain ─────────────────────────

/// `orderCustomFieldDefinition.findMany({ where: { companyId, entity } })` — aktif + terarsip.
pub async fn definitions<'e>(
    exec: impl PgExecutor<'e>,
    company_id: i32,
    entity: &str,
) -> ApiResult<Vec<CfDefinition>> {
    let rows: Vec<OrderCfRow> = sqlx::query_as(&format!(
        r#"SELECT {COLS} FROM "orderCustomFieldDefinition"
           WHERE "companyId" = $1 AND entity = $2::"OrderCustomFieldEntity" ORDER BY id"#
    ))
    .bind(company_id)
    .bind(entity)
    .fetch_all(exec)
    .await?;
    Ok(rows.iter().map(OrderCfRow::definition).collect())
}

/// Definisi aktif `entity` urut `sortOrder` (config admin/mobile); `exclude_sensitive` = §0.13 mobile.
pub async fn active_rows<'e>(
    exec: impl PgExecutor<'e>,
    company_id: i32,
    entity: &str,
    exclude_sensitive: bool,
) -> ApiResult<Vec<OrderCfRow>> {
    Ok(sqlx::query_as(&format!(
        r#"SELECT {COLS} FROM "orderCustomFieldDefinition"
           WHERE "companyId" = $1 AND entity = $2::"OrderCustomFieldEntity" AND "isActive" = true
             AND ($3 = false OR "isSensitive" = false)
           ORDER BY "sortOrder" ASC, id ASC"#
    ))
    .bind(company_id)
    .bind(entity)
    .bind(exclude_sensitive)
    .fetch_all(exec)
    .await?)
}

/// `{ key, label }` definisi `order` non-sensitif urut `sortOrder` (kolom export, §0.13).
pub async fn export_columns(
    db: &sqlx::PgPool,
    company_id: i32,
) -> ApiResult<Vec<(String, String)>> {
    Ok(sqlx::query_as(
        r#"SELECT key, label FROM "orderCustomFieldDefinition"
           WHERE "companyId" = $1 AND entity = 'order' AND "isSensitive" = false
           ORDER BY "sortOrder" ASC, id ASC"#,
    )
    .bind(company_id)
    .fetch_all(db)
    .await?)
}

// ───────────────────────── DTO ─────────────────────────

#[derive(Debug)]
pub struct CfDto {
    pub entity: Field<String>,
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

/// `CreateOrderCustomFieldDto` (`create`) / `UpdateOrderCustomFieldDto`.
pub fn parse_dto(m: &Map<String, Value>, create: bool) -> Result<CfDto, Vec<String>> {
    let mut e = Vec::new();
    let entity = if create {
        tri(m, "entity", true, |v| {
            check_enum(&mut e, "entity", v, ORDER_CUSTOM_FIELD_ENTITIES)
        })
    } else {
        Field::Absent
    };
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
            entity,
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

/// `assertOptionsAppendOnly` (§0.13).
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
                    "existing option \"{}\" cannot be removed or relabeled — options can only be appended",
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

async fn find_row_or_throw(state: &AppState, company_id: i32, raw: &str) -> ApiResult<OrderCfRow> {
    let row: Option<OrderCfRow> = sqlx::query_as(&format!(
        r#"SELECT {COLS} FROM "orderCustomFieldDefinition" WHERE uuid = $1 AND "companyId" = $2 LIMIT 1"#
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

async fn find_by_id<'e>(exec: impl PgExecutor<'e>, id: i32) -> ApiResult<OrderCfRow> {
    Ok(sqlx::query_as(&format!(
        r#"SELECT {COLS} FROM "orderCustomFieldDefinition" WHERE id = $1"#
    ))
    .bind(id)
    .fetch_one(exec)
    .await?)
}

const MODEL: Model = Model::OrderCustomFieldDefinition;

/// Node: src/custom-fields/custom-fields.service.ts:124 `create`
pub async fn create(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    dto: &CfDto,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let (Field::Val(entity), Field::Val(key), Field::Val(label), Field::Val(field_type)) =
        (&dto.entity, &dto.key, &dto.label, &dto.field_type)
    else {
        return Err(internal("entity/key/label/type wajib"));
    };
    assert_options_for_type(field_type, &dto.options)?;
    let mut tx = state.db.begin().await?;
    let inserted = sqlx::query_scalar::<_, i32>(
        r#"INSERT INTO "orderCustomFieldDefinition"
             ("companyId", entity, key, label, type, options, "isRequired", "isSensitive", "sortOrder", "createdById")
           VALUES ($1, $2::"OrderCustomFieldEntity", $3, $4, $5::"CustomFieldType", $6, $7, $8, $9, $10) RETURNING id"#,
    )
    .bind(company_id)
    .bind(entity)
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
                format!("key \"{key}\" is already used for {entity} in this company"),
            ));
        }
        Err(e) => return Err(e.into()),
    };
    if let Some(r) = audit.result_row(&mut *tx, MODEL, Lookup::Id(id)).await {
        audit.record_create(MODEL, &r).await;
    }
    bump_catalog_epoch(&mut tx, audit, company_id).await?;
    let row = find_by_id(&mut *tx, id).await?;
    tx.commit().await?;
    Ok(to_entity(&row))
}

/// `ListOrderCustomFieldsQueryDto`.
pub struct ListQuery {
    pub entity: String,
    pub search: Option<String>,
    pub is_active: Option<bool>,
    pub page: i64,
    pub limit: i64,
}

pub fn parse_list(m: &Map<String, Value>) -> Result<ListQuery, Vec<String>> {
    let mut e = Vec::new();
    let entity = check_enum(
        &mut e,
        "entity",
        m.get("entity"),
        ORDER_CUSTOM_FIELD_ENTITIES,
    );
    let search = opt_max_string(m, &mut e, "search", 120);
    let is_active = query_bool(m, &mut e, "isActive");
    let (page, limit) = page_limit_max(m, &mut e, 100);
    match entity {
        Some(entity) if e.is_empty() => Ok(ListQuery {
            entity,
            search,
            is_active,
            page,
            limit,
        }),
        _ => Err(e),
    }
}

/// Node: src/custom-fields/custom-fields.service.ts:159 `findAll`
pub async fn find_all(state: &AppState, user_id: i32, q: &ListQuery) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let filter = |qb: &mut QueryBuilder<'_, Postgres>| {
        qb.push(r#" WHERE "companyId" = "#)
            .push_bind(company_id)
            .push(" AND entity = ")
            .push_bind(q.entity.clone())
            .push(r#"::"OrderCustomFieldEntity""#);
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
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(format!(
        r#"SELECT {COLS} FROM "orderCustomFieldDefinition""#
    ));
    filter(&mut qb);
    qb.push(r#" ORDER BY "sortOrder" ASC, label ASC, id ASC LIMIT "#)
        .push_bind(q.limit)
        .push(" OFFSET ")
        .push_bind(offset(q.page, q.limit));
    let rows: Vec<OrderCfRow> = qb.build_query_as().fetch_all(&state.db).await?;
    let mut cq: QueryBuilder<Postgres> =
        QueryBuilder::new(r#"SELECT COUNT(*) FROM "orderCustomFieldDefinition""#);
    filter(&mut cq);
    let total: i64 = cq.build_query_scalar().fetch_one(&state.db).await?;
    Ok(json!({
        "data": rows.iter().map(to_entity).collect::<Vec<_>>(),
        "meta": page_meta(total, q.page, q.limit),
    }))
}

/// Node: src/custom-fields/custom-fields.service.ts:197 `update`
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
    let pre = audit.pre_read(MODEL, Lookup::Id(row.id)).await;
    let mut qb: QueryBuilder<Postgres> =
        QueryBuilder::new(r#"UPDATE "orderCustomFieldDefinition" SET "#);
    match &dto.label {
        Field::Val(l) => {
            qb.push("label = ").push_bind(l.clone()).push(", ");
        }
        Field::Null => {
            qb.push("label = NULL, ");
        }
        Field::Absent => {}
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
    if let Some(r) = audit.result_row(&mut *tx, MODEL, Lookup::Id(row.id)).await {
        audit.record_update(MODEL, pre, &r).await;
    }
    bump_catalog_epoch(&mut tx, audit, company_id).await?;
    let updated = find_by_id(&mut *tx, row.id).await?;
    tx.commit().await?;
    Ok(to_entity(&updated))
}

/// Node: src/custom-fields/custom-fields.service.ts:252 `remove` — arsip.
pub async fn remove(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    id: &str,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let row = find_row_or_throw(state, company_id, id).await?;
    let mut tx = state.db.begin().await?;
    let pre = audit.pre_read(MODEL, Lookup::Id(row.id)).await;
    sqlx::query(r#"UPDATE "orderCustomFieldDefinition" SET "isActive" = false, "updatedAt" = $2 WHERE id = $1"#)
        .bind(row.id)
        .bind(now_naive())
        .execute(&mut *tx)
        .await?;
    if let Some(r) = audit.result_row(&mut *tx, MODEL, Lookup::Id(row.id)).await {
        audit.record_update(MODEL, pre, &r).await;
    }
    bump_catalog_epoch(&mut tx, audit, company_id).await?;
    tx.commit().await?;
    Ok(json!({ "id": row.uuid, "archived": true }))
}

// ───────────────────────── handler ─────────────────────────

/// Node: src/orders/admin-order-custom-fields.controller.ts:89 `findAll`
async fn h_find_all(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedQuery(q): ValidatedQuery<Raw>,
) -> ApiResult<Json<Value>> {
    let q = parse_list(&q.0).map_err(ApiError::Validation)?;
    Ok(Json(find_all(&state, ctx.require_user_id()?, &q).await?))
}

/// Node: src/orders/admin-order-custom-fields.controller.ts:108 `create` (201)
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

/// Node: src/orders/admin-order-custom-fields.controller.ts:135 `findOne`
async fn h_find_one(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let company_id = require_company_id(&state.db, ctx.require_user_id()?).await?;
    let row = find_row_or_throw(&state, company_id, &id).await?;
    Ok(Json(json!({ "data": to_entity(&row) })))
}

/// Node: src/orders/admin-order-custom-fields.controller.ts:146 `update`
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

/// Node: src/orders/admin-order-custom-fields.controller.ts:170 `remove`
async fn h_remove(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let data = remove(&state, &audit, ctx.require_user_id()?, &id).await?;
    Ok(Json(json!({ "data": data })))
}

/// 5 route `/v1/admin/order-custom-fields` (`@RequireFeature('orders')`).
pub fn routes(state: &AppState) -> Router<AppState> {
    const P: &str = "/v1/admin/order-custom-fields";
    const PI: &str = "/v1/admin/order-custom-fields/{id}";
    let spec = crate::ORDERS;
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

    // Node: src/custom-fields/custom-fields.service.spec.ts (bagian murni)
    #[test]
    fn dto_and_rules() {
        let e = parse_dto(
            json!({"key": "po", "label": "PO", "type": "text"})
                .as_object()
                .unwrap(),
            true,
        )
        .unwrap_err();
        assert_eq!(
            e,
            vec!["entity must be one of the following values: order, orderItem".to_owned()]
        );
        let d = parse_dto(
            json!({"entity": "orderItem", "key": "po", "label": "PO", "type": "select", "options": [{"value": "a", "label": "A"}]})
                .as_object()
                .unwrap(),
            true,
        )
        .unwrap();
        assert!(assert_options_for_type("select", &d.options).is_ok());
        assert!(assert_options_for_type("text", &d.options).is_err());
        assert!(assert_options_for_type("multiselect", &Field::Absent).is_err());
        let existing = json!([{"value": "a", "label": "A"}]);
        assert!(
            assert_options_append_only(
                Some(&existing),
                &[CfOption {
                    value: "a".into(),
                    label: "B".into()
                }]
            )
            .is_err()
        );
        assert!(
            assert_options_append_only(
                Some(&existing),
                &[
                    CfOption {
                        value: "a".into(),
                        label: "A".into()
                    },
                    CfOption {
                        value: "b".into(),
                        label: "B".into()
                    }
                ]
            )
            .is_ok()
        );
        let q = parse_list(json!({}).as_object().unwrap());
        assert!(q.is_err());
        assert_eq!(cf_entity("orderItem"), CfEntity::OrderItem);
    }
}
