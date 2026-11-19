//! Method publik `JiraService` (semua menangkap galat sendiri dan membalas
//! `{ success, message, data }` — Node tidak pernah melempar dari method ini).

use chrono::NaiveDateTime;
use mz_audit::AuditHandle;
use mz_core::AppState;
use mz_core::time::{JsDate, now_naive};
use mz_int_common::company::admin_company_id;
use mz_int_common::envelope::Envelope;
use mz_int_common::jsonb;
use reqwest::Method;
use serde::Serialize;
use serde_json::Value;
use serde_json::value::RawValue;
use sqlx::{Postgres, QueryBuilder};

use crate::creds::{
    self, Set, company_credentials, needs_refresh, refresh_access_token, register_webhook,
    update_company, valid_access_token,
};
use crate::http::{self, Error, Res};
use crate::sync::{SyncCounts, SyncTarget, sync_issues_internal};
use crate::util::{js_string, path};

fn finish<T>(module: &str, r: Res<Envelope<T>>) -> Envelope<T> {
    r.unwrap_or_else(|err| {
        tracing::error!(module, error = %err, "Jira service error");
        Envelope::fail(err.message())
    })
}

/// `{ success: true, message, data: null }`.
fn ok_null<T>(message: &str) -> Envelope<T> {
    Envelope {
        success: true,
        message: message.to_owned(),
        data: None,
    }
}

/// Filter Prisma pada kolom `@db.Uuid` dengan string non-UUID melempar (P2023).
/// Teks galat meniru crate `uuid` versi Prisma (`mz_core::prisma`).
fn parse_uuid(s: &str) -> Res<uuid::Uuid> {
    mz_core::prisma::parse_uuid(s).map_err(Error::Msg)
}

// ── status ───────────────────────────────────────────────────────────────────

/// `data` `getConnectionStatus`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct JiraStatus {
    pub connected: bool,
    pub cloud_id: Option<String>,
}

/// `getConnectionStatus(userId)` (`jira.service.ts:683`). Tidak pernah melempar.
pub async fn get_connection_status(state: &AppState, user_id: i32) -> Envelope<JiraStatus> {
    let r = async {
        let Some(c) = company_credentials(state, user_id).await? else {
            return Ok(Envelope::fail("Company not found"));
        };
        Ok(Envelope::ok(
            "Success",
            JiraStatus {
                connected: c.access_token.is_some_and(|t| !t.is_empty()),
                cloud_id: c.cloud_id,
            },
        ))
    }
    .await;
    finish("jira-status", r)
}

// ── OAuth ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthData {
    pub cloud_id: Value,
    pub site_name: Option<Value>,
}

#[derive(Serialize)]
struct CodeBody {
    grant_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_secret: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    redirect_uri: Option<String>,
}

/// `handleOAuthCallback(userId, code)` (`jira.service.ts:510`). `user_id = None` =
/// `parseInt(state)` → `NaN`.
pub(crate) async fn handle_oauth_callback(
    state: &AppState,
    audit: &AuditHandle,
    user_id: Option<i32>,
    code: Option<String>,
) -> Envelope<OAuthData> {
    let r = async {
        let Some(user_id) = user_id else {
            return Err(Error::Msg(
                "Argument `userId`: Invalid value provided. Expected Int, provided Float."
                    .to_owned(),
            ));
        };
        let Some(company_id) = admin_company_id(state, user_id).await? else {
            return Ok(Envelope::fail("Company not found"));
        };
        let token = http::send(state.http.post(http::token_url(state)).json(&CodeBody {
            grant_type: "authorization_code",
            client_id: state.config.var("JIRA_CLIENT_ID"),
            client_secret: state.config.var("JIRA_CLIENT_SECRET"),
            code,
            redirect_uri: state.config.var("JIRA_REDIRECT_URI"),
        }))
        .await?;
        let access_token = match token.get("access_token") {
            Some(Value::String(s)) => s.clone(),
            Some(other) => js_string(other),
            None => "undefined".to_owned(),
        };
        let resources = http::send(http::bearer(
            state,
            Method::GET,
            &http::resources_url(state),
            &access_token,
        ))
        .await?;
        let Some(first) = resources.as_array().and_then(|a| a.first()) else {
            return Ok(Envelope::fail("No Jira cloud sites found for this account"));
        };
        let cloud_id = first.get("id").cloned().unwrap_or(Value::Null);
        let cloud_id_str = match &cloud_id {
            Value::String(s) => Some(s.clone()),
            Value::Null => None,
            other => {
                return Err(Error::Msg(format!(
                    "Invalid value for argument `jiraCloudId`: Expected String, provided {other}."
                )));
            }
        };
        let site_url = path(first, &["url"]).map(js_string);
        let mut sets = Vec::new();
        if token.get("access_token").is_some() {
            sets.push(Set::Text("jiraAccessToken", Some(access_token.clone())));
        }
        match token.get("refresh_token") {
            None => {}
            Some(Value::Null) => sets.push(Set::Text("jiraRefreshToken", None)),
            Some(v) => sets.push(Set::Text("jiraRefreshToken", Some(js_string(v)))),
        }
        sets.push(Set::Text("jiraCloudId", cloud_id_str.clone()));
        sets.push(Set::Text("jiraSiteUrl", site_url));
        sets.push(Set::Ts(
            "jiraTokenExpiresAt",
            Some(creds::expires_at(token.get("expires_in"))?),
        ));
        update_company(state, audit, company_id, &sets).await?;

        if let Err(err) = register_webhook(
            state,
            audit,
            company_id,
            &access_token,
            cloud_id_str.as_deref().unwrap_or("undefined"),
        )
        .await
        {
            tracing::warn!("Webhook registration failed for company {company_id}: {err}");
        }

        Ok(Envelope::ok(
            "Jira connected successfully",
            OAuthData {
                cloud_id,
                site_name: first.get("name").cloned(),
            },
        ))
    }
    .await;
    finish("jira-oauth-callback", r)
}

