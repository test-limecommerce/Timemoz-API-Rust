//! CRUD tabel `setting` (`SettingsService.findAll/create/update/delete`). Hanya SYSTEM_ADMIN.
//! `setting` bukan model ber-audit.

use axum::Json;
use axum::extract::State;
use axum::http::Uri;
use mz_core::extract::{PathParams, ValidatedJson, ValidatedQuery};
use mz_core::time::{JsDate, now_naive};
use mz_core::uuid_rules::parse_uuid_pipe;
use mz_core::{ApiError, ApiResult, AppState};
use serde::Serialize;
use sqlx::{Postgres, QueryBuilder};

use crate::dto::{CreateSetting, Paginator, UpdateSetting};

const STATUS_ACTIVE: i32 = 1;
const STATUS_DELETED: i32 = 0;

/// Semua kolom skalar `setting` (urutan `schema.prisma`).
const COLS: &str = r#"id, uuid::text AS uuid, code, value, label, description,
    "createdAt" AS created_at, "updatedAt" AS updated_at, status"#;

#[derive(Debug, sqlx::FromRow, Serialize)]
pub struct SettingRow {
    id: i32,
    uuid: String,
    code: String,
    value: Option<String>,
    label: Option<String>,
    description: Option<String>,
    #[serde(rename = "createdAt")]
    created_at: JsDate,
    #[serde(rename = "updatedAt")]
    updated_at: Option<JsDate>,
    status: i32,
}

#[derive(Serialize)]
pub struct FindAllData {
    total: i64,
    #[serde(rename = "itemsPerPage")]
    items_per_page: i64,
    #[serde(rename = "pageNumber")]
    page_number: i64,
    result: Vec<SettingRow>,
}

#[derive(Serialize)]
pub struct Envelope<T> {
    success: bool,
    message: &'static str,
    data: T,
}

#[derive(Serialize)]
pub struct One {
    setting: SettingRow,
}

#[derive(Serialize)]
pub struct Deleted {
    #[serde(rename = "isDeleted")]
    is_deleted: bool,
}

/// Galat yang di Node berasal dari Prisma (validasi argumen / P2025 / P2023): 500 generik.
fn prisma_failure(what: &str) -> ApiError {
    ApiError::internal(anyhow::anyhow!("{what}"))
}

/// Kolom yang boleh dipakai `orderBy` (kolom skalar `setting`); selain itu Prisma menolak (500).
fn order_column(name: &str) -> Option<&'static str> {
    Some(match name {
        "id" => "id",
        "uuid" => "uuid",
        "code" => "code",
        "value" => "value",
        "label" => "label",
        "description" => "description",
        "createdAt" => r#""createdAt""#,
        "updatedAt" => r#""updatedAt""#,
        "status" => "status",
        _ => return None,
    })
}

/// `contains` + `mode: 'insensitive'` (Prisma meng-escape `%`, `_`, `\`).
fn like_pattern(search: &str) -> String {
    let mut out = String::from("%");
    for c in search.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('%');
    out
}

/// Node: src/settings/settings.controller.ts:51 (`GET /settings`)
pub async fn find_all(
    State(state): State<AppState>,
    ValidatedQuery(q): ValidatedQuery<Paginator>,
) -> ApiResult<Json<Envelope<FindAllData>>> {
    let page = q.page();
    let search = page.search.clone().filter(|s| !s.is_empty());
    let pattern = search.as_deref().map(like_pattern);

    let mut count: QueryBuilder<Postgres> =
        QueryBuilder::new("SELECT count(*) FROM setting WHERE status = ");
    count.push_bind(STATUS_ACTIVE);
    if let Some(p) = &pattern {
        count.push(r" AND (code ILIKE ").push_bind(p.clone());
        count
            .push(r" ESCAPE '\' OR value ILIKE ")
            .push_bind(p.clone());
        count.push(r" ESCAPE '\')");
    }
    let total: i64 = count.build_query_scalar().fetch_one(&state.db).await?;

    let falsy = |n: Option<i64>| n.is_none_or(|n| n == 0);
    let falsy_s = |s: &Option<String>| s.as_deref().is_none_or(str::is_empty);
    let (page_number, items, order_by, order_col) = if falsy(page.page_number)
        && falsy(page.items_per_page)
        && falsy_s(&page.order_by)
        && falsy_s(&page.order_column)
    {
        (
            Some(1),
            Some(total),
            Some("desc".to_owned()),
            Some("id".to_owned()),
        )
    } else {
        (
            page.page_number,
            page.items_per_page,
            page.order_by.clone(),
            page.order_column.clone(),
        )
    };
    // `parseInt(undefined)` = NaN → Prisma menolak take/skip (500).
    let (Some(page_number), Some(items)) = (page_number, items) else {
        return Err(prisma_failure("invalid take/skip"));
    };
    let skip = page_number.saturating_sub(1).saturating_mul(items);
    if items < 0 || skip < 0 {
        return Err(prisma_failure("negative take/skip"));
    }

    let mut qb: QueryBuilder<Postgres> =
        QueryBuilder::new(format!("SELECT {COLS} FROM setting WHERE status = "));
    qb.push_bind(STATUS_ACTIVE);
    if let Some(p) = &pattern {
        qb.push(r" AND (code ILIKE ").push_bind(p.clone());
        qb.push(r" ESCAPE '\' OR value ILIKE ").push_bind(p.clone());
        qb.push(r" ESCAPE '\')");
    }
    // `orderBy: { [orderColumn]: orderBy }`: arah undefined = tanpa urutan.
    match (order_col.as_deref(), order_by.as_deref()) {
        (_, None) => {}
        (Some(col), Some(dir @ ("asc" | "desc"))) => {
            let col = order_column(col).ok_or_else(|| prisma_failure("unknown orderBy column"))?;
            qb.push(format!(" ORDER BY {col} {}", dir.to_uppercase()));
        }
        _ => return Err(prisma_failure("invalid orderBy")),
    }
    qb.push(" LIMIT ").push_bind(items);
    qb.push(" OFFSET ").push_bind(skip);
    let result: Vec<SettingRow> = qb.build_query_as().fetch_all(&state.db).await?;

    Ok(Json(Envelope {
        success: true,
        message: "Success get all data settings",
        data: FindAllData {
            total,
            items_per_page: items,
            page_number,
            result,
        },
    }))
}

