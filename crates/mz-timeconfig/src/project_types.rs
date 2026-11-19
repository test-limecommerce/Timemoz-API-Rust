//! Port `src/project-types` (5 route `ProjectTypeController`).

use axum::Json;
use axum::extract::State;
use mz_core::extract::{PathParams, Validate, ValidatedJson, ValidatedQuery};
use mz_core::time::{JsDate, now_naive};
use mz_core::{ApiError, ApiResult, AppState};
use mz_projects::dto::PaginatorQuery;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::util::{defined, parse_uuid};

const STATUS_DELETED: i32 = 0;
const STATUS_ACTIVE: i32 = 1;

const SELECT: &str = r#"SELECT id, uuid::text AS uuid, code, name, "createdAt" AS created_at,
    "updatedAt" AS updated_at, status FROM "projectType""#;

const RETURNING: &str = r#"RETURNING id, uuid::text AS uuid, code, name, "createdAt" AS created_at,
    "updatedAt" AS updated_at, status"#;

/// Baris `projectType` (semua kolom skalar, seperti Prisma tanpa `select`).
#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectTypeRow {
    pub id: i32,
    pub uuid: String,
    pub code: String,
    pub name: String,
    pub created_at: JsDate,
    pub updated_at: Option<JsDate>,
    pub status: i32,
}

/// Body `CreateProjectTypeDto` / `UpdateProjectTypeDto` (`PartialType`).
#[derive(Debug, Clone, Deserialize)]
#[serde(transparent)]
pub struct TypeBody(pub Map<String, Value>);

fn validate_field(out: &mut Vec<String>, key: &str, v: Option<&Value>) {
    // Dekorator dievaluasi dari bawah: @IsNotEmpty lalu @IsString.
    if v.is_none_or(|v| v.is_null() || v.as_str() == Some("")) {
        out.push(format!("{key} should not be empty"));
    }
    if v.and_then(Value::as_str).is_none() {
        out.push(format!("{key} must be a string"));
    }
}

impl Validate for TypeBody {
    fn validate(&self) -> Vec<String> {
        let mut out = Vec::new();
        for key in ["code", "name"] {
            validate_field(&mut out, key, self.0.get(key));
        }
        out
    }
}

/// Body `PATCH` (`PartialType`: tiap field opsional).
#[derive(Debug, Clone, Deserialize)]
#[serde(transparent)]
pub struct TypePatchBody(pub Map<String, Value>);

impl Validate for TypePatchBody {
    fn validate(&self) -> Vec<String> {
        let mut out = Vec::new();
        for key in ["code", "name"] {
            if let Some(v) = defined(self.0.get(key)) {
                validate_field(&mut out, key, Some(v));
            }
        }
        out
    }
}

/// Kolom yang boleh dipakai `orderBy` (kolom skalar model).
const ORDER_COLUMNS: &[&str] = &[
    "id",
    "uuid",
    "code",
    "name",
    "createdAt",
    "updatedAt",
    "status",
];

fn prisma_error(what: &str) -> ApiError {
    ApiError::internal(anyhow::anyhow!("prisma validation error: {what}"))
}

/// Node: src/project-types/project-types.service.ts:30 (`findAll`)
pub async fn find_all(state: &AppState, q: &PaginatorQuery) -> ApiResult<Value> {
    let total: i64 = sqlx::query_scalar(r#"SELECT COUNT(*) FROM "projectType" WHERE status = $1"#)
        .bind(STATUS_ACTIVE)
        .fetch_one(&state.db)
        .await?;

    let none_given = q.page_number.is_none()
        && q.items_per_page.is_none()
        && q.order_by.is_none()
        && q.order_column.is_none();
    let (page, per_page, order_by, order_column) = if none_given {
        (1, total, "desc".to_owned(), "createdAt".to_owned())
    } else {
        // `parseInt(undefined)` = NaN → Prisma menolak (500) di Node.
        let page = q.page_number().ok_or_else(|| prisma_error("skip is NaN"))?;
        let per_page = q
            .items_per_page()
            .ok_or_else(|| prisma_error("take is NaN"))?;
        (
            page,
            per_page,
            q.order_by.clone().unwrap_or_else(|| "desc".into()),
            q.order_column.clone().unwrap_or_else(|| "createdAt".into()),
        )
    };

    if !ORDER_COLUMNS.contains(&order_column.as_str()) {
        return Err(prisma_error("unknown orderBy column"));
    }
    let direction = match order_by.as_str() {
        "asc" => "ASC",
        "desc" => "DESC",
        _ => return Err(prisma_error("invalid sort order")),
    };
    let skip = (page - 1).saturating_mul(per_page);
    if skip < 0 || per_page < 0 {
        return Err(prisma_error("negative take/skip"));
    }

    // `order_column` dan `direction` berasal dari daftar putih di atas.
    let rows: Vec<ProjectTypeRow> = sqlx::query_as(&format!(
        r#"{SELECT} WHERE status = $1 ORDER BY "{order_column}" {direction}, id {direction} LIMIT $2 OFFSET $3"#
    ))
    .bind(STATUS_ACTIVE)
    .bind(per_page)
    .bind(skip)
    .fetch_all(&state.db)
    .await?;

    Ok(json!({
        "success": true,
        "message": "Success get all data projectType",
        "data": { "total": total, "itemsPerPage": per_page, "pageNumber": page, "result": rows },
    }))
}

/// Node: src/project-types/project-types.service.ts:67 (`findOne`)
pub async fn find_one(state: &AppState, uuid: &str) -> ApiResult<Value> {
    let row: Option<ProjectTypeRow> = sqlx::query_as(&format!(
        "{SELECT} WHERE uuid = $1::uuid AND status = $2 LIMIT 1"
    ))
    .bind(uuid)
    .bind(STATUS_ACTIVE)
    .fetch_optional(&state.db)
    .await?;
    Ok(match row {
        None => json!({
            "success": false,
            "message": format!("Data projectType by uuid {uuid} not found"),
            "data": null,
        }),
        Some(r) => json!({
            "success": true,
            "message": format!("Success get data projectType by uuid {uuid}"),
            "data": r,
        }),
    })
}

fn str_of<'a>(m: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    defined(m.get(key)).and_then(Value::as_str)
}

