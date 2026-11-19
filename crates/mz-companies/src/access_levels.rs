//! Port `src/access-levels/access-levels.{controller,service}.ts` — 5 route `/access-levels*`
//! (deprecated; digantikan `/v1/admin/roles`). Tulisan ke `accessLevel` (entitas audit `role`)
//! lewat `AuditHandle`.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mz_audit::{AuditHandle, Columns, Lookup, Model, read_rows};
use mz_core::enums::{BaseRole, Role, effective_role_code};
use mz_core::extract::{PathParams, Validate, ValidatedJson, ValidatedQuery};
use mz_core::time::now_naive;
use mz_core::uuid_guard::{GuardTable, ensure_uuid_access};
use mz_core::uuid_rules::uuid_pipe;
use mz_core::{ApiError, ApiResult, AppState, RequestCtx};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sqlx::{Postgres, QueryBuilder};

use crate::common::{PageData, PageQuery, envelope, failure, node_500, paginate};

/// `AccessLevelEntity.STATUS_ACTIVE` / `STATUS_DELETED`.
const STATUS_ACTIVE: i32 = 1;
const STATUS_DELETED: i32 = 0;

/// `CreateAccessLevelDto` (`code`, `name`: `@IsString() @IsNotEmpty()`); juga dasar
/// `UpdateAccessLevelDto = PartialType(...)` (`@IsOptional()` pada tiap properti).
#[derive(Debug, Clone, Deserialize)]
#[serde(transparent)]
pub struct AccessLevelBody(pub Map<String, Value>);

impl AccessLevelBody {
    fn check(&self, optional: bool) -> Vec<String> {
        let mut out = Vec::new();
        for key in ["code", "name"] {
            match self.0.get(key) {
                Some(Value::String(s)) => {
                    if s.is_empty() {
                        out.push(format!("{key} should not be empty"));
                    }
                }
                Some(Value::Null) | None if optional => {}
                Some(v) if !v.is_null() => out.push(format!("{key} must be a string")),
                _ => {
                    out.push(format!("{key} should not be empty"));
                    out.push(format!("{key} must be a string"));
                }
            }
        }
        out
    }
}

/// `POST` memakai DTO penuh.
#[derive(Debug, Clone, Deserialize)]
#[serde(transparent)]
pub struct CreateBody(pub AccessLevelBody);

impl Validate for CreateBody {
    fn validate(&self) -> Vec<String> {
        self.0.check(false)
    }
}

/// `PATCH` memakai `PartialType`.
#[derive(Debug, Clone, Deserialize)]
#[serde(transparent)]
pub struct UpdateBody(pub AccessLevelBody);

impl Validate for UpdateBody {
    fn validate(&self) -> Vec<String> {
        self.0.check(true)
    }
}

/// Node: src/access-levels/access-levels.controller.ts:50 (`create`), service :27.
///
/// §9.1d — baris baru tidak pernah `isSystem`, jadi selalu melanggar `accessLevel_shape_chk`;
/// endpoint ini menjawab 422 yang terbaca sebagai pengganti galat Postgres mentah.
pub async fn create(ValidatedJson(_body): ValidatedJson<CreateBody>) -> ApiResult<Response> {
    Err(ApiError::coded(
        StatusCode::UNPROCESSABLE_ENTITY,
        "ACCESS_LEVELS_CREATE_DEPRECATED",
        "Use POST /v1/admin/roles to create a role instead",
    ))
}