/// Node: src/settings/settings.controller.ts:63 (`POST /settings`, 200)
pub async fn create(
    State(state): State<AppState>,
    ValidatedJson(body): ValidatedJson<CreateSetting>,
) -> ApiResult<Json<Envelope<One>>> {
    // Node: `label: dto.code`, `description: dto.value` (dto.label/description diabaikan).
    let code = body.str("code");
    let value = body.str("value");
    let setting: SettingRow = sqlx::query_as(&format!(
        "INSERT INTO setting (code, value, label, description, status)
         VALUES ($1, $2, $1, $2, $3) RETURNING {COLS}"
    ))
    .bind(code)
    .bind(value)
    .bind(STATUS_ACTIVE)
    .fetch_one(&state.db)
    .await?;
    Ok(Json(Envelope {
        success: true,
        message: "create setting success",
        data: One { setting },
    }))
}

async fn do_update(
    state: &AppState,
    uuid: &str,
    body: &UpdateSetting,
) -> ApiResult<Json<Envelope<One>>> {
    let uuid = parse_uuid_pipe(uuid)?;
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new("UPDATE setting SET ");
    for f in ["code", "value", "label", "description"] {
        match body.0.get(f) {
            Some(serde_json::Value::String(s)) => {
                qb.push(format!("{f} = ")).push_bind(s.clone()).push(", ");
            }
            // `code: null` melanggar NOT NULL (Prisma validation → 500); lainnya set NULL.
            Some(serde_json::Value::Null) if f == "code" => {
                return Err(prisma_failure("code cannot be null"));
            }
            Some(serde_json::Value::Null) => {
                qb.push(format!("{f} = NULL, "));
            }
            _ => {}
        }
    }
    qb.push(r#""updatedAt" = "#).push_bind(now_naive());
    qb.push(", status = ").push_bind(STATUS_ACTIVE);
    qb.push(" WHERE uuid = ").push_bind(uuid);
    qb.push(format!(" RETURNING {COLS}"));
    // `update` tanpa baris = P2025 → 500.
    let setting: SettingRow = qb.build_query_as().fetch_one(&state.db).await?;
    Ok(Json(Envelope {
        success: true,
        message: "update setting success",
        data: One { setting },
    }))
}

/// Node: src/settings/settings.controller.ts:74 (`PATCH /settings/:uuid`, 200).
/// `@UUID(...)` mengembalikan true untuk role yang lolos `@Roles(SYSTEM_ADMIN)` (selalu
/// `seesWholeCompany`), jadi cabang `ForbiddenException` tidak terjangkau.
pub async fn update(
    State(state): State<AppState>,
    PathParams(uuid): PathParams<String>,
    ValidatedJson(body): ValidatedJson<UpdateSetting>,
) -> ApiResult<Json<Envelope<One>>> {
    do_update(&state, &uuid, &body).await
}

/// `PATCH /settings/<path statis>`: Nest mencocokkannya ke `PATCH :uuid`.
pub async fn update_tail(
    State(state): State<AppState>,
    uri: Uri,
    ValidatedJson(body): ValidatedJson<UpdateSetting>,
) -> ApiResult<Json<Envelope<One>>> {
    do_update(&state, tail(&uri), &body).await
}

async fn do_delete(state: &AppState, uuid: &str) -> ApiResult<Json<Envelope<Deleted>>> {
    // `@Param('uuid')` tanpa pipe: nilai bukan uuid / baris tidak ada → error Prisma (500).
    let uuid = uuid::Uuid::parse_str(uuid).map_err(|_| prisma_failure("invalid uuid"))?;
    let res = sqlx::query("UPDATE setting SET status = $1 WHERE uuid = $2")
        .bind(STATUS_DELETED)
        .bind(uuid)
        .execute(&state.db)
        .await?;
    if res.rows_affected() == 0 {
        return Err(prisma_failure("Record to update not found"));
    }
    Ok(Json(Envelope {
        success: true,
        message: "delete setting success",
        data: Deleted { is_deleted: true },
    }))
}

/// Node: src/settings/settings.controller.ts:96 (`DELETE /settings/:uuid`, 200)
pub async fn delete(
    State(state): State<AppState>,
    PathParams(uuid): PathParams<String>,
) -> ApiResult<Json<Envelope<Deleted>>> {
    do_delete(&state, &uuid).await
}

/// `DELETE /settings/<path statis>` → `DELETE :uuid` Nest.
pub async fn delete_tail(
    State(state): State<AppState>,
    uri: Uri,
) -> ApiResult<Json<Envelope<Deleted>>> {
    do_delete(&state, tail(&uri)).await
}

fn tail(uri: &Uri) -> &str {
    uri.path().rsplit('/').next().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn like_escapes_wildcards() {
        assert_eq!(like_pattern("a%b_c\\"), "%a\\%b\\_c\\\\%");
    }

    #[test]
    fn order_whitelist() {
        assert_eq!(order_column("createdAt"), Some("\"createdAt\""));
        assert_eq!(order_column("password"), None);
    }
}
