//! Port `src/admin-prohibited-apps/*` (`admin-prohibited-apps.controller.ts`, `.service.ts`).
//!
//! * `GET /admin-prohibited-apps` (`:31`): hanya `JwtAdminAuthGuard`; tanpa audit.
//! * `GET /admin-prohibited-apps/:uuid` (`:39`): tanpa `ParseUUIDPipe`. UUID tak valid → Prisma
//!   melempar → `success: false` (200), bukan 400.
//! * `POST /admin-prohibited-apps/create` (`:47`): 201, audit `prohibited_app.create`.
//! * `PATCH /admin-prohibited-apps/update/:uuid` (`:56`): 200, audit `prohibited_app.update`.
//! * `DELETE /admin-prohibited-apps/delete/:uuid` (`:71`): 200, audit `prohibited_app.delete`.
//!
//! Cache: Node tidak menyimpan `prohibitedApps` di cache (`/settings/prohibited-apps` membaca
//! DB tiap request), jadi tidak ada invalidasi yang perlu dipanggil di sini.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use mz_core::prisma::parse_uuid;
use mz_core::time::{JsDate, now_naive};
use mz_core::{ApiResult, AppState};
use mz_operator::{AuditCtx, AuditMeta, Operator, audit_after_success};
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::{Postgres, QueryBuilder};

use crate::common::{
    ListQuery, RawBody, RawQuery, db_fail, fail, field, finish, is_not_empty, is_string,
    is_success, max_len_ok, ok, parse_list_query, skip_for, str_of,
};

/// `ProhibitedAppsListQueryDto` (`PROHIBITED_APPS_ORDER_COLUMNS`).
const ORDER_COLUMNS: &[&str] = &["createdAt", "name", "appName"];

const CREATE_META: AuditMeta = AuditMeta {
    action: "prohibited_app.create",
    target_type: "prohibitedApp",
    company_from: None,
};
const UPDATE_META: AuditMeta = AuditMeta {
    action: "prohibited_app.update",
    target_type: "prohibitedApp",
    company_from: None,
};
const DELETE_META: AuditMeta = AuditMeta {
    action: "prohibited_app.delete",
    target_type: "prohibitedApp",
    company_from: None,
};

/// Baris `prohibitedApps` tanpa `select` (semua skalar, urutan skema).
#[derive(Debug, sqlx::FromRow, Serialize)]
#[sqlx(rename_all = "camelCase")]
#[serde(rename_all = "camelCase")]
struct AppRow {
    id: i32,
    uuid: String,
    app_name: String,
    platform: String,
    created_at: JsDate,
    updated_at: Option<JsDate>,
}

const ROW_COLS: &str = r#"id, uuid::text AS "uuid", "appName", platform, "createdAt", "updatedAt""#;

