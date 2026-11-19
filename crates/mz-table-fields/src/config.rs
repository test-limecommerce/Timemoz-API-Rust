//! API konfigurasi matriks visibilitas kolom (`table-field-config.service.ts`): daftar tabel,
//! baca/simpan/reset matriks. Satu-satunya pemanggil [`crate::policy::invalidate_company`].
//! `tableFieldVisibility` bukan model ber-audit.

use axum::Json;
use axum::extract::State;
use http::StatusCode;
use mz_core::company::require_company_id;
use mz_core::extract::{ValidatedJson, ValidatedQuery};
use mz_core::uuid_rules::is_uuid;
use mz_core::{ApiError, ApiResult, AppState, RequestCtx};
use serde::Serialize;
use serde_json::{Map, Value};
use sqlx::{Postgres, QueryBuilder};

use crate::dto::{MatrixQuery, MatrixRow, PutBody, ResetQuery};
use crate::policy::invalidate_company;
use crate::registry::{
    ColumnDefinition, TABLES, TableDefinition, column_of, configurable_columns, table_by_key,
};

/// `accessLevel.status`: 1 = aktif.
const ACTIVE: i32 = 1;

#[derive(Serialize)]
pub struct Data<T> {
    pub data: T,
}

/// Kolom di layar konfigurasi. Properti `undefined` JS tidak ikut ke JSON.
#[derive(Serialize)]
pub struct ColumnEntity {
    key: &'static str,
    label: &'static str,
    #[serde(rename = "labelEn")]
    label_en: &'static str,
    #[serde(rename = "allowedModes", skip_serializing_if = "Option::is_none")]
    allowed_modes: Option<Vec<&'static str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    searchable: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sortable: Option<bool>,
}

fn column_entity(c: &ColumnDefinition) -> ColumnEntity {
    ColumnEntity {
        key: c.key,
        label: c.label,
        label_en: c.label_en,
        allowed_modes: (!c.allowed_modes.is_empty())
            .then(|| c.allowed_modes.iter().map(|m| m.as_str()).collect()),
        searchable: c.searchable.then_some(true),
        sortable: c.sortable.then_some(true),
    }
}

#[derive(Serialize)]
pub struct TableEntity {
    key: &'static str,
    label: &'static str,
    #[serde(rename = "labelEn")]
    label_en: &'static str,
    columns: Vec<ColumnEntity>,
}

fn table_entity(t: &TableDefinition) -> TableEntity {
    TableEntity {
        key: t.key,
        label: t.label,
        label_en: t.label_en,
        columns: configurable_columns(t.key)
            .into_iter()
            .map(column_entity)
            .collect(),
    }
}

#[derive(Serialize)]
pub struct RoleEntity {
    #[serde(rename = "accessLevelId")]
    access_level_id: String,
    code: String,
    name: String,
    #[serde(rename = "isSystem")]
    is_system: bool,
    /// `Record<string, string>`: urutan penyisipan, kunci lama ditimpa di tempat
    /// (`serde_json::Map` + `preserve_order`), seperti objek JS.
    overrides: Map<String, Value>,
}

#[derive(Serialize)]
pub struct MatrixEntity {
    #[serde(rename = "tableKey")]
    table_key: &'static str,
    columns: Vec<ColumnEntity>,
    #[serde(rename = "companyDefault")]
    company_default: Map<String, Value>,
    roles: Vec<RoleEntity>,
}

#[derive(sqlx::FromRow)]
struct VisibleRole {
    id: i32,
    uuid: String,
    code: String,
    name: String,
    #[sqlx(rename = "isSystem")]
    is_system: bool,
}

/// Role yang boleh muncul di matriks: milik company pelaku ATAU bawaan, tanpa `SYSTEM_ADMIN`.
async fn visible_roles(state: &AppState, company_id: i32) -> sqlx::Result<Vec<VisibleRole>> {
    sqlx::query_as(
        r#"SELECT id, uuid::text AS uuid, code, name, "isSystem"
             FROM "accessLevel"
            WHERE status = $2 AND ("companyId" = $1 OR "isSystem" = true)
              AND NOT (code = 'SYSTEM_ADMIN')
            ORDER BY "isSystem" DESC, name ASC, id ASC"#,
    )
    .bind(company_id)
    .bind(ACTIVE)
    .fetch_all(&state.db)
    .await
}

