//! `default-manual-time-approvers` (`SettingsService.get/add/removeDefaultManualTimeApprover`).
//! Model `defaultManualTimeApprover` / `projectManualTimeApprover` bukan model ber-audit.
//! Semua method Node membungkus isinya dalam try/catch → `{ success:false, message, data:null }`.

use axum::Json;
use axum::extract::State;
use mz_core::company::find_company_id;
use mz_core::extract::{PathParams, ValidatedJson};
use mz_core::time::{JsDate, now_naive};
use mz_core::uuid_rules::parse_uuid_pipe;
use mz_core::{ApiResult, AppState, RequestCtx};
use mz_int_common::envelope::Envelope;
use serde::Serialize;

use crate::dto::ApproverBody;

#[derive(Serialize)]
pub struct Approver {
    uuid: String,
    user: ApproverUser,
    #[serde(rename = "createdAt")]
    created_at: JsDate,
}

#[derive(Serialize)]
pub struct ApproverUser {
    uuid: String,
    name: String,
    username: String,
}

/// `{ success, message }` tanpa kunci `data` (return awal Node) atau dengan `data`.
#[derive(Serialize)]
#[serde(untagged)]
pub enum Reply<T> {
    Plain { success: bool, message: String },
    Data(Envelope<T>),
}

fn plain<T>(success: bool, message: &str) -> Reply<T> {
    Reply::Plain {
        success,
        message: message.to_owned(),
    }
}

/// Node: src/settings/settings.controller.ts:143 (`GET default-manual-time-approvers`, 200)
pub async fn list(
    State(state): State<AppState>,
    ctx: RequestCtx,
) -> ApiResult<Json<Envelope<Vec<Approver>>>> {
    let uid = ctx.require_user_id()?;
    Ok(Json(match list_inner(&state, uid).await {
        Ok(env) => env,
        Err(e) => Envelope::fail(e.to_string()),
    }))
}

async fn list_inner(state: &AppState, uid: i32) -> sqlx::Result<Envelope<Vec<Approver>>> {
    let Some(company_id) = find_company_id(&state.db, uid).await? else {
        return Ok(Envelope::fail("Company not found"));
    };
    let rows: Vec<(String, String, String, String, JsDate)> = sqlx::query_as(
        r#"SELECT d.uuid::text, u.uuid::text, u.name, u.username, d."createdAt"
             FROM "defaultManualTimeApprover" d
             JOIN users u ON u.id = d."userId"
            WHERE d."companyId" = $1 AND d.status = 1
            ORDER BY d."createdAt" ASC"#,
    )
    .bind(company_id)
    .fetch_all(&state.db)
    .await?;
    Ok(Envelope::ok(
        "Success",
        rows.into_iter()
            .map(|(uuid, user_uuid, name, username, created_at)| Approver {
                uuid,
                user: ApproverUser {
                    uuid: user_uuid,
                    name,
                    username,
                },
                created_at,
            })
            .collect(),
    ))
}

#[derive(Serialize)]
pub struct Record {
    uuid: String,
}

/// Node: src/settings/settings.controller.ts:154 (`POST default-manual-time-approvers`, 200)
pub async fn add(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedJson(body): ValidatedJson<ApproverBody>,
) -> ApiResult<Json<Reply<Record>>> {
    let uid = ctx.require_user_id()?;
    Ok(Json(
        match add_inner(&state, uid, &body.user_uuid()).await {
            Ok(reply) => reply,
            Err(e) => Reply::Data(Envelope::fail(e.to_string())),
        },
    ))
}