/// Node: src/access-levels/access-levels.controller.ts:64 (`findAll`), service :34.
pub async fn find_all(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedQuery(query): ValidatedQuery<PageQuery>,
) -> ApiResult<Response> {
    let user_id = ctx.require_user_id()?;
    // §3.4a — baris `userAccessLevel` terbaru user (tanpa filter status), deterministik.
    let latest: Option<(String, bool, Option<String>)> = sqlx::query_as(
        r#"SELECT al.code, al."isSystem", al."baseRole"::text
           FROM "userAccessLevel" ual JOIN "accessLevel" al ON al.id = ual."accessLevelId"
           WHERE ual."userId" = $1 ORDER BY ual.id DESC LIMIT 1"#,
    )
    .bind(user_id)
    .fetch_optional(&state.db)
    .await?;
    // `user.userAccessLevel[0].accessLevel` → TypeError (500) bila user tanpa penugasan.
    let Some((code, is_system, base_role)) = latest else {
        return Err(node_500(
            "Cannot read properties of undefined (reading 'accessLevel')",
        ));
    };
    let is_system_admin = effective_role_code(
        &code,
        is_system,
        base_role.and_then(|b| b.parse::<BaseRole>().ok()),
    ) == Role::SYSTEM_ADMIN.as_str();

    // Role sistem global (`companyId IS NULL`) + role custom company aktif pelaku; role custom company
    // lain tidak pernah muncul (sama dengan pemetaan `accessLevel` di `company-ownership.ts`). Tanpa company
    // aktif hanya role global. Filter yang sama dipakai untuk `total` (satu closure di `paginate`).
    let company_id = ctx.company_id(&state).await?;
    let args = query.args();
    let page = paginate(
        &state,
        Model::AccessLevel,
        move |qb: &mut QueryBuilder<'_, Postgres>| {
            qb.push("status = ").push_bind(STATUS_ACTIVE);
            match company_id {
                Some(company) => {
                    qb.push(r#" AND ("companyId" IS NULL OR "companyId" = "#)
                        .push_bind(company)
                        .push(")");
                }
                None => {
                    qb.push(r#" AND "companyId" IS NULL"#);
                }
            }
            // Daftar siapa yang BOLEH melihat baris `SYSTEM_ADMIN` (§1.12 aturan 1).
            if !is_system_admin {
                qb.push(" AND code <> ")
                    .push_bind(Role::SYSTEM_ADMIN.as_str());
            }
        },
        &args,
    )
    .await?;
    let data = PageData {
        total: page.total,
        items_per_page: page.items_per_page,
        page_number: page.page_number,
        result: page.rows,
    };
    Ok(Json(envelope("Success get all data accessLevel", data)).into_response())
}

/// Node: src/access-levels/access-levels.controller.ts:77 (`findOne`), service :110.
pub async fn find_one(
    State(state): State<AppState>,
    ctx: RequestCtx,
    PathParams(uuid): PathParams<String>,
) -> ApiResult<Response> {
    let uuid = uuid_pipe(uuid)?;
    ensure_uuid_access(&state, &ctx, GuardTable::AccessLevel, &uuid).await?;
    let row = read_rows(
        &state.db,
        Model::AccessLevel,
        Columns::All,
        &Lookup::filter({
            let uuid = uuid.clone();
            move |qb| {
                qb.push("uuid = ")
                    .push_bind(uuid.clone())
                    .push("::uuid AND status = ");
                qb.push_bind(STATUS_ACTIVE);
            }
        }),
        None,
        Some(1),
    )
    .await?
    .into_iter()
    .next();
    match row {
        Some(row) => Ok(Json(envelope(
            format!("Success get data accessLevel by uuid {uuid}"),
            row,
        ))
        .into_response()),
        None => Ok(Json(failure(format!(
            "Data accessLevel by uuid {uuid} not found"
        )))
        .into_response()),
    }
}

/// `rejectIfSystem`: role bawaan tidak boleh diubah/dihapus lewat endpoint lama.
async fn reject_if_system(state: &AppState, uuid: &str) -> ApiResult<()> {
    let row: Option<(bool, String)> = sqlx::query_as(
        r#"SELECT "isSystem", code FROM "accessLevel" WHERE uuid = $1::uuid LIMIT 1"#,
    )
    .bind(uuid)
    .fetch_optional(&state.db)
    .await?;
    if let Some((true, code)) = row {
        return Err(ApiError::coded(
            StatusCode::UNPROCESSABLE_ENTITY,
            "ACCESS_LEVEL_SYSTEM_IMMUTABLE",
            format!("Access Level '{code}' is a built-in role and cannot be modified or deleted"),
        ));
    }
    Ok(())
}

/// Tulis satu `accessLevel.update` + audit; mengembalikan id barisnya.
async fn update_access_level(
    state: &AppState,
    audit: &AuditHandle,
    uuid: &str,
    code: Option<&str>,
    name: Option<&str>,
    status: Option<i32>,
    touch_updated_at: bool,
) -> ApiResult<i32> {
    let pre = audit
        .pre_read(Model::AccessLevel, Lookup::Uuid(uuid.to_owned()))
        .await;
    let mut qb = QueryBuilder::<Postgres>::new("UPDATE \"accessLevel\" SET ");
    let mut first = true;
    let mut sep = |qb: &mut QueryBuilder<'_, Postgres>| {
        if !first {
            qb.push(", ");
        }
        first = false;
    };
    if let Some(c) = code {
        sep(&mut qb);
        qb.push("code = ").push_bind(c.to_owned());
    }
    if let Some(n) = name {
        sep(&mut qb);
        qb.push("name = ").push_bind(n.to_owned());
    }
    if let Some(s) = status {
        sep(&mut qb);
        qb.push("status = ").push_bind(s);
    }
    if touch_updated_at {
        sep(&mut qb);
        qb.push("\"updatedAt\" = ").push_bind(now_naive());
    }
    qb.push(" WHERE uuid = ")
        .push_bind(uuid.to_owned())
        .push("::uuid RETURNING id");
    let id: Option<i32> = qb.build_query_scalar().fetch_optional(&state.db).await?;
    // Prisma P2025 (`Record to update not found`) → 500.
    let id = id.ok_or_else(|| node_500("Record to update not found"))?;
    if let Some(row) = read_rows(
        &state.db,
        Model::AccessLevel,
        Columns::All,
        &Lookup::Id(id),
        None,
        Some(1),
    )
    .await?
    .into_iter()
    .next()
    {
        audit.record_update(Model::AccessLevel, pre, &row).await;
    }
    Ok(id)
}

