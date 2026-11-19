//! Kerja latar fire-and-forget (di Node: promise tanpa `await` + `.catch(console.error)`):
//! hitung ulang burn budget, komentar ClickUp (`mz_int_clickup::track_task_time_comment`),
//! worklog Jira (`mz_int_jira::track_worklog`), email approver.

use mz_core::AppState;
use serde_json::{Value, json};

/// `this.projectBudgetService.recalculateBurn(id).catch(console.error)`.
pub fn recalculate_burn(state: &AppState, project_id: i32) {
    let state = state.clone();
    mz_core::bg::spawn(async move {
        if let Err(err) = mz_projects::budget::recalculate_burn(&state, project_id).await {
            tracing::error!(project_id, error = %err, "Budget recalculate error");
        }
    });
}

/// Permintaan push integrasi untuk satu timeline.
#[derive(Debug, Clone)]
pub struct Push {
    pub user_id: i32,
    pub timeline_id: i32,
    pub start_ms: i64,
    pub end_ms: i64,
    pub note: Option<String>,
}

/// Sumber task: uuid dari DTO (`source` = clickup/jira) atau `taskId` timeline.
#[derive(Debug, Clone, Copy)]
pub enum TaskRef<'a> {
    Uuid(&'a str),
    Id(i32),
}