// ── disconnect ───────────────────────────────────────────────────────────────

type DisconnectRow = (
    Option<i32>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<NaiveDateTime>,
);

/// `disconnect(userId)` (`jira.service.ts:585`).
pub(crate) async fn disconnect(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
) -> Envelope<()> {
    let r = async {
        let Some(company_id) = admin_company_id(state, user_id).await? else {
            return Ok(Envelope::fail("Company not found"));
        };
        let row: Option<DisconnectRow> = sqlx::query_as(
            r#"SELECT "jiraWebhookId", "jiraCloudId", "jiraAccessToken", "jiraRefreshToken",
                      "jiraTokenExpiresAt" FROM company WHERE id = $1 LIMIT 1"#,
        )
        .bind(company_id)
        .fetch_optional(&state.db)
        .await?;
        if let Some((Some(webhook_id), Some(cloud_id), Some(access), refresh, expires)) = row
            && webhook_id != 0
            && !cloud_id.is_empty()
            && !access.is_empty()
        {
            let attempt = async {
                let mut token = access;
                if needs_refresh(expires)
                    && let Some(r) = refresh.filter(|r| !r.is_empty())
                {
                    token = refresh_access_token(state, audit, company_id, &r).await?;
                }
                creds::delete_webhook(state, audit, company_id, &token, &cloud_id, webhook_id).await
            };
            if let Err(err) = attempt.await {
                tracing::warn!("Webhook deletion failed for company {company_id}: {err}");
            }
        }

        let clear = update_company(
            state,
            audit,
            company_id,
            &[
                Set::Text("jiraAccessToken", None),
                Set::Text("jiraRefreshToken", None),
                Set::Text("jiraCloudId", None),
                Set::Ts("jiraTokenExpiresAt", None),
                Set::Int("jiraWebhookId", None),
                Set::Text("jiraWebhookSecret", None),
            ],
        );
        let unlink = async {
            sqlx::query(
                r#"UPDATE "projectJiraMapping" SET status = 0, "updatedAt" = $2
                   WHERE "companyId" = $1 AND status = 1"#,
            )
            .bind(company_id)
            .bind(now_naive())
            .execute(&state.db)
            .await
            .map_err(Error::from)
        };
        let (a, b) = tokio::join!(clear, unlink);
        a?;
        b?;
        Ok(ok_null("Jira disconnected successfully"))
    }
    .await;
    finish("jira-disconnect", r)
}

// ── projects ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct LinkedProject {
    pub uuid: String,
    pub name: String,
}

/// Satu baris `getProjects` (`undefined` dari Jira → kunci tidak dikirim).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JiraProjectItem {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_type_key: Option<Value>,
    pub avatar_url: Value,
    pub linked_project: Option<LinkedProject>,
}