async fn add_inner(state: &AppState, uid: i32, user_uuid: &str) -> sqlx::Result<Reply<Record>> {
    let target_uuid =
        uuid::Uuid::parse_str(user_uuid).map_err(|e| sqlx::Error::Decode(e.into()))?;
    let company_id = find_company_id(&state.db, uid).await?;
    let target: Option<i32> =
        sqlx::query_scalar("SELECT id FROM users WHERE uuid = $1 AND status = 1 LIMIT 1")
            .bind(target_uuid)
            .fetch_optional(&state.db)
            .await?;
    let Some(company_id) = company_id else {
        return Ok(plain(false, "Company not found"));
    };
    let Some(target_id) = target else {
        return Ok(plain(false, "User not found"));
    };
    let is_member: Option<i32> = sqlx::query_scalar(
        r#"SELECT id FROM "userCompany" WHERE "userId" = $1 AND "companyId" = $2 AND status = 1 LIMIT 1"#,
    )
    .bind(target_id)
    .bind(company_id)
    .fetch_optional(&state.db)
    .await?;
    if is_member.is_none() {
        return Ok(plain(false, "User is not a member of this company"));
    }
    let existing: Option<(i32, i32)> = sqlx::query_as(
        r#"SELECT id, status FROM "defaultManualTimeApprover"
            WHERE "companyId" = $1 AND "userId" = $2 ORDER BY id LIMIT 1"#,
    )
    .bind(company_id)
    .bind(target_id)
    .fetch_optional(&state.db)
    .await?;

    let record_uuid: String = match existing {
        Some((_, 1)) => return Ok(plain(false, "User is already a default approver")),
        Some((id, _)) => {
            sqlx::query_scalar(
                r#"UPDATE "defaultManualTimeApprover" SET status = 1, "updatedAt" = $2
                    WHERE id = $1 RETURNING uuid::text"#,
            )
            .bind(id)
            .bind(now_naive())
            .fetch_one(&state.db)
            .await?
        }
        None => {
            sqlx::query_scalar(
                r#"INSERT INTO "defaultManualTimeApprover" ("companyId", "userId", status)
                   VALUES ($1, $2, 1) RETURNING uuid::text"#,
            )
            .bind(company_id)
            .bind(target_id)
            .fetch_one(&state.db)
            .await?
        }
    };

    // Sinkron ke semua project aktif company (Node: loop findFirst/update/create per project).
    sqlx::query(
        r#"UPDATE "projectManualTimeApprover" pa SET status = 1, "updatedAt" = $3
            WHERE pa."userId" = $2 AND pa.status <> 1
              AND pa."projectId" IN (SELECT "projectId" FROM "projectCompany"
                                      WHERE "companyId" = $1 AND status = 1)"#,
    )
    .bind(company_id)
    .bind(target_id)
    .bind(now_naive())
    .execute(&state.db)
    .await?;
    sqlx::query(
        r#"INSERT INTO "projectManualTimeApprover" ("projectId", "userId", status)
           SELECT DISTINCT pc."projectId", $2, 1 FROM "projectCompany" pc
            WHERE pc."companyId" = $1 AND pc.status = 1
           ON CONFLICT ("projectId", "userId") DO NOTHING"#,
    )
    .bind(company_id)
    .bind(target_id)
    .execute(&state.db)
    .await?;

    Ok(Reply::Data(Envelope::ok(
        "Default approver added and synced to all projects",
        Record { uuid: record_uuid },
    )))
}

/// Node: src/settings/settings.controller.ts:167 (`DELETE default-manual-time-approvers/:uuid`, 200)
pub async fn remove(
    State(state): State<AppState>,
    ctx: RequestCtx,
    PathParams(uuid): PathParams<String>,
) -> ApiResult<Json<Reply<()>>> {
    let uid = ctx.require_user_id()?;
    let uuid = parse_uuid_pipe(&uuid)?;
    Ok(Json(match remove_inner(&state, uid, uuid).await {
        Ok(reply) => reply,
        Err(e) => Reply::Data(Envelope::fail(e.to_string())),
    }))
}

async fn remove_inner(state: &AppState, uid: i32, uuid: uuid::Uuid) -> sqlx::Result<Reply<()>> {
    let Some(company_id) = find_company_id(&state.db, uid).await? else {
        return Ok(plain(false, "Company not found"));
    };
    let id: Option<i32> = sqlx::query_scalar(
        r#"SELECT id FROM "defaultManualTimeApprover"
            WHERE uuid = $1 AND "companyId" = $2 AND status = 1 LIMIT 1"#,
    )
    .bind(uuid)
    .bind(company_id)
    .fetch_optional(&state.db)
    .await?;
    let Some(id) = id else {
        return Ok(plain(false, "Default approver not found"));
    };
    sqlx::query(
        r#"UPDATE "defaultManualTimeApprover" SET status = 0, "updatedAt" = $2 WHERE id = $1"#,
    )
    .bind(id)
    .bind(now_naive())
    .execute(&state.db)
    .await?;
    Ok(plain(true, "Default approver removed"))
}