/// §1.6 — target role: milik company pelaku ATAU bawaan; selain itu 404.
async fn require_access_level(state: &AppState, company_id: i32, uuid: &str) -> ApiResult<i32> {
    let not_found = || {
        ApiError::coded(
            StatusCode::NOT_FOUND,
            "ACCESS_LEVEL_NOT_FOUND",
            format!("Access level '{uuid}' not found"),
        )
    };
    // Kolom `@db.Uuid`: nilai bukan UUID dikenali DB sebagai galat; DTO sudah menjamin bentuknya.
    let Ok(parsed) = uuid::Uuid::parse_str(uuid) else {
        return Err(not_found());
    };
    sqlx::query_scalar(
        r#"SELECT id FROM "accessLevel"
            WHERE uuid = $1 AND status = $3 AND ("companyId" = $2 OR "isSystem" = true)
              AND NOT (code = 'SYSTEM_ADMIN') LIMIT 1"#,
    )
    .bind(parsed)
    .bind(company_id)
    .bind(ACTIVE)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(not_found)
}

fn require_table(table_key: &str) -> ApiResult<&'static TableDefinition> {
    table_by_key(table_key).ok_or_else(|| {
        ApiError::coded(
            StatusCode::NOT_FOUND,
            "TABLE_NOT_FOUND",
            format!("Table '{table_key}' not found"),
        )
    })
}

async fn get_matrix_for(
    state: &AppState,
    company_id: i32,
    table: &'static TableDefinition,
) -> ApiResult<MatrixEntity> {
    let roles = visible_roles(state, company_id).await?;
    let role_ids: Vec<i32> = roles.iter().map(|r| r.id).collect();
    let rows: Vec<(Option<i32>, String, String)> = sqlx::query_as(
        r#"SELECT "accessLevelId", "columnKey", mode::text
             FROM "tableFieldVisibility"
            WHERE "companyId" = $1 AND "tableKey" = $2
              AND ("accessLevelId" IS NULL OR "accessLevelId" = ANY($3))
            ORDER BY id"#,
    )
    .bind(company_id)
    .bind(table.key)
    .bind(&role_ids)
    .fetch_all(&state.db)
    .await?;

    let columns = configurable_columns(table.key);
    let mut company_default = Map::new();
    let mut overrides: std::collections::HashMap<i32, Map<String, Value>> = Default::default();
    for (access_level_id, column_key, mode) in &rows {
        // Kolom yang sudah dihapus dari registry atau `locked` tidak dirender sebagai override.
        if !columns.iter().any(|c| c.key == column_key) {
            continue;
        }
        match access_level_id {
            None => {
                company_default.insert(column_key.clone(), mode.as_str().into());
            }
            Some(id) => {
                overrides
                    .entry(*id)
                    .or_default()
                    .insert(column_key.clone(), mode.as_str().into());
            }
        }
    }
    Ok(MatrixEntity {
        table_key: table.key,
        columns: columns.into_iter().map(column_entity).collect(),
        company_default,
        roles: roles
            .into_iter()
            .map(|r| RoleEntity {
                overrides: overrides.remove(&r.id).unwrap_or_default(),
                access_level_id: r.uuid,
                code: r.code,
                name: r.name,
                is_system: r.is_system,
            })
            .collect(),
    })
}

/// Node: src/table-fields/table-fields.controller.ts:78 (`GET /v1/admin/table-fields/tables`)
pub async fn list_tables() -> Json<Data<Vec<TableEntity>>> {
    Json(Data {
        data: TABLES.iter().map(table_entity).collect(),
    })
}

/// Node: src/table-fields/table-fields.controller.ts:85 (`GET /v1/admin/table-fields/matrix`)
pub async fn get_matrix(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedQuery(q): ValidatedQuery<MatrixQuery>,
) -> ApiResult<Json<Data<MatrixEntity>>> {
    let company_id = require_company_id(&state.db, ctx.require_user_id()?).await?;
    let table = require_table(&q.table_key())?;
    Ok(Json(Data {
        data: get_matrix_for(&state, company_id, table).await?,
    }))
}

/// Validasi satu entri (`validateEntry`): kolom di luar registry atau `locked` → 422 FIELD_LOCKED.
fn validate_entry(table_key: &str, column_key: &str, mode: &str) -> ApiResult<()> {
    let Some(column) = column_of(table_key, column_key).filter(|c| !c.locked) else {
        return Err(ApiError::coded(
            StatusCode::UNPROCESSABLE_ENTITY,
            "FIELD_LOCKED",
            format!("Column '{column_key}' on table '{table_key}' cannot be configured"),
        ));
    };
    if !column.allowed_modes.is_empty() && !column.allowed_modes.iter().any(|m| m.as_str() == mode)
    {
        return Err(ApiError::coded(
            StatusCode::UNPROCESSABLE_ENTITY,
            "FIELD_MODE_NOT_ALLOWED",
            format!("Mode '{mode}' is not allowed for column '{column_key}'"),
        ));
    }
    Ok(())
}