async fn task_lookup(
    state: &AppState,
    task: TaskRef<'_>,
    source: &str,
) -> sqlx::Result<Option<(Option<String>, Option<Value>)>> {
    match task {
        TaskRef::Uuid(u) => {
            let Some(u) = crate::dto::uuid_of(u) else { return Ok(None) };
            sqlx::query_as(r#"SELECT "externalId", "externalMeta" FROM task WHERE uuid = $1 AND source = $2 LIMIT 1"#)
                .bind(u)
                .bind(source)
                .fetch_optional(&state.db)
                .await
        }
        TaskRef::Id(id) => {
            sqlx::query_as(r#"SELECT "externalId", "externalMeta" FROM task WHERE id = $1 AND source = $2 LIMIT 1"#)
                .bind(id)
                .bind(source)
                .fetch_optional(&state.db)
                .await
        }
    }
}

/// ClickUp: `existing` = `clickupTimeEntryId`; `gate(has_existing)` memutuskan
/// apakah push dilakukan (create: selalu; update: `durationDiff !== 0` / `>= 60`).
pub fn clickup(
    state: &AppState,
    task: TaskRef<'_>,
    push: Push,
    existing: Option<String>,
    gate: impl FnOnce(bool) -> bool + Send + 'static,
) {
    let state = state.clone();
    let task = match task {
        TaskRef::Uuid(u) => TaskOwned::Uuid(u.to_owned()),
        TaskRef::Id(i) => TaskOwned::Id(i),
    };
    mz_core::bg::spawn(async move {
        let found = match task_lookup(&state, task.as_ref(), "clickup").await {
            Ok(f) => f,
            Err(err) => {
                tracing::error!(error = %err, "ClickUp task lookup error");
                return;
            }
        };
        let Some(external) = found.and_then(|(e, _)| e).filter(|e| !e.is_empty()) else {
            return;
        };
        if !gate(existing.is_some()) {
            return;
        }
        let comment_id = mz_int_clickup::track_task_time_comment(
            &state,
            push.user_id,
            &external,
            push.start_ms,
            push.end_ms,
            push.note.as_deref(),
            existing.as_deref(),
        )
        .await;
        match comment_id {
            Some(comment_id) if existing.is_none() => {
                if let Err(err) = sqlx::query(
                    r#"UPDATE "trackingTimeline" SET "clickupTimeEntryId" = $2 WHERE id = $1"#,
                )
                .bind(push.timeline_id)
                .bind(&comment_id)
                .execute(&state.db)
                .await
                {
                    tracing::error!(error = %err, "Save clickup commentId error");
                }
            }
            _ => {}
        }
    });
}

/// Jira: sama dengan [`clickup`], issue key dari `task.externalMeta.issueKey`.
pub fn jira(
    state: &AppState,
    task: TaskRef<'_>,
    push: Push,
    existing: Option<String>,
    gate: impl FnOnce(bool) -> bool + Send + 'static,
) {
    let state = state.clone();
    let task = match task {
        TaskRef::Uuid(u) => TaskOwned::Uuid(u.to_owned()),
        TaskRef::Id(i) => TaskOwned::Id(i),
    };
    mz_core::bg::spawn(async move {
        let found = match task_lookup(&state, task.as_ref(), "jira").await {
            Ok(f) => f,
            Err(err) => {
                tracing::error!(error = %err, "Jira task lookup error");
                return;
            }
        };
        let Some(issue_key) = found
            .and_then(|(_, meta)| meta)
            .and_then(|m| m.get("issueKey").and_then(Value::as_str).map(str::to_owned))
            .filter(|k| !k.is_empty())
        else {
            return;
        };
        if !gate(existing.is_some()) {
            return;
        }
        let worklog_id = mz_int_jira::track_worklog(
            &state,
            push.user_id,
            &issue_key,
            push.start_ms,
            push.end_ms,
            push.note.as_deref(),
            existing.as_deref(),
        )
        .await;
        match worklog_id {
            Some(worklog_id) if existing.is_none() => {
                if let Err(err) = sqlx::query(
                    r#"UPDATE "trackingTimeline" SET "jiraWorklogId" = $2 WHERE id = $1"#,
                )
                .bind(push.timeline_id)
                .bind(&worklog_id)
                .execute(&state.db)
                .await
                {
                    tracing::error!(error = %err, "Save jiraWorklogId error");
                }
            }
            _ => {}
        }
    });
}

enum TaskOwned {
    Uuid(String),
    Id(i32),
}

impl TaskOwned {
    fn as_ref(&self) -> TaskRef<'_> {
        match self {
            Self::Uuid(u) => TaskRef::Uuid(u),
            Self::Id(i) => TaskRef::Id(*i),
        }
    }
}

/// Email pengajuan manual time ke approver project (`createManualTime` cabang PENDING):
/// buat token per approver (gagal → email tanpa tombol aksi), lalu kirim.
pub fn approval_emails(
    state: &AppState,
    timeline_id: i32,
    owner_id: i32,
    project_id: i32,
    start_ms: i64,
    end_ms: i64,
    reason: String,
) {
    let state = state.clone();
    mz_core::bg::spawn(async move {
        if let Err(err) = approval_emails_inner(
            &state,
            timeline_id,
            owner_id,
            project_id,
            start_ms,
            end_ms,
            &reason,
        )
        .await
        {
            tracing::error!(error = %err, "manual time approval email error");
        }
    });
}

async fn approval_emails_inner(
    state: &AppState,
    timeline_id: i32,
    owner_id: i32,
    project_id: i32,
    start_ms: i64,
    end_ms: i64,
    reason: &str,
) -> sqlx::Result<()> {
    let (staff, project): (Option<String>, Option<String>) = sqlx::query_as(
        r#"SELECT (SELECT name FROM users WHERE id = $1), (SELECT name FROM project WHERE id = $2)"#,
    )
    .bind(owner_id)
    .bind(project_id)
    .fetch_one(&state.db)
    .await?;
    let approvers: Vec<(i32, Option<String>, Option<String>)> = sqlx::query_as(
        r#"SELECT a."userId", u.name, u.username FROM "projectManualTimeApprover" a
           LEFT JOIN users u ON u.id = a."userId"
           WHERE a."projectId" = $1 AND a.status = 1 ORDER BY a.id"#,
    )
    .bind(project_id)
    .fetch_all(&state.db)
    .await?;
    if approvers.is_empty() {
        return Ok(());
    }
    let duration = crate::range::format_manual_time_seconds((end_ms - start_ms) / 1000);
    let date = crate::range::format_day_month_year(start_ms);
    let host_be = state
        .config
        .var("HOST_BE")
        .unwrap_or_else(|| "http://localhost:3000".into());
    let host_fe = state
        .config
        .var("HOST_FE")
        .unwrap_or_else(|| "http://localhost:3001".into());
    let expired_at = mz_core::time::now_naive() + chrono::Duration::days(7);
    let staff = staff.unwrap_or_default();
    for (approver_id, name, username) in approvers {
        let token: Option<uuid::Uuid> = sqlx::query_scalar(
            r#"INSERT INTO "manualTimeApprovalToken" ("trackingTimelineId", "approverId", "expiredAt")
               VALUES ($1, $2, $3) RETURNING uuid"#,
        )
        .bind(timeline_id)
        .bind(approver_id)
        .bind(expired_at)
        .fetch_one(&state.db)
        .await
        .ok();
        let url = |path: String| token.map(|_| path).unwrap_or_default();
        let t = token.map(|u| u.to_string()).unwrap_or_default();
        let name = name.unwrap_or_default();
        let subject = format!("Manual Time Approval Request from {staff}");
        mz_core::mail::spawn_send(
            state,
            mz_core::mail::OwnedMail {
                to_email: username.unwrap_or_default(),
                to_name: name.clone(),
                subject: subject.clone(),
                text: None,
                template: "manual-time-approval-request",
                context: json!({
                    "approverName": name,
                    "staffName": staff,
                    "date": date,
                    "duration": duration,
                    "projectName": project.clone().unwrap_or_default(),
                    "reason": reason,
                    "approveUrl": url(format!("{host_be}/manual-time-approvals/email-action?token={t}&action=approve")),
                    "rejectUrl": url(format!("{host_be}/manual-time-approvals/email-action?token={t}&action=reject")),
                    "detailUrl": url(format!("{host_fe}/manual-time-approvals?tab=approvals&token={t}")),
                }),
                category: "Manual Time Approval",
                label: "send manual time approval request email",
            },
        );
    }
    Ok(())
}