/// `getProjects(userId)` (`jira.service.ts:706`).
pub(crate) async fn get_projects(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
) -> Envelope<Vec<JiraProjectItem>> {
    let r = async {
        let Some(token) = valid_access_token(state, audit, user_id).await? else {
            return Ok(Envelope::fail("Jira not connected"));
        };
        let data = http::send(
            http::bearer(
                state,
                Method::GET,
                &http::jira_url(state, &token.cloud_id, "/project/search"),
                &token.access_token,
            )
            .query(&[("maxResults", "100"), ("orderBy", "name")]),
        )
        .await?;
        let mappings: Vec<(String, String, String)> = sqlx::query_as(
            r#"SELECT m."jiraProjectId", p.uuid::text, p.name
               FROM "projectJiraMapping" m JOIN project p ON p.id = m."projectId"
               WHERE m."companyId" = $1 AND m.status = 1 ORDER BY m.id"#,
        )
        .bind(token.company_id)
        .fetch_all(&state.db)
        .await?;
        let linked = |id: Option<&Value>| -> Option<LinkedProject> {
            let id = id?.as_str()?;
            mappings
                .iter()
                .rev()
                .find(|(j, _, _)| j == id)
                .map(|(_, uuid, name)| LinkedProject {
                    uuid: uuid.clone(),
                    name: name.clone(),
                })
        };
        let projects = path(&data, &["values"])
            .and_then(Value::as_array)
            .map(|vals| {
                vals.iter()
                    .map(|p| JiraProjectItem {
                        id: p.get("id").cloned(),
                        key: p.get("key").cloned(),
                        name: p.get("name").cloned(),
                        project_type_key: p.get("projectTypeKey").cloned(),
                        avatar_url: path(p, &["avatarUrls", "48x48"])
                            .cloned()
                            .unwrap_or(Value::Null),
                        linked_project: linked(p.get("id")),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(Envelope::ok("Success", projects))
    }
    .await;
    finish("jira-get-projects", r)
}

/// Body `LinkProjectDto` (sudah tervalidasi).
#[derive(Debug, Clone)]
pub(crate) struct LinkProjectInput {
    pub project_uuid: String,
    pub jira_project_id: String,
    pub jira_project_key: String,
    pub jira_project_name: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkProjectData {
    pub uuid: String,
    pub project_uuid: String,
    pub project_name: String,
    pub jira_project_id: String,
    pub jira_project_key: String,
    pub jira_project_name: String,
}

/// `project.findFirst({ where: { uuid, projectCompany: { some: { companyId, status: 1 } }, status: 1 } })`.
async fn company_project(
    state: &AppState,
    uuid: &str,
    company_id: i32,
) -> Res<Option<(i32, String, String)>> {
    let uuid = parse_uuid(uuid)?;
    Ok(sqlx::query_as(
        r#"SELECT p.id, p.uuid::text, p.name FROM project p
           WHERE p.uuid = $1 AND p.status = 1
             AND EXISTS (SELECT 1 FROM "projectCompany" pc
                         WHERE pc."projectId" = p.id AND pc."companyId" = $2 AND pc.status = 1)
           LIMIT 1"#,
    )
    .bind(uuid)
    .bind(company_id)
    .fetch_optional(&state.db)
    .await?)
}

/// `linkProject(userId, dto)` (`jira.service.ts:752`).
pub(crate) async fn link_project(
    state: &AppState,
    user_id: i32,
    dto: LinkProjectInput,
) -> Envelope<LinkProjectData> {
    let r = async {
        let Some(c) = company_credentials(state, user_id).await? else {
            return Ok(Envelope::fail("Company not found"));
        };
        let Some((project_id, project_uuid, project_name)) =
            company_project(state, &dto.project_uuid, c.company_id).await?
        else {
            return Ok(Envelope::fail("Project not found"));
        };
        let conflicting: Option<String> = sqlx::query_scalar(
            r#"SELECT p.name FROM "projectJiraMapping" m JOIN project p ON p.id = m."projectId"
               WHERE m."jiraProjectId" = $1 AND m."companyId" = $2 AND m.status = 1
                 AND m."projectId" <> $3
               LIMIT 1"#,
        )
        .bind(&dto.jira_project_id)
        .bind(c.company_id)
        .bind(project_id)
        .fetch_optional(&state.db)
        .await?;
        if let Some(name) = conflicting {
            return Ok(Envelope::fail(format!(
                "This Jira project is already linked to project '{name}'"
            )));
        }
        let (uuid, jira_project_id, jira_project_key, jira_project_name): (
            String,
            String,
            String,
            String,
        ) = sqlx::query_as(
            r#"INSERT INTO "projectJiraMapping"
                 ("projectId", "companyId", "jiraProjectId", "jiraProjectKey", "jiraProjectName", status)
               VALUES ($1, $2, $3, $4, $5, 1)
               ON CONFLICT ("projectId", "companyId") DO UPDATE SET
                 "jiraProjectId" = EXCLUDED."jiraProjectId",
                 "jiraProjectKey" = EXCLUDED."jiraProjectKey",
                 "jiraProjectName" = EXCLUDED."jiraProjectName",
                 status = 1, "updatedAt" = $6
               RETURNING uuid::text, "jiraProjectId", "jiraProjectKey", "jiraProjectName""#,
        )
        .bind(project_id)
        .bind(c.company_id)
        .bind(&dto.jira_project_id)
        .bind(&dto.jira_project_key)
        .bind(&dto.jira_project_name)
        .bind(now_naive())
        .fetch_one(&state.db)
        .await?;
        Ok(Envelope::ok(
            "Project linked to Jira project successfully",
            LinkProjectData {
                uuid,
                project_uuid,
                project_name,
                jira_project_id,
                jira_project_key,
                jira_project_name,
            },
        ))
    }
    .await;
    finish("jira-link-project", r)
}

/// `unlinkProject(userId, projectUuid)` (`jira.service.ts:822`).
pub(crate) async fn unlink_project(
    state: &AppState,
    user_id: i32,
    project_uuid: &str,
) -> Envelope<()> {
    let r = async {
        let Some(c) = company_credentials(state, user_id).await? else {
            return Ok(Envelope::fail("Company not found"));
        };
        let Some((project_id, _, _)) = company_project(state, project_uuid, c.company_id).await?
        else {
            return Ok(Envelope::fail("Project not found"));
        };
        let mapping: Option<i32> = sqlx::query_scalar(
            r#"SELECT id FROM "projectJiraMapping"
               WHERE "projectId" = $1 AND "companyId" = $2 AND status = 1 LIMIT 1"#,
        )
        .bind(project_id)
        .bind(c.company_id)
        .fetch_optional(&state.db)
        .await?;
        let Some(mapping_id) = mapping else {
            return Ok(Envelope::fail("No active Jira link found for this project"));
        };
        sqlx::query(
            r#"UPDATE "projectJiraMapping" SET status = 0, "updatedAt" = $2 WHERE id = $1"#,
        )
        .bind(mapping_id)
        .bind(now_naive())
        .execute(&state.db)
        .await?;
        Ok(ok_null("Project unlinked from Jira successfully"))
    }
    .await;
    finish("jira-unlink-project", r)
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct LinkedProjectRow {
    pub uuid: String,
    #[sqlx(rename = "projectUuid")]
    pub project_uuid: String,
    #[sqlx(rename = "projectName")]
    pub project_name: String,
    #[sqlx(rename = "jiraProjectId")]
    pub jira_project_id: String,
    #[sqlx(rename = "jiraProjectKey")]
    pub jira_project_key: String,
    #[sqlx(rename = "jiraProjectName")]
    pub jira_project_name: String,
    #[sqlx(rename = "linkedAt")]
    pub linked_at: JsDate,
}

/// `getLinkedProjects(userId)` (`jira.service.ts:861`).
pub(crate) async fn get_linked_projects(
    state: &AppState,
    user_id: i32,
) -> Envelope<Vec<LinkedProjectRow>> {
    let r = async {
        let Some(c) = company_credentials(state, user_id).await? else {
            return Ok(Envelope::fail("Company not found"));
        };
        let rows: Vec<LinkedProjectRow> = sqlx::query_as(
            r#"SELECT m.uuid::text AS uuid, p.uuid::text AS "projectUuid", p.name AS "projectName",
                      m."jiraProjectId", m."jiraProjectKey", m."jiraProjectName",
                      m."createdAt" AS "linkedAt"
               FROM "projectJiraMapping" m JOIN project p ON p.id = m."projectId"
               WHERE m."companyId" = $1 AND m.status = 1
               ORDER BY m."createdAt" DESC"#,
        )
        .bind(c.company_id)
        .fetch_all(&state.db)
        .await?;
        Ok(Envelope::ok("Success", rows))
    }
    .await;
    finish("jira-linked-projects", r)
}

// ── sync ─────────────────────────────────────────────────────────────────────

/// `syncTasks(userId, projectUuid, includeClosed)` dengan audit dari request (refresh
/// token menulis `company`).
pub async fn sync_tasks_with(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    project_uuid: &str,
    include_closed: bool,
) -> Envelope<SyncCounts> {
    let r = async {
        let Some(token) = valid_access_token(state, audit, user_id).await? else {
            return Ok(Envelope::fail("Jira not connected"));
        };
        let project_uuid = parse_uuid(project_uuid)?;
        let mapping: Option<(String, String, i32)> = sqlx::query_as(
            r#"SELECT m."jiraProjectId", m."jiraProjectKey", p.id
               FROM "projectJiraMapping" m JOIN project p ON p.id = m."projectId"
               WHERE m."companyId" = $1 AND m.status = 1 AND p.uuid = $2 AND p.status = 1
               LIMIT 1"#,
        )
        .bind(token.company_id)
        .bind(project_uuid)
        .fetch_optional(&state.db)
        .await?;
        let Some((jira_project_id, jira_project_key, project_id)) = mapping else {
            return Ok(Envelope::fail(
                "Project not found or not linked to a Jira project",
            ));
        };
        let result = sync_issues_internal(
            state,
            &SyncTarget {
                company_id: token.company_id,
                project_id,
                jira_project_id: &jira_project_id,
                jira_project_key: &jira_project_key,
                site_url: token.site_url.as_deref(),
                access_token: &token.access_token,
                cloud_id: &token.cloud_id,
            },
            include_closed,
            false,
        )
        .await?;
        Ok(Envelope::ok(
            format!(
                "Sync complete: {} tasks created, {} tasks updated",
                result.created, result.updated
            ),
            result,
        ))
    }
    .await;
    finish("jira-sync-tasks", r)
}

/// `syncTasks(userId, projectUuid, includeClosed)` (`jira.service.ts:893`) untuk
/// pemanggil tanpa `AuditHandle` request (`/integrations` agregat). Tidak pernah
/// melempar. Tulisan `company` (refresh token) dicatat dengan pelaku `user_id`.
pub async fn sync_tasks(
    state: &AppState,
    user_id: i32,
    project_uuid: &str,
    include_closed: bool,
) -> Envelope<SyncCounts> {
    let audit = AuditHandle::detached(state.db.clone());
    audit.set_actor(Some(user_id));
    sync_tasks_with(state, &audit, user_id, project_uuid, include_closed).await
}

// ── tasks ────────────────────────────────────────────────────────────────────

/// Filter `getProjectTasks` (query sudah tervalidasi).
#[derive(Debug, Clone, Default)]
pub(crate) struct TaskFilter {
    pub search: Option<String>,
    pub status: Option<String>,
    pub assignee: Option<String>,
    pub start_date: Option<String>,
    pub end_date: Option<String>,
    pub page: i64,
    pub limit: i64,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct TaskRow {
    pub uuid: String,
    pub source: String,
    #[sqlx(rename = "externalId")]
    pub external_id: Option<String>,
    #[sqlx(rename = "externalUrl")]
    pub external_url: Option<String>,
    #[sqlx(skip)]
    pub external_meta: Option<Box<RawValue>>,
    pub name: String,
    pub description: Option<String>,
    pub status: Option<String>,
    pub priority: Option<i32>,
    #[sqlx(rename = "priorityLabel")]
    pub priority_label: Option<String>,
    #[sqlx(rename = "dueDate")]
    pub due_date: Option<JsDate>,
    #[sqlx(rename = "startDate")]
    pub start_date: Option<JsDate>,
    #[sqlx(skip)]
    pub assignees: Option<Box<RawValue>>,
    #[sqlx(skip)]
    pub tags: Option<Box<RawValue>>,
    #[sqlx(rename = "lastSyncedAt")]
    pub last_synced_at: JsDate,
    #[serde(skip)]
    #[sqlx(rename = "metaText")]
    pub meta_text: Option<String>,
    #[serde(skip)]
    #[sqlx(rename = "assigneesText")]
    pub assignees_text: Option<String>,
    #[serde(skip)]
    #[sqlx(rename = "tagsText")]
    pub tags_text: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TasksPage {
    pub project_name: String,
    pub total: i64,
    pub page: i64,
    pub limit: i64,
    pub total_pages: i64,
    pub tasks: Vec<TaskRow>,
}

fn date_arg(raw: Option<&str>, op: &str) -> Res<Option<NaiveDateTime>> {
    raw.map(|s| {
        mz_core::time::parse_js_date(s).ok_or_else(|| {
            Error::Msg(format!(
                "Invalid value for argument `{op}`: Provided Date object is invalid. Expected Date."
            ))
        })
    })
    .transpose()
}

fn push_task_where(
    qb: &mut QueryBuilder<'_, Postgres>,
    project_id: i32,
    company_id: i32,
    f: &TaskFilter,
    start: Option<NaiveDateTime>,
    end: Option<NaiveDateTime>,
) {
    qb.push(r#" WHERE "projectId" = "#).push_bind(project_id);
    qb.push(r#" AND "companyId" = "#).push_bind(company_id);
    qb.push(" AND source = 'jira'");
    if let Some(s) = f.search.as_deref().filter(|s| !s.is_empty()) {
        qb.push(" AND name ILIKE '%' || ")
            .push_bind(s.to_owned())
            .push(" || '%'");
    }
    if let Some(s) = f.status.as_deref().filter(|s| !s.is_empty()) {
        qb.push(" AND status ILIKE ").push_bind(s.to_owned());
    }
    if let Some(a) = f.assignee.as_deref().filter(|s| !s.is_empty()) {
        mz_int_common::assignee::push_assignee_filter(qb, a);
    }
    if let Some(s) = start {
        qb.push(r#" AND "dueDate" >= "#).push_bind(s);
    }
    if let Some(e) = end {
        qb.push(r#" AND "dueDate" <= "#).push_bind(e);
    }
}

/// `getProjectTasks(...)` (`jira.service.ts:944`).
pub(crate) async fn get_project_tasks(
    state: &AppState,
    user_id: i32,
    project_uuid: &str,
    f: TaskFilter,
) -> Envelope<TasksPage> {
    let r = async {
        let Some(c) = company_credentials(state, user_id).await? else {
            return Ok(Envelope::fail("Company not found"));
        };
        let Some((project_id, _, project_name)) =
            company_project(state, project_uuid, c.company_id).await?
        else {
            return Ok(Envelope::fail("Project not found"));
        };
        let start = date_arg(f.start_date.as_deref().filter(|s| !s.is_empty()), "gte")?;
        let end = date_arg(f.end_date.as_deref().filter(|s| !s.is_empty()), "lte")?;
        let skip = (f.page - 1) * f.limit;

        let mut count_q: QueryBuilder<Postgres> = QueryBuilder::new("SELECT COUNT(*) FROM task");
        push_task_where(&mut count_q, project_id, c.company_id, &f, start, end);
        let mut list_q: QueryBuilder<Postgres> = QueryBuilder::new(
            r#"SELECT uuid::text AS uuid, source, "externalId", "externalUrl",
                      "externalMeta"::text AS "metaText", name, description, status, priority,
                      "priorityLabel", "dueDate", "startDate", assignees::text AS "assigneesText",
                      tags::text AS "tagsText", "lastSyncedAt"
               FROM task"#,
        );
        push_task_where(&mut list_q, project_id, c.company_id, &f, start, end);
        list_q
            .push(r#" ORDER BY "createdAt" ASC OFFSET "#)
            .push_bind(skip)
            .push(" LIMIT ")
            .push_bind(f.limit);
        let (total, tasks) = tokio::join!(
            count_q.build_query_scalar::<i64>().fetch_one(&state.db),
            list_q.build_query_as::<TaskRow>().fetch_all(&state.db),
        );
        let total = total?;
        let mut tasks = tasks?;
        for t in &mut tasks {
            t.external_meta = jsonb::raw_opt(t.meta_text.as_deref());
            t.assignees = jsonb::raw_opt(t.assignees_text.as_deref());
            t.tags = jsonb::raw_opt(t.tags_text.as_deref());
        }
        let total_pages = if f.limit > 0 {
            (total + f.limit - 1) / f.limit
        } else {
            0
        };
        Ok(Envelope::ok(
            "Success",
            TasksPage {
                project_name,
                total,
                page: f.page,
                limit: f.limit,
                total_pages,
                tasks,
            },
        ))
    }
    .await;
    finish("jira-get-project-tasks", r)
}

async fn task_external_id(
    state: &AppState,
    task_uuid: &str,
    company_id: i32,
) -> Res<Option<(i32, Option<String>)>> {
    let uuid = parse_uuid(task_uuid)?;
    Ok(sqlx::query_as(
        r#"SELECT id, "externalId" FROM task
           WHERE uuid = $1 AND "companyId" = $2 AND source = 'jira' LIMIT 1"#,
    )
    .bind(uuid)
    .bind(company_id)
    .fetch_optional(&state.db)
    .await?)
}

async fn transitions(
    state: &AppState,
    token: &creds::Token,
    external_id: Option<&str>,
) -> Res<Value> {
    let url = http::jira_url(
        state,
        &token.cloud_id,
        &format!("/issue/{}/transitions", external_id.unwrap_or("null")),
    );
    Ok(http::send(http::bearer(state, Method::GET, &url, &token.access_token)).await?)
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusChange {
    pub task_uuid: String,
    pub status: String,
}

/// `changeTaskStatus(userId, taskUuid, status)` (`jira.service.ts:1024`).
pub(crate) async fn change_task_status(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    task_uuid: &str,
    status: String,
) -> Envelope<StatusChange> {
    let r = async {
        let Some(token) = valid_access_token(state, audit, user_id).await? else {
            return Ok(Envelope::fail("Jira not connected"));
        };
        let Some((task_id, external_id)) =
            task_external_id(state, task_uuid, token.company_id).await?
        else {
            return Ok(Envelope::fail("Task not found"));
        };
        let res = transitions(state, &token, external_id.as_deref()).await?;
        let wanted = status.to_lowercase();
        let mut found: Option<Value> = None;
        for t in path(&res, &["transitions"])
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(name) = t.get("name").and_then(Value::as_str) else {
                return Err(Error::Msg(
                    "Cannot read properties of undefined (reading 'toLowerCase')".to_owned(),
                ));
            };
            if name.to_lowercase() == wanted {
                found = Some(t.get("id").cloned().unwrap_or(Value::Null));
                break;
            }
        }
        let Some(transition_id) = found else {
            return Ok(Envelope::fail(format!(
                "Transition '{status}' not available for this issue"
            )));
        };
        let url = http::jira_url(
            state,
            &token.cloud_id,
            &format!(
                "/issue/{}/transitions",
                external_id.as_deref().unwrap_or("null")
            ),
        );
        http::send(
            http::bearer(state, Method::POST, &url, &token.access_token)
                .json(&serde_json::json!({ "transition": { "id": transition_id } })),
        )
        .await?;
        let now = now_naive();
        sqlx::query(
            r#"UPDATE task SET status = $2, "lastSyncedAt" = $3, "updatedAt" = $3 WHERE id = $1"#,
        )
        .bind(task_id)
        .bind(&status)
        .bind(now)
        .execute(&state.db)
        .await?;
        Ok(Envelope::ok(
            "Task status updated successfully",
            StatusChange {
                task_uuid: task_uuid.to_owned(),
                status,
            },
        ))
    }
    .await;
    finish("jira-change-task-status", r)
}

#[derive(Debug, Clone, Serialize)]
pub struct TransitionItem {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<Value>,
    pub color: Value,
}

/// `getTaskStatuses(userId, taskUuid)` (`jira.service.ts:1075`).
pub(crate) async fn get_task_statuses(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    task_uuid: &str,
) -> Envelope<Vec<TransitionItem>> {
    let r = async {
        let Some(token) = valid_access_token(state, audit, user_id).await? else {
            return Ok(Envelope::fail("Jira not connected"));
        };
        let Some((_, external_id)) = task_external_id(state, task_uuid, token.company_id).await?
        else {
            return Ok(Envelope::fail("Task not found"));
        };
        let res = transitions(state, &token, external_id.as_deref()).await?;
        let items = path(&res, &["transitions"])
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .map(|t| TransitionItem {
                        id: t.get("id").cloned(),
                        name: t.get("name").cloned(),
                        color: path(t, &["to", "statusCategory", "colorName"])
                            .cloned()
                            .unwrap_or(Value::Null),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(Envelope::ok("Statuses fetched successfully", items))
    }
    .await;
    finish("jira-get-task-statuses", r)
}

// ── members ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct LinkedUser {
    pub uuid: String,
    pub name: String,
    pub username: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemberItem {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jira_account_id: Option<Value>,
    pub jira_display_name: Value,
    pub jira_email: Value,
    pub jira_avatar_url: Value,
    pub linked_user: Option<LinkedUser>,
}

/// `getMembers(userId)` (`jira.service.ts:1115`).
pub(crate) async fn get_members(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
) -> Envelope<Vec<MemberItem>> {
    let r = async {
        let Some(token) = valid_access_token(state, audit, user_id).await? else {
            return Ok(Envelope::fail("Jira not connected"));
        };
        let data = http::send(
            http::bearer(
                state,
                Method::GET,
                &http::jira_url(state, &token.cloud_id, "/users/search"),
                &token.access_token,
            )
            .query(&[("maxResults", "200")]),
        )
        .await?;
        let mappings: Vec<(String, String, String, String)> = sqlx::query_as(
            r#"SELECT m."jiraAccountId", u.uuid::text, u.name, u.username
               FROM "userJiraMapping" m JOIN users u ON u.id = m."userId"
               WHERE m."companyId" = $1 AND m.status = 1 ORDER BY m.id"#,
        )
        .bind(token.company_id)
        .fetch_all(&state.db)
        .await?;
        let linked =
            |id: Option<&Value>| -> Option<LinkedUser> {
                let id = id?.as_str()?;
                mappings.iter().rev().find(|(a, _, _, _)| a == id).map(
                    |(_, uuid, name, username)| LinkedUser {
                        uuid: uuid.clone(),
                        name: name.clone(),
                        username: username.clone(),
                    },
                )
            };
        let members = data
            .as_array()
            .map(|users| {
                users
                    .iter()
                    .filter(|u| u.get("accountType").and_then(Value::as_str) == Some("atlassian"))
                    .map(|u| MemberItem {
                        jira_account_id: u.get("accountId").cloned(),
                        jira_display_name: path(u, &["displayName"])
                            .cloned()
                            .unwrap_or(Value::Null),
                        jira_email: path(u, &["emailAddress"]).cloned().unwrap_or(Value::Null),
                        jira_avatar_url: path(u, &["avatarUrls", "48x48"])
                            .cloned()
                            .unwrap_or(Value::Null),
                        linked_user: linked(u.get("accountId")),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(Envelope::ok("Success", members))
    }
    .await;
    finish("jira-get-members", r)
}

/// Body `LinkUserDto` (sudah tervalidasi).
#[derive(Debug, Clone)]
pub(crate) struct LinkUserInput {
    pub user_uuid: String,
    pub jira_account_id: String,
    pub jira_display_name: Option<String>,
    pub jira_email: Option<String>,
    pub jira_avatar_url: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkUserData {
    pub uuid: String,
    pub user_uuid: String,
    pub user_name: String,
    pub jira_account_id: String,
    pub jira_display_name: Option<String>,
    pub jira_email: Option<String>,
}

/// `users.findFirst({ where: { uuid, userCompany: { some: { companyId, status: 1 } }, status: 1 } })`.
async fn company_user(
    state: &AppState,
    uuid: &str,
    company_id: i32,
) -> Res<Option<(i32, String, String)>> {
    let uuid = parse_uuid(uuid)?;
    Ok(sqlx::query_as(
        r#"SELECT u.id, u.uuid::text, u.name FROM users u
           WHERE u.uuid = $1 AND u.status = 1
             AND EXISTS (SELECT 1 FROM "userCompany" uc
                         WHERE uc."userId" = u.id AND uc."companyId" = $2 AND uc.status = 1)
           LIMIT 1"#,
    )
    .bind(uuid)
    .bind(company_id)
    .fetch_optional(&state.db)
    .await?)
}

/// `linkUser(userId, dto)` (`jira.service.ts:1165`).
pub(crate) async fn link_user(
    state: &AppState,
    user_id: i32,
    dto: LinkUserInput,
) -> Envelope<LinkUserData> {
    let r = async {
        let Some(c) = company_credentials(state, user_id).await? else {
            return Ok(Envelope::fail("Company not found"));
        };
        let Some((target_id, target_uuid, target_name)) =
            company_user(state, &dto.user_uuid, c.company_id).await?
        else {
            return Ok(Envelope::fail("User not found in this company"));
        };
        let conflicting: Option<String> = sqlx::query_scalar(
            r#"SELECT u.name FROM "userJiraMapping" m JOIN users u ON u.id = m."userId"
               WHERE m."jiraAccountId" = $1 AND m."companyId" = $2 AND m.status = 1
                 AND m."userId" <> $3
               LIMIT 1"#,
        )
        .bind(&dto.jira_account_id)
        .bind(c.company_id)
        .bind(target_id)
        .fetch_optional(&state.db)
        .await?;
        if let Some(name) = conflicting {
            return Ok(Envelope::fail(format!(
                "This Jira member is already linked to user '{name}'"
            )));
        }
        let (uuid, jira_account_id, jira_display_name, jira_email): (
            String,
            String,
            Option<String>,
            Option<String>,
        ) = sqlx::query_as(
            r#"INSERT INTO "userJiraMapping"
                 ("userId", "companyId", "jiraAccountId", "jiraDisplayName", "jiraEmail",
                  "jiraAvatarUrl", status)
               VALUES ($1, $2, $3, $4, $5, $6, 1)
               ON CONFLICT ("userId", "companyId") DO UPDATE SET
                 "jiraAccountId" = EXCLUDED."jiraAccountId",
                 "jiraDisplayName" = EXCLUDED."jiraDisplayName",
                 "jiraEmail" = EXCLUDED."jiraEmail",
                 "jiraAvatarUrl" = EXCLUDED."jiraAvatarUrl",
                 status = 1, "updatedAt" = $7
               RETURNING uuid::text, "jiraAccountId", "jiraDisplayName", "jiraEmail""#,
        )
        .bind(target_id)
        .bind(c.company_id)
        .bind(&dto.jira_account_id)
        .bind(&dto.jira_display_name)
        .bind(&dto.jira_email)
        .bind(&dto.jira_avatar_url)
        .bind(now_naive())
        .fetch_one(&state.db)
        .await?;
        Ok(Envelope::ok(
            "User linked to Jira member successfully",
            LinkUserData {
                uuid,
                user_uuid: target_uuid,
                user_name: target_name,
                jira_account_id,
                jira_display_name,
                jira_email,
            },
        ))
    }
    .await;
    finish("jira-link-user", r)
}

/// `unlinkUser(userId, userUuid)` (`jira.service.ts:1236`).
pub(crate) async fn unlink_user(state: &AppState, user_id: i32, user_uuid: &str) -> Envelope<()> {
    let r = async {
        let Some(c) = company_credentials(state, user_id).await? else {
            return Ok(Envelope::fail("Company not found"));
        };
        let Some((target_id, _, _)) = company_user(state, user_uuid, c.company_id).await? else {
            return Ok(Envelope::fail("User not found in this company"));
        };
        let mapping: Option<i32> = sqlx::query_scalar(
            r#"SELECT id FROM "userJiraMapping"
               WHERE "userId" = $1 AND "companyId" = $2 AND status = 1 LIMIT 1"#,
        )
        .bind(target_id)
        .bind(c.company_id)
        .fetch_optional(&state.db)
        .await?;
        let Some(mapping_id) = mapping else {
            return Ok(Envelope::fail("No active Jira link found for this user"));
        };
        sqlx::query(r#"UPDATE "userJiraMapping" SET status = 0, "updatedAt" = $2 WHERE id = $1"#)
            .bind(mapping_id)
            .bind(now_naive())
            .execute(&state.db)
            .await?;
        Ok(ok_null("User unlinked from Jira successfully"))
    }
    .await;
    finish("jira-unlink-user", r)
}