struct Planned {
    access_level_id: Option<i32>,
    keep: Vec<(String, String)>,
}

async fn plan_rows(
    state: &AppState,
    company_id: i32,
    table: &TableDefinition,
    rows: &[MatrixRow],
) -> ApiResult<Vec<Planned>> {
    let mut planned = Vec::new();
    for row in rows {
        let access_level_id = match row.access_level_id.as_deref() {
            Some(uuid) if is_uuid(uuid) => {
                Some(require_access_level(state, company_id, uuid).await?)
            }
            Some(uuid) => {
                return Err(ApiError::coded(
                    StatusCode::NOT_FOUND,
                    "ACCESS_LEVEL_NOT_FOUND",
                    format!("Access level '{uuid}' not found"),
                ));
            }
            None => None,
        };
        let mut seen = std::collections::HashSet::new();
        let mut keep = Vec::new();
        for entry in &row.entries {
            if !seen.insert(entry.column_key.clone()) {
                return Err(ApiError::bad_request(format!(
                    "Column '{}' repeated in the same row",
                    entry.column_key
                )));
            }
            validate_entry(table.key, &entry.column_key, &entry.mode)?;
            // §0.13 — baris default se-company hanya untuk yang BEDA dari default registry
            // (`visible`); override role BOLEH `visible` (membatalkan default yang lebih ketat).
            if access_level_id.is_none() && entry.mode == "visible" {
                continue;
            }
            keep.push((entry.column_key.clone(), entry.mode.clone()));
        }
        planned.push(Planned {
            access_level_id,
            keep,
        });
    }
    Ok(planned)
}

/// Node: src/table-fields/table-fields.controller.ts:93 (`PUT /v1/admin/table-fields/matrix`, 200)
pub async fn put_matrix(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedJson(body): ValidatedJson<PutBody>,
) -> ApiResult<Json<Data<MatrixEntity>>> {
    let user_id = ctx.require_user_id()?;
    let company_id = require_company_id(&state.db, user_id).await?;
    let table = require_table(&body.table_key())?;
    let planned = plan_rows(&state, company_id, table, &body.rows()).await?;

    // Semantik REPLACE per `(tableKey, accessLevelId)`.
    let mut tx = state.db.begin().await?;
    for row in &planned {
        sqlx::query(
            r#"DELETE FROM "tableFieldVisibility"
                WHERE "companyId" = $1 AND "tableKey" = $2
                  AND "accessLevelId" IS NOT DISTINCT FROM $3"#,
        )
        .bind(company_id)
        .bind(table.key)
        .bind(row.access_level_id)
        .execute(&mut *tx)
        .await?;
        if !row.keep.is_empty() {
            let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
                r#"INSERT INTO "tableFieldVisibility"
                   ("companyId", "accessLevelId", "tableKey", "columnKey", mode, "updatedById") "#,
            );
            qb.push_values(&row.keep, |mut b, (column_key, mode)| {
                b.push_bind(company_id)
                    .push_bind(row.access_level_id)
                    .push_bind(table.key)
                    .push_bind(column_key.clone())
                    .push_bind(mode.clone())
                    .push_unseparated(r#"::"FieldVisibilityMode""#)
                    .push_bind(user_id);
            });
            qb.build().execute(&mut *tx).await?;
        }
    }
    tx.commit().await?;

    invalidate_company(&state, company_id);
    Ok(Json(Data {
        data: get_matrix_for(&state, company_id, table).await?,
    }))
}

/// Node: src/table-fields/table-fields.controller.ts:106 (`DELETE /v1/admin/table-fields/matrix`)
pub async fn reset_matrix(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedQuery(q): ValidatedQuery<ResetQuery>,
) -> ApiResult<Json<Data<MatrixEntity>>> {
    let company_id = require_company_id(&state.db, ctx.require_user_id()?).await?;
    let table = require_table(&q.table_key())?;
    let access_level_id = match q.access_level_id() {
        Some(uuid) => Some(require_access_level(&state, company_id, &uuid).await?),
        None => None,
    };
    sqlx::query(
        r#"DELETE FROM "tableFieldVisibility"
            WHERE "companyId" = $1 AND "tableKey" = $2
              AND "accessLevelId" IS NOT DISTINCT FROM $3"#,
    )
    .bind(company_id)
    .bind(table.key)
    .bind(access_level_id)
    .execute(&state.db)
    .await?;
    invalidate_company(&state, company_id);
    Ok(Json(Data {
        data: get_matrix_for(&state, company_id, table).await?,
    }))
}
