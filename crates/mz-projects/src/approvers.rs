//! Bagian `project-manual-time-approvers.service.ts` yang dipanggil route
//! projects: `setApprovers` (memakai `addApprover`). Semua error ditelan seperti
//! blok `try/catch` Node (hasilnya `{ success: false }` yang diabaikan pemanggil).

use mz_core::AppState;
use mz_core::time::now_naive;

/// Hasil `addApprover` (hanya `success`/`message` yang bermakna).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddOutcome {
    pub success: bool,
    pub message: String,
}

fn fail(msg: &str) -> AddOutcome {
    AddOutcome {
        success: false,
        message: msg.to_owned(),
    }
}

async fn add_approver_inner(
    state: &AppState,
    project_uuid: &str,
    user_uuid: &str,
) -> sqlx::Result<AddOutcome> {
    let project: Option<i32> =
        sqlx::query_scalar("SELECT id FROM project WHERE uuid = $1::uuid AND status = 1")
            .bind(project_uuid)
            .fetch_optional(&state.db)
            .await?;
    let user: Option<i32> =
        sqlx::query_scalar("SELECT id FROM users WHERE uuid = $1::uuid AND status = 1")
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
        r#"SELECT id, status FROM "projectManualTimeApprover" WHERE "projectId" = $1 AND "userId" = $2"#,
    )
    .bind(project_id)
    .bind(user_id)
    .fetch_optional(&state.db)
    .await?;

    if let Some((id, status)) = existing {
        if status == 1 {
            return Ok(fail("User is already an approver for this project"));
        }
        sqlx::query(
            r#"UPDATE "projectManualTimeApprover" SET status = 1, "updatedAt" = $2 WHERE id = $1"#,
        )
        .bind(id)
        .bind(now_naive())
        .execute(&state.db)
        .await?;
    } else {
        sqlx::query(r#"INSERT INTO "projectManualTimeApprover" ("projectId", "userId", status) VALUES ($1, $2, 1)"#)
            .bind(project_id)
            .bind(user_id)
            .execute(&state.db)
            .await?;
    }
    Ok(AddOutcome {
        success: true,
        message: "Approver added".into(),
    })
}

/// `addApprover(projectUuid, { userUuid })`.
pub async fn add_approver(state: &AppState, project_uuid: &str, user_uuid: &str) -> AddOutcome {
    match add_approver_inner(state, project_uuid, user_uuid).await {
        Ok(o) => o,
        Err(err) => {
            tracing::error!(error = %err, "project-manual-time-approvers-add");
            fail(&err.to_string())
        }
    }
}

/// `setApprovers(projectUuid, projectId, userUuids)`.
pub async fn set_approvers(
    state: &AppState,
    project_uuid: &str,
    project_id: i32,
    user_uuids: &[String],
) -> sqlx::Result<()> {
    sqlx::query(
        r#"UPDATE "projectManualTimeApprover" SET status = 0, "updatedAt" = $2
           WHERE "projectId" = $1 AND status = 1"#,
    )
    .bind(project_id)
    .bind(now_naive())
    .execute(&state.db)
    .await?;
    for user_uuid in user_uuids {
        let _ = add_approver(state, project_uuid, user_uuid).await;
    }
    Ok(())
}