/// Node: src/access-levels/access-levels.controller.ts:102 (`update`), service :133.
///
/// §9.1c — hanya `code`/`name` yang diteruskan (lihat deviasi: Node melempar seluruh DTO ke Prisma).
pub async fn update(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    PathParams(uuid): PathParams<String>,
    ValidatedJson(body): ValidatedJson<UpdateBody>,
) -> ApiResult<Response> {
    let uuid = uuid_pipe(uuid)?;
    ensure_uuid_access(&state, &ctx, GuardTable::AccessLevel, &uuid).await?;
    reject_if_system(&state, &uuid).await?;
    let text = |k: &str| body.0.0.get(k).and_then(Value::as_str).map(str::to_owned);
    let id = update_access_level(
        &state,
        &audit,
        &uuid,
        text("code").as_deref(),
        text("name").as_deref(),
        None,
        true,
    )
    .await?;
    // Perubahan kode/nama role custom harus terlihat segera di cache role & permission.
    state.invalidate_role(id);
    let row = read_rows(
        &state.db,
        Model::AccessLevel,
        Columns::All,
        &Lookup::Id(id),
        None,
        Some(1),
    )
    .await?
    .into_iter()
    .next()
    .unwrap_or_default();
    Ok(Json(envelope("update accessLevel success", row)).into_response())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Deleted {
    is_deleted: bool,
}

/// Node: src/access-levels/access-levels.controller.ts:119 (`remove`), service :153 — soft delete.
pub async fn remove(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    PathParams(uuid): PathParams<String>,
) -> ApiResult<Response> {
    let uuid = uuid_pipe(uuid)?;
    ensure_uuid_access(&state, &ctx, GuardTable::AccessLevel, &uuid).await?;
    reject_if_system(&state, &uuid).await?;
    let id = update_access_level(
        &state,
        &audit,
        &uuid,
        None,
        None,
        Some(STATUS_DELETED),
        false,
    )
    .await?;
    state.invalidate_role(id);
    Ok(Json(envelope(
        "delete accessLevel success",
        Deleted { is_deleted: true },
    ))
    .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn body(v: Value) -> AccessLevelBody {
        AccessLevelBody(v.as_object().cloned().unwrap_or_default())
    }

    #[test]
    fn create_requires_code_and_name() {
        assert_eq!(
            body(json!({})).check(false),
            vec![
                "code should not be empty",
                "code must be a string",
                "name should not be empty",
                "name must be a string"
            ]
        );
        assert!(
            body(json!({"code": "X", "name": "Y"}))
                .check(false)
                .is_empty()
        );
    }

    #[test]
    fn update_is_partial() {
        assert!(body(json!({})).check(true).is_empty());
        assert_eq!(
            body(json!({"code": 5})).check(true),
            vec!["code must be a string"]
        );
        assert_eq!(
            body(json!({"name": ""})).check(true),
            vec!["name should not be empty"]
        );
    }
}