/// `ProhibitedAppsListQueryDto` + `search` (`appName` atau `platform`, insensitive `contains`).
fn push_search(qb: &mut QueryBuilder<'_, Postgres>, search: Option<&str>) {
    if let Some(s) = search.filter(|s| !s.is_empty()) {
        // Prisma `contains` + `insensitive`: ILIKE tanpa escape `%`/`_` (lihat pricing::push_search).
        qb.push(r#" WHERE ("appName" ILIKE '%' || "#)
            .push_bind(s.to_owned())
            .push(" || '%' OR platform::text ILIKE '%' || ")
            .push_bind(s.to_owned())
            .push(" || '%')");
    }
}

/// Node: src/admin-prohibited-apps/admin-prohibited-apps.controller.ts:31 (`findAll`).
pub async fn h_list(
    State(state): State<AppState>,
    _op: Operator,
    RawQuery(raw): RawQuery,
) -> ApiResult<Json<Value>> {
    let ListQuery {
        page,
        per_page,
        search,
        order_column,
        order_asc,
    } = parse_list_query(raw.as_deref(), ORDER_COLUMNS)?;
    let skip = skip_for(page, per_page)?;
    // `name` dipetakan ke kolom `appName`; selain createdAt semuanya appName.
    let key = if order_column == "createdAt" {
        "createdAt"
    } else {
        "appName"
    };
    let dir = if order_asc { "ASC" } else { "DESC" };

    let mut count = QueryBuilder::<Postgres>::new(r#"SELECT COUNT(*) FROM "prohibitedApps""#);
    push_search(&mut count, search.as_deref());
    let total: i64 = count.build_query_scalar().fetch_one(&state.db).await?;

    let mut qb =
        QueryBuilder::<Postgres>::new(format!(r#"SELECT {ROW_COLS} FROM "prohibitedApps""#));
    push_search(&mut qb, search.as_deref());
    qb.push(" ORDER BY \"").push(key).push("\" ").push(dir);
    qb.push(" OFFSET ").push_bind(skip);
    qb.push(" LIMIT ").push_bind(per_page);
    let rows: Vec<AppRow> = qb.build_query_as().fetch_all(&state.db).await?;

    Ok(Json(ok(
        "Success get all prohibited apps",
        json!({
            "total": total,
            "itemsPerPage": per_page,
            "pageNumber": page,
            "result": rows,
        }),
    )))
}

/// Node: src/admin-prohibited-apps/admin-prohibited-apps.controller.ts:39 (`findOne`, tanpa pipe).
pub async fn h_find_one(
    State(state): State<AppState>,
    _op: Operator,
    Path(uuid): Path<String>,
) -> Json<Value> {
    Json(find_one(&state, &uuid).await)
}

/// `GET /admin-prohibited-apps/create`: Nest mencocokkannya ke `GET :uuid` tanpa pipe,
/// jadi Prisma gagal dan respons 200 `success: false` (bukan 400).
pub async fn h_find_literal_create(State(state): State<AppState>, _op: Operator) -> Json<Value> {
    Json(find_one(&state, "create").await)
}

async fn find_one(state: &AppState, uuid: &str) -> Value {
    const MSG: &str = "Failed to get prohibited app";
    let Ok(id) = parse_uuid(uuid) else {
        return fail(MSG);
    };
    let row: Result<Option<AppRow>, sqlx::Error> = sqlx::query_as(&format!(
        r#"SELECT {ROW_COLS} FROM "prohibitedApps" WHERE uuid = $1 LIMIT 1"#
    ))
    .bind(id)
    .fetch_optional(&state.db)
    .await;
    match row {
        Ok(Some(r)) => ok(
            "Success get prohibited app",
            serde_json::to_value(r).unwrap_or(Value::Null),
        ),
        Ok(None) => fail("Prohibited app not found"),
        Err(e) => db_fail("prohibited_app.find_one", MSG, &e),
    }
}

/// `CreateProhibitedAppDto`: `@MaxLength(n) @IsNotEmpty() @IsString()` per properti
/// (pesan dari bawah ke atas). Pipe global: tanpa whitelist.
fn validate_app(body: &Value) -> ApiResult<()> {
    let mut e = Vec::new();
    for (key, max) in [("appName", 100), ("platform", 20)] {
        let v = field(body, key);
        if !max_len_ok(v, max) {
            e.push(format!(
                "{key} must be shorter than or equal to {max} characters"
            ));
        }
        if !is_not_empty(v) {
            e.push(format!("{key} should not be empty"));
        }
        if !is_string(v) {
            e.push(format!("{key} must be a string"));
        }
    }
    finish(e)
}

const CREATE_FAIL: &str = "Failed to create prohibited app";
const UPDATE_FAIL: &str = "Failed to update prohibited app";
const DELETE_FAIL: &str = "Failed to delete prohibited app";

/// Node: src/admin-prohibited-apps/admin-prohibited-apps.controller.ts:47 (`create`, 201).
pub async fn h_create(
    State(state): State<AppState>,
    op: Operator,
    audit: AuditCtx,
    RawBody(body): RawBody,
) -> ApiResult<(StatusCode, Json<Value>)> {
    validate_app(&body)?;
    let out = match create(&state, &body).await {
        Ok(v) => v,
        Err(e) => db_fail("prohibited_app.create", CREATE_FAIL, &e),
    };
    if is_success(&out) {
        audit_after_success(&state, &op, &audit, CREATE_META, None, Some(&body)).await;
    }
    Ok((StatusCode::CREATED, Json(out)))
}

/// Pencocokan `appName`/`platform` tanpa memperhatikan huruf (`mode: insensitive`, `equals`).
async fn create(state: &AppState, body: &Value) -> Result<Value, sqlx::Error> {
    let app = str_of(body, "appName").unwrap_or_default();
    let platform = str_of(body, "platform").unwrap_or_default();
    let dup: Option<i32> = sqlx::query_scalar(
        r#"SELECT id FROM "prohibitedApps"
            WHERE lower("appName") = lower($1) AND lower(platform) = lower($2) LIMIT 1"#,
    )
    .bind(app)
    .bind(platform)
    .fetch_optional(&state.db)
    .await?;
    if dup.is_some() {
        return Ok(fail("App with this name and platform already exists"));
    }
    let (id, uuid): (i32, String) = sqlx::query_as(
        r#"INSERT INTO "prohibitedApps" ("appName", platform) VALUES ($1, $2)
           RETURNING id, uuid::text"#,
    )
    .bind(app)
    .bind(platform)
    .fetch_one(&state.db)
    .await?;
    Ok(ok(
        "Successfully created prohibited app",
        json!({ "id": id, "uuid": uuid }),
    ))
}

/// Node: src/admin-prohibited-apps/admin-prohibited-apps.controller.ts:56 (`update`, 200).
pub async fn h_update(
    State(state): State<AppState>,
    op: Operator,
    audit: AuditCtx,
    Path(uuid): Path<String>,
    RawBody(body): RawBody,
) -> ApiResult<Json<Value>> {
    validate_app(&body)?;
    let out = match update(&state, &uuid, &body).await {
        Ok(v) => v,
        Err(e) => db_fail("prohibited_app.update", UPDATE_FAIL, &e),
    };
    if is_success(&out) {
        audit_after_success(&state, &op, &audit, UPDATE_META, Some(&uuid), Some(&body)).await;
    }
    Ok(Json(out))
}

async fn update(state: &AppState, uuid: &str, body: &Value) -> Result<Value, sqlx::Error> {
    let Ok(uuid_val) = parse_uuid(uuid) else {
        return Ok(fail(UPDATE_FAIL));
    };
    let existing: Option<i32> =
        sqlx::query_scalar(r#"SELECT id FROM "prohibitedApps" WHERE uuid = $1 LIMIT 1"#)
            .bind(uuid_val)
            .fetch_optional(&state.db)
            .await?;
    if existing.is_none() {
        return Ok(fail("Prohibited app not found"));
    }
    let app = str_of(body, "appName").unwrap_or_default();
    let platform = str_of(body, "platform").unwrap_or_default();
    let dup: Option<i32> = sqlx::query_scalar(
        r#"SELECT id FROM "prohibitedApps"
            WHERE lower("appName") = lower($1) AND lower(platform) = lower($2) AND uuid <> $3
            LIMIT 1"#,
    )
    .bind(app)
    .bind(platform)
    .bind(uuid_val)
    .fetch_optional(&state.db)
    .await?;
    if dup.is_some() {
        return Ok(fail(
            "Another app with this name and platform already exists",
        ));
    }
    sqlx::query(r#"UPDATE "prohibitedApps" SET "appName" = $1, platform = $2, "updatedAt" = $3 WHERE uuid = $4"#)
        .bind(app)
        .bind(platform)
        .bind(now_naive())
        .bind(uuid_val)
        .execute(&state.db)
        .await?;
    Ok(ok(
        "Successfully updated prohibited app",
        json!({ "uuid": uuid }),
    ))
}

/// Node: src/admin-prohibited-apps/admin-prohibited-apps.controller.ts:71 (`delete`, 200).
pub async fn h_delete(
    State(state): State<AppState>,
    op: Operator,
    audit: AuditCtx,
    Path(uuid): Path<String>,
) -> ApiResult<Json<Value>> {
    let out = match delete(&state, &uuid).await {
        Ok(v) => v,
        Err(e) => db_fail("prohibited_app.delete", DELETE_FAIL, &e),
    };
    if is_success(&out) {
        audit_after_success(&state, &op, &audit, DELETE_META, Some(&uuid), None).await;
    }
    Ok(Json(out))
}

async fn delete(state: &AppState, uuid: &str) -> Result<Value, sqlx::Error> {
    let Ok(uuid_val) = parse_uuid(uuid) else {
        return Ok(fail(DELETE_FAIL));
    };
    let existing: Option<i32> =
        sqlx::query_scalar(r#"SELECT id FROM "prohibitedApps" WHERE uuid = $1 LIMIT 1"#)
            .bind(uuid_val)
            .fetch_optional(&state.db)
            .await?;
    if existing.is_none() {
        return Ok(fail("Prohibited app not found"));
    }
    sqlx::query(r#"DELETE FROM "prohibitedApps" WHERE uuid = $1"#)
        .bind(uuid_val)
        .execute(&state.db)
        .await?;
    Ok(ok("Successfully deleted prohibited app", Value::Null))
}
