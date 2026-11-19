//! Port `src/project-manual-time-approvers` (2 route). Berbeda dari
//! `mz_projects::approvers` (hanya `setApprovers`), di sini respons memuat `data.uuid`.

use axum::Json;
use axum::extract::State;
use mz_core::extract::{PathParams, Validate, ValidatedJson};
use mz_core::time::now_naive;
use mz_core::uuid_rules::is_uuid;
use mz_core::{ApiResult, AppState};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::util::{defined, parse_uuid};

/// Body `CreateApproverDto`.
#[derive(Debug, Clone, Deserialize)]
#[serde(transparent)]
pub struct ApproverBody(pub Map<String, Value>);

impl Validate for ApproverBody {
    fn validate(&self) -> Vec<String> {
        let mut out = Vec::new();
        let v = self.0.get("userUuid");
        if v.is_none_or(|v| v.is_null() || v.as_str() == Some("")) {
            out.push("userUuid should not be empty".to_owned());
        }
        if !v.and_then(Value::as_str).is_some_and(is_uuid) {
            out.push("userUuid must be a UUID".to_owned());
        }
        out
    }
}

fn fail(msg: &str) -> Value {
    json!({ "success": false, "message": msg })
}

async fn add_inner(state: &AppState, project_uuid: &str, user_uuid: &str) -> sqlx::Result<Value> {
    let project: Option<i32> =
        sqlx::query_scalar("SELECT id FROM project WHERE uuid = $1::uuid AND status = 1 LIMIT 1")
            .bind(project_uuid)
            .fetch_optional(&state.db)
            .await?;
    let user: Option<i32> =
        sqlx::query_scalar("SELECT id FROM users WHERE uuid = $1::uuid AND status = 1 LIMIT 1")
            .bind(user_uuid)
            .fetch_optional(&state.db)
            .await?;
    let Some(project_id) = project else {
        return Ok(fail("Project not found"));
    };
    let Some(user_id) = user else {
        return Ok(fail("User not found"));
    };

    let is_member: Option<i32> = sqlx::query_scalar(
        r#"SELECT id FROM "projectUser" WHERE "projectId" = $1 AND "userId" = $2 AND status = 1 LIMIT 1"#,
    )
    .bind(project_id)
    .bind(user_id)
    .fetch_optional(&state.db)
    .await?;
    if is_member.is_none() {
        return Ok(fail("User is not a member of this project"));
    }

    let existing: Option<(i32, i32)> = sqlx::query_as(
        r#"SELECT id, status FROM "projectManualTimeApprover" WHERE "projectId" = $1 AND "userId" = $2 LIMIT 1"#,
    )
    .bind(project_id)
    .bind(user_id)
    .fetch_optional(&state.db)
    .await?;

    if let Some((id, status)) = existing {
        if status == 1 {
            return Ok(fail("User is already an approver for this project"));
        }
        let uuid: String = sqlx::query_scalar(
            r#"UPDATE "projectManualTimeApprover" SET status = 1, "updatedAt" = $2
               WHERE id = $1 RETURNING uuid::text"#,
        )
        .bind(id)
        .bind(now_naive())
        .fetch_one(&state.db)
        .await?;
        return Ok(
            json!({ "success": true, "message": "Approver added", "data": { "uuid": uuid } }),
        );
    }
    let uuid: String = sqlx::query_scalar(
        r#"INSERT INTO "projectManualTimeApprover" ("projectId", "userId", status)
           VALUES ($1, $2, 1) RETURNING uuid::text"#,
    )
    .bind(project_id)
    .bind(user_id)
    .fetch_one(&state.db)
    .await?;
    Ok(json!({ "success": true, "message": "Approver added", "data": { "uuid": uuid } }))
}

/// Node: src/project-manual-time-approvers/project-manual-time-approvers.service.ts:9 (`addApprover`).
pub async fn add_approver(state: &AppState, project_uuid: &str, user_uuid: &str) -> Value {
    match add_inner(state, project_uuid, user_uuid).await {
        Ok(v) => v,
        Err(err) => {
            tracing::error!(error = %err, "project-manual-time-approvers-add");
            json!({ "success": false, "message": err.to_string(), "data": null })
        }
    }
}

async fn remove_inner(
    state: &AppState,
    project_uuid: &str,
    approver_uuid: &str,
) -> sqlx::Result<Value> {
    let approver: Option<i32> = sqlx::query_scalar(
        r#"SELECT a.id FROM "projectManualTimeApprover" a JOIN project p ON p.id = a."projectId"
           WHERE a.uuid = $1::uuid AND p.uuid = $2::uuid AND a.status = 1 LIMIT 1"#,
    )
    .bind(approver_uuid)
    .bind(project_uuid)
    .fetch_optional(&state.db)
    .await?;
    let Some(id) = approver else {
        return Ok(fail("Approver not found"));
    };
    sqlx::query(
        r#"UPDATE "projectManualTimeApprover" SET status = 0, "updatedAt" = $2 WHERE id = $1"#,
    )
    .bind(id)
    .bind(now_naive())
    .execute(&state.db)
    .await?;
    Ok(json!({ "success": true, "message": "Approver removed" }))
}

/// Node: src/project-manual-time-approvers/project-manual-time-approvers.service.ts:96 (`removeApprover`).
pub async fn remove_approver(state: &AppState, project_uuid: &str, approver_uuid: &str) -> Value {
    match remove_inner(state, project_uuid, approver_uuid).await {
        Ok(v) => v,
        Err(err) => {
            tracing::error!(error = %err, "project-manual-time-approvers-remove");
            json!({ "success": false, "message": err.to_string(), "data": null })
        }
    }
}

/// Node: src/project-manual-time-approvers/project-manual-time-approvers.controller.ts:35 (`addApprover`, 200)
pub async fn h_add(
    State(state): State<AppState>,
    PathParams(project_uuid): PathParams<String>,
    ValidatedJson(body): ValidatedJson<ApproverBody>,
) -> ApiResult<Json<Value>> {
    let project_uuid = parse_uuid(project_uuid)?;
    let user_uuid = defined(body.0.get("userUuid"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    Ok(Json(add_approver(&state, &project_uuid, user_uuid).await))
}

/// Node: src/project-manual-time-approvers/project-manual-time-approvers.controller.ts:49 (`removeApprover`, 200)
pub async fn h_remove(
    State(state): State<AppState>,
    PathParams((project_uuid, approver_uuid)): PathParams<(String, String)>,
) -> ApiResult<Json<Value>> {
    let project_uuid = parse_uuid(project_uuid)?;
    let approver_uuid = parse_uuid(approver_uuid)?;
    Ok(Json(
        remove_approver(&state, &project_uuid, &approver_uuid).await,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_validation() {
        let b = |v: Value| ApproverBody(v.as_object().cloned().unwrap_or_default());
        assert_eq!(
            b(json!({})).validate(),
            vec!["userUuid should not be empty", "userUuid must be a UUID"]
        );
        assert_eq!(
            b(json!({"userUuid":"x"})).validate(),
            vec!["userUuid must be a UUID"]
        );
        assert!(
            b(json!({"userUuid":"ccae960e-ec50-4d1a-929a-d6d68285efb5"}))
                .validate()
                .is_empty()
        );
    }
}