/// Node: src/project-types/project-types.service.ts:12 (`create`)
pub async fn create(state: &AppState, body: &Map<String, Value>) -> ApiResult<Value> {
    let row: ProjectTypeRow = sqlx::query_as(&format!(
        r#"INSERT INTO "projectType" (code, name, status) VALUES ($1, $2, $3) {RETURNING}"#
    ))
    .bind(str_of(body, "code").unwrap_or_default())
    .bind(str_of(body, "name").unwrap_or_default())
    .bind(STATUS_ACTIVE)
    .fetch_one(&state.db)
    .await?;
    Ok(json!({ "success": true, "message": "register projectType success", "data": row }))
}

/// Node: src/project-types/project-types.service.ts:83 (`update`)
pub async fn update(state: &AppState, uuid: &str, body: &Map<String, Value>) -> ApiResult<Value> {
    let row: Option<ProjectTypeRow> = sqlx::query_as(&format!(
        r#"UPDATE "projectType" SET code = COALESCE($2, code), name = COALESCE($3, name),
           "updatedAt" = $4 WHERE uuid = $1::uuid {RETURNING}"#
    ))
    .bind(uuid)
    .bind(str_of(body, "code"))
    .bind(str_of(body, "name"))
    .bind(now_naive())
    .fetch_optional(&state.db)
    .await?;
    // Prisma P2025 (record not found) → 500 di Node.
    let row = row.ok_or_else(|| prisma_error("record to update not found"))?;
    Ok(json!({ "success": true, "message": "update projectType success", "data": row }))
}

/// Node: src/project-types/project-types.service.ts:99 (`remove`)
pub async fn remove(state: &AppState, uuid: &str) -> ApiResult<Value> {
    let res = sqlx::query(r#"UPDATE "projectType" SET status = $2 WHERE uuid = $1::uuid"#)
        .bind(uuid)
        .bind(STATUS_DELETED)
        .execute(&state.db)
        .await?;
    mz_core::db::expect_one(res.rows_affected(), "projectType")?;
    Ok(json!({
        "success": true,
        "message": "delete projectType success",
        "data": { "isDeleted": true },
    }))
}

// ───────────────────────── handler HTTP ─────────────────────────

/// Node: src/project-types/project-types.controller.ts:41 (`findAll`)
pub async fn h_find_all(
    State(state): State<AppState>,
    ValidatedQuery(q): ValidatedQuery<PaginatorQuery>,
) -> ApiResult<Json<Value>> {
    Ok(Json(find_all(&state, &q).await?))
}

/// Node: src/project-types/project-types.controller.ts:55 (`findOne`)
pub async fn h_find_one(
    State(state): State<AppState>,
    PathParams(uuid): PathParams<String>,
) -> ApiResult<Json<Value>> {
    let uuid = parse_uuid(uuid)?;
    Ok(Json(find_one(&state, &uuid).await?))
}

/// Node: src/project-types/project-types.controller.ts:70 (`create`, `@HttpCode(200)`)
pub async fn h_create(
    State(state): State<AppState>,
    ValidatedJson(body): ValidatedJson<TypeBody>,
) -> ApiResult<Json<Value>> {
    Ok(Json(create(&state, &body.0).await?))
}

/// Node: src/project-types/project-types.controller.ts:85 (`update`)
pub async fn h_update(
    State(state): State<AppState>,
    PathParams(uuid): PathParams<String>,
    ValidatedJson(body): ValidatedJson<TypePatchBody>,
) -> ApiResult<Json<Value>> {
    let uuid = parse_uuid(uuid)?;
    Ok(Json(update(&state, &uuid, &body.0).await?))
}

/// Node: src/project-types/project-types.controller.ts:100 (`remove`)
pub async fn h_remove(
    State(state): State<AppState>,
    PathParams(uuid): PathParams<String>,
) -> ApiResult<Json<Value>> {
    let uuid = parse_uuid(uuid)?;
    Ok(Json(remove(&state, &uuid).await?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(v: Value) -> TypeBody {
        TypeBody(v.as_object().cloned().unwrap_or_default())
    }

    #[test]
    fn create_validation_messages() {
        assert_eq!(
            body(json!({})).validate(),
            vec![
                "code should not be empty",
                "code must be a string",
                "name should not be empty",
                "name must be a string"
            ]
        );
        assert!(body(json!({"code":"A","name":"B"})).validate().is_empty());
        assert_eq!(
            body(json!({"code":"","name":"B"})).validate(),
            vec!["code should not be empty"]
        );
    }

    #[test]
    fn patch_allows_missing() {
        let p = TypePatchBody(Map::new());
        assert!(p.validate().is_empty());
        let p = TypePatchBody(json!({"name": 3}).as_object().cloned().unwrap_or_default());
        assert_eq!(p.validate(), vec!["name must be a string"]);
    }
}
