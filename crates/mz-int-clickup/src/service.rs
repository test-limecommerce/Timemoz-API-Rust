//! `ClickupService` (`src/integrations/clickup/clickup.service.ts`): method publik yang
//! dipakai controller. Semua method Node membungkus isinya dengan `try/catch` dan
//! mengembalikan `{ success, message, data }` — tidak ada yang melempar.

use std::collections::HashMap;

use mz_core::AppState;
use mz_core::time::{JsDate, now_naive, parse_js_date};
use mz_int_common::envelope::Envelope;
use mz_int_common::jsonb::raw_opt;
use serde::Serialize;
use serde_json::value::RawValue;
use serde_json::{Value, json};
use sqlx::{Postgres, QueryBuilder};

use crate::client::{Client, HttpErr};
use crate::dto::{LinkProject, LinkUser, ListItem, ProjectTasksQuery};
use crate::tasks::{SyncTotals, Thrown, sync_tasks_internal};
use crate::util::{field, js_str, nn, truthy};

/// `TASK_WEBHOOK_EVENTS`.
pub(crate) const TASK_WEBHOOK_EVENTS: [&str; 8] = [
    "taskCreated",
    "taskUpdated",
    "taskDeleted",
    "taskStatusUpdated",
    "taskAssigneeUpdated",
    "taskDueDateUpdated",
    "taskTagUpdated",
    "taskPriorityUpdated",
];

const NO_COMPANY: &str = "Company not found";
const NO_TOKEN: &str = "ClickUp API token not configured";
const NO_WORKSPACE: &str = "ClickUp workspace not selected. Please select a workspace first.";
const BAD_TOKEN_UPDATE: &str = "Invalid ClickUp API token. Please update your token.";

type Res<T> = Result<Envelope<T>, Thrown>;

/// `catch (error) { Sentry...; return { success:false, message: error.message, data:null } }`.
fn caught<T>(module: &'static str, r: Res<T>) -> Envelope<T> {
    r.unwrap_or_else(|Thrown(message)| {
        tracing::error!(module, error = %message, "clickup");
        Envelope::fail(message)
    })
}

/// `{ success: true, message, data: null }`.
fn ok_null<T>(message: &str) -> Envelope<T> {
    Envelope {
        success: true,
        message: message.into(),
        data: None,
    }
}

/// `x.map(...)` pada nilai yang bukan array → TypeError JS.
fn js_array<'a>(v: Option<&'a Value>, name: &str, method: &str) -> Result<&'a [Value], Thrown> {
    match v {
        Some(Value::Array(a)) => Ok(a),
        None => Err(Thrown(format!(
            "Cannot read properties of undefined (reading '{method}')"
        ))),
        Some(Value::Null) => Err(Thrown(format!(
            "Cannot read properties of null (reading '{method}')"
        ))),
        Some(_) => Err(Thrown(format!("{name}.{method} is not a function"))),
    }
}

/// `obj.key` untuk respons: `None` = `undefined` (kunci dibuang `JSON.stringify`).
fn prop(v: &Value, key: &str) -> Option<Value> {
    v.get(key).cloned()
}

// ───────────────────────── kredensial ─────────────────────────

/// `getCompanyCredentials(userId)`.
#[derive(Debug, Clone)]
pub(crate) struct Creds {
    pub company_id: i32,
    pub token: Option<String>,
    pub workspace_id: Option<String>,
}

impl Creds {
    /// Token bila truthy.
    pub(crate) fn token(&self) -> Option<&str> {
        self.token.as_deref().filter(|t| !t.is_empty())
    }
    fn workspace(&self) -> Option<&str> {
        self.workspace_id.as_deref().filter(|t| !t.is_empty())
    }
}

pub(crate) async fn credentials(state: &AppState, user_id: i32) -> sqlx::Result<Option<Creds>> {
    let Some(company_id) = mz_int_common::company::admin_company_id(state, user_id).await? else {
        return Ok(None);
    };
    let row: Option<(i32, Option<String>, Option<String>)> = sqlx::query_as(
        r#"SELECT id, "clickupApiToken", "clickupWorkspaceId" FROM company WHERE id = $1"#,
    )
    .bind(company_id)
    .fetch_optional(&state.db)
    .await?;
    Ok(row.map(|(id, token, ws)| Creds {
        company_id: id,
        token,
        workspace_id: ws,
    }))
}

/// `getToken(userId)` (truthy saja).
async fn get_token(state: &AppState, user_id: i32) -> sqlx::Result<Option<String>> {
    Ok(credentials(state, user_id)
        .await?
        .and_then(|c| c.token().map(str::to_owned)))
}

/// `uuid` Prisma `@db.Uuid`: string tak sah → dianggap tidak ketemu (deviasi).
fn parse_uuid(s: &str) -> Option<uuid::Uuid> {
    mz_core::prisma::parse_uuid(s).ok()
}

/// `project.findFirst({ where: { uuid, projectCompany: { some: { companyId, status: 1 } }, status: 1 } })`.
async fn company_project(
    state: &AppState,
    uuid: &str,
    company_id: i32,
) -> sqlx::Result<Option<(i32, String, String)>> {
    let Some(u) = parse_uuid(uuid) else {
        return Ok(None);
    };
    sqlx::query_as(
        r#"SELECT p.id, p.uuid::text, p.name FROM project p
           WHERE p.uuid = $1 AND p.status = 1
             AND EXISTS (SELECT 1 FROM "projectCompany" pc
                         WHERE pc."projectId" = p.id AND pc."companyId" = $2 AND pc.status = 1)
           LIMIT 1"#,
    )
    .bind(u)
    .bind(company_id)
    .fetch_optional(&state.db)
    .await
}

/// `users.findFirst({ where: { uuid, userCompany: { some: { companyId, status: 1 } }, status: 1 } })`.
async fn company_user(
    state: &AppState,
    uuid: &str,
    company_id: i32,
) -> sqlx::Result<Option<(i32, String, String, String)>> {
    let Some(u) = parse_uuid(uuid) else {
        return Ok(None);
    };
    sqlx::query_as(
        r#"SELECT u.id, u.uuid::text, u.name, u.username FROM users u
           WHERE u.uuid = $1 AND u.status = 1
             AND EXISTS (SELECT 1 FROM "userCompany" uc
                         WHERE uc."userId" = u.id AND uc."companyId" = $2 AND uc.status = 1)
           LIMIT 1"#,
    )
    .bind(u)
    .bind(company_id)
    .fetch_optional(&state.db)
    .await
}

// ───────────────────────── webhook ClickUp ─────────────────────────

/// `registerWebhook(token, workspaceId, folderId)` → `(webhookId, secret)`; gagal → `None`.
pub(crate) async fn register_webhook(
    state: &AppState,
    token: &str,
    workspace_id: &str,
    folder_id: &str,
) -> Option<(Option<String>, Option<String>)> {
    let host = state
        .config
        .var("HOST_BE")
        .unwrap_or_else(|| "undefined".into());
    let endpoint = format!("{host}/integrations/clickup/webhook");
    let body =
        json!({ "endpoint": endpoint, "events": TASK_WEBHOOK_EVENTS, "folder_id": folder_id });
    let res = Client::new(state, token)
        .post(&format!("/team/{workspace_id}/webhook"), &body)
        .await;
    let msg = match res {
        Ok(data) => match data.get("webhook").filter(|w| !w.is_null()) {
            Some(w) => {
                let s = |k: &str| nn(w.get(k)).map(|v| js_str(Some(v)));
                return Some((s("id"), s("secret")));
            }
            None => "Cannot read properties of undefined (reading 'id')".to_owned(),
        },
        Err(e) => e.message,
    };
    tracing::error!("Failed to register ClickUp webhook for folder {folder_id}: {msg}");
    None
}

/// `deleteWebhook(token, webhookId)` (best-effort).
async fn delete_webhook(state: &AppState, token: &str, webhook_id: &str) {
    if let Err(e) = Client::new(state, token)
        .delete(&format!("/webhook/{webhook_id}"))
        .await
    {
        tracing::error!(
            "Failed to delete ClickUp webhook {webhook_id}: {}",
            e.message
        );
    }
}

// ───────────────────────── getConnectionStatus ─────────────────────────

/// `data` `getConnectionStatus`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClickupStatus {
    pub connected: bool,
    pub workspace_id: Option<String>,
}

/// Node: `clickup.service.ts:559` `getConnectionStatus`.
pub async fn get_connection_status(state: &AppState, user_id: i32) -> Envelope<ClickupStatus> {
    let r: Res<ClickupStatus> = async {
        let Some(c) = credentials(state, user_id).await? else {
            return Ok(Envelope::fail(NO_COMPANY));
        };
        Ok(Envelope::ok(
            "Success",
            ClickupStatus {
                connected: c.token().is_some(),
                workspace_id: c.workspace_id,
            },
        ))
    }
    .await;
    caught("clickup-status", r)
}

// ───────────────────────── workspaces / spaces / lists ─────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct Workspace {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    color: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    avatar: Option<Value>,
}

/// Node: `clickup.service.ts:579` `getWorkspaces` (galat apa pun → pesan tetap).
pub async fn get_workspaces(state: &AppState, user_id: i32) -> Envelope<Vec<Workspace>> {
    let r: Res<Vec<Workspace>> = async {
        let Some(token) = get_token(state, user_id).await? else {
            return Ok(Envelope::fail("ClickUp token not configured"));
        };
        let res = Client::new(state, &token).get("/team").await?;
        let teams = js_array(res.get("teams"), "res.data.teams", "map")?;
        let data = teams
            .iter()
            .map(|t| Workspace {
                id: prop(t, "id"),
                name: prop(t, "name"),
                color: prop(t, "color"),
                avatar: prop(t, "avatar"),
            })
            .collect();
        Ok(Envelope::ok("Success", data))
    }
    .await;
    r.unwrap_or_else(|Thrown(e)| {
        tracing::error!(module = "clickup-workspaces", error = %e, "clickup");
        Envelope::fail("Failed to fetch ClickUp workspaces")
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct Space {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    private: Option<Value>,
}

/// Node: `clickup.service.ts:614` `getSpaces`.
pub async fn get_spaces(
    state: &AppState,
    user_id: i32,
    workspace_id: &str,
) -> Envelope<Vec<Space>> {
    let r: Res<Vec<Space>> = async {
        let Some(token) = get_token(state, user_id).await? else {
            return Ok(Envelope::fail("ClickUp token not configured"));
        };
        let res = Client::new(state, &token)
            .get(&format!("/team/{workspace_id}/space?archived=false"))
            .await?;
        let spaces = js_array(res.get("spaces"), "res.data.spaces", "map")?;
        let data = spaces
            .iter()
            .map(|s| Space {
                id: prop(s, "id"),
                name: prop(s, "name"),
                private: prop(s, "private"),
            })
            .collect();
        Ok(Envelope::ok("Success", data))
    }
    .await;
    r.unwrap_or_else(|Thrown(e)| {
        tracing::error!(module = "clickup-spaces", error = %e, "clickup");
        Envelope::fail("Failed to fetch ClickUp spaces")
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct FolderRef {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListEntry {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<Value>,
    folder: Option<FolderRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    task_count: Option<Value>,
}

/// Node: `clickup.service.ts:649` `getLists` (list tanpa folder + list di tiap folder).
pub async fn get_lists(state: &AppState, user_id: i32, space_id: &str) -> Envelope<Vec<ListEntry>> {
    let r: Res<Vec<ListEntry>> = async {
        let Some(token) = get_token(state, user_id).await? else {
            return Ok(Envelope::fail("ClickUp token not configured"));
        };
        let http = Client::new(state, &token);
        let folderless_path = format!("/space/{space_id}/list?archived=false");
        let folders_path = format!("/space/{space_id}/folder?archived=false");
        let (folderless, folders) =
            tokio::join!(http.get(&folderless_path), http.get(&folders_path));
        let (folderless, folders) = (folderless?, folders?);

        let mut data: Vec<ListEntry> =
            js_array(folderless.get("lists"), "folderlessRes.data.lists", "map")?
                .iter()
                .map(|l| ListEntry {
                    id: prop(l, "id"),
                    name: prop(l, "name"),
                    folder: None,
                    task_count: prop(l, "task_count"),
                })
                .collect();
        // `for...of` atas nilai non-iterable → TypeError.
        let folder_items = match folders.get("folders") {
            Some(Value::Array(a)) => a.clone(),
            _ => return Err(Thrown("foldersRes.data.folders is not iterable".into())),
        };
        for folder in &folder_items {
            let lists = http
                .get(&format!(
                    "/folder/{}/list?archived=false",
                    js_str(folder.get("id"))
                ))
                .await?;
            let Some(Value::Array(items)) = lists.get("lists") else {
                return Err(Thrown("listsRes.data.lists is not iterable".into()));
            };
            for l in items {
                data.push(ListEntry {
                    id: prop(l, "id"),
                    name: prop(l, "name"),
                    folder: Some(FolderRef {
                        id: prop(folder, "id"),
                        name: prop(folder, "name"),
                    }),
                    task_count: prop(l, "task_count"),
                });
            }
        }
        Ok(Envelope::ok("Success", data))
    }
    .await;
    r.unwrap_or_else(|Thrown(e)| {
        tracing::error!(module = "clickup-lists", error = %e, "clickup");
        Envelope::fail("Failed to fetch ClickUp lists")
    })
}

// ───────────────────────── sync-projects / synced-projects ─────────────────────────

#[derive(Debug, Clone, Default, Serialize)]
pub struct SyncProjectsResult {
    created: Vec<String>,
    skipped: Vec<String>,
}

/// Node: `clickup.service.ts:707` `syncProjects` (tanpa transaksi, seperti Node).
pub async fn sync_projects(
    state: &AppState,
    user_id: i32,
    lists: &[ListItem],
) -> Envelope<SyncProjectsResult> {
    let r: Res<SyncProjectsResult> = async {
        let Some(company_id) = mz_int_common::company::admin_company_id(state, user_id).await?
        else {
            return Ok(Envelope::fail(NO_COMPANY));
        };
        let project_type: Option<i32> = sqlx::query_scalar(
            r#"SELECT id FROM "projectType" WHERE status = 1 ORDER BY id ASC LIMIT 1"#,
        )
        .fetch_optional(&state.db)
        .await?;
        let Some(project_type_id) = project_type else {
            return Ok(Envelope::fail("No project type found"));
        };

        let mut results = SyncProjectsResult::default();
        for list in lists {
            let existing: Option<i32> =
                sqlx::query_scalar(r#"SELECT id FROM project WHERE "clickupListId" = $1 LIMIT 1"#)
                    .bind(&list.clickup_list_id)
                    .fetch_optional(&state.db)
                    .await?;
            if existing.is_some() {
                results.skipped.push(list.name.clone());
                continue;
            }
            let project_id: i32 = sqlx::query_scalar(
                r#"INSERT INTO project (code, name, "screenshotPerHour", "projectTypeId", "startDate",
                                       status, "clickupListId")
                   VALUES ($1, $2, 0, $3, $4, 1, $5) RETURNING id"#,
            )
            .bind(format!("CU-{}", list.clickup_list_id))
            .bind(&list.name)
            .bind(project_type_id)
            .bind(chrono::Utc::now().date_naive())
            .bind(&list.clickup_list_id)
            .fetch_one(&state.db)
            .await?;
            let pc = sqlx::query(
                r#"INSERT INTO "projectCompany" ("projectId", "companyId", status) VALUES ($1, $2, 1)"#,
            )
            .bind(project_id)
            .bind(company_id)
            .execute(&state.db);
            let pu = sqlx::query(
                r#"INSERT INTO "projectUser" ("projectId", "userId", status) VALUES ($1, $2, 1)"#,
            )
            .bind(project_id)
            .bind(user_id)
            .execute(&state.db);
            let (a, b) = tokio::join!(pc, pu);
            a?;
            b?;
            results.created.push(list.name.clone());
        }
        let message = format!(
            "Synced: {} created, {} already exist",
            results.created.len(),
            results.skipped.len()
        );
        Ok(Envelope::ok(message, results))
    }
    .await;
    caught("clickup-sync-projects", r)
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct SyncedProject {
    uuid: String,
    name: String,
    clickup_list_id: Option<String>,
    created_at: JsDate,
}

/// Node: `clickup.service.ts:776` `getSyncedProjects`.
pub async fn get_synced_projects(state: &AppState, user_id: i32) -> Envelope<Vec<SyncedProject>> {
    let r: Res<Vec<SyncedProject>> = async {
        let Some(company_id) = mz_int_common::company::admin_company_id(state, user_id).await?
        else {
            return Ok(Envelope::fail(NO_COMPANY));
        };
        let rows: Vec<(String, String, Option<String>, chrono::NaiveDateTime)> = sqlx::query_as(
            r#"SELECT p.uuid::text, p.name, p."clickupListId", p."createdAt" FROM project p
               WHERE p."clickupListId" IS NOT NULL AND p.status = 1
                 AND EXISTS (SELECT 1 FROM "projectCompany" pc
                             WHERE pc."projectId" = p.id AND pc."companyId" = $1 AND pc.status = 1)
               ORDER BY p."createdAt" DESC"#,
        )
        .bind(company_id)
        .fetch_all(&state.db)
        .await?;
        let data = rows
            .into_iter()
            .map(|(uuid, name, clickup_list_id, created_at)| SyncedProject {
                uuid,
                name,
                clickup_list_id,
                created_at: JsDate(created_at),
            })
            .collect();
        Ok(Envelope::ok("Success", data))
    }
    .await;
    caught("clickup-synced-projects", r)
}

// ───────────────────────── folders ─────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct NamedRef {
    uuid: String,
    name: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderEntry {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<Value>,
    space: FolderRef,
    task_count: Value,
    linked_projects: Vec<NamedRef>,
}

/// Node: `clickup.service.ts:809` `getFolders`.
pub async fn get_folders(state: &AppState, user_id: i32) -> Envelope<Vec<FolderEntry>> {
    let r: Res<Vec<FolderEntry>> = async {
        let Some(c) = credentials(state, user_id).await? else {
            return Ok(Envelope::fail(NO_COMPANY));
        };
        let Some(token) = c.token() else {
            return Ok(Envelope::fail(NO_TOKEN));
        };
        let Some(ws) = c.workspace() else {
            return Ok(Envelope::fail(NO_WORKSPACE));
        };
        let http = Client::new(state, token);
        let spaces_res = match http.get(&format!("/team/{ws}/space?archived=false")).await {
            Ok(v) => v,
            Err(HttpErr {
                status: Some(401), ..
            }) => return Ok(Envelope::fail(BAD_TOKEN_UPDATE)),
            Err(e) => return Err(e.into()),
        };
        let spaces = js_array(spaces_res.get("spaces"), "spaces", "map")?;

        // `Promise.all` + `.catch(() => [])` per space.
        let fetches = spaces.iter().map(|space| {
            let http = &http;
            async move {
                let path = format!("/space/{}/folder?archived=false", js_str(space.get("id")));
                match http.get(&path).await {
                    Ok(res) => match res.get("folders") {
                        Some(Value::Array(f)) => f
                            .iter()
                            .map(|folder| (space, folder.clone()))
                            .collect::<Vec<_>>(),
                        _ => Vec::new(),
                    },
                    Err(_) => Vec::new(),
                }
            }
        });
        let entries: Vec<(&Value, Value)> = futures::future::join_all(fetches)
            .await
            .into_iter()
            .flatten()
            .collect();

        let mappings: Vec<(String, String, String)> = sqlx::query_as(
            r#"SELECT m."clickupFolderId", p.uuid::text, p.name
               FROM "projectClickupMapping" m JOIN project p ON p.id = m."projectId"
               WHERE m."companyId" = $1 AND m.status = 1
               ORDER BY m.id"#,
        )
        .bind(c.company_id)
        .fetch_all(&state.db)
        .await?;
        let mut linked: HashMap<String, Vec<NamedRef>> = HashMap::new();
        for (folder_id, uuid, name) in mappings {
            linked
                .entry(folder_id)
                .or_default()
                .push(NamedRef { uuid, name });
        }

        let data = entries
            .into_iter()
            .map(|(space, folder)| {
                // `Map.get(folder.id)`: hanya kunci string yang bisa cocok.
                let linked_projects = folder
                    .get("id")
                    .and_then(Value::as_str)
                    .and_then(|id| linked.get(id))
                    .cloned()
                    .unwrap_or_default();
                FolderEntry {
                    id: prop(&folder, "id"),
                    name: prop(&folder, "name"),
                    space: FolderRef {
                        id: prop(space, "id"),
                        name: prop(space, "name"),
                    },
                    task_count: nn(folder.get("task_count")).cloned().unwrap_or(json!(0)),
                    linked_projects,
                }
            })
            .collect();
        Ok(Envelope::ok("Success", data))
    }
    .await;
    caught("clickup-folders", r)
}

// ───────────────────────── projects/link ─────────────────────────

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkProjectResult {
    uuid: String,
    project_uuid: String,
    project_name: String,
    clickup_folder_id: String,
    clickup_folder_name: String,
    clickup_space_name: String,
    webhook_registered: bool,
}

/// Node: `clickup.service.ts:905` `linkProject` (webhook best-effort, lalu upsert mapping).
pub async fn link_project(
    state: &AppState,
    user_id: i32,
    dto: &LinkProject,
) -> Envelope<LinkProjectResult> {
    let r: Res<LinkProjectResult> = async {
        let Some(c) = credentials(state, user_id).await? else {
            return Ok(Envelope::fail(NO_COMPANY));
        };
        let Some((project_id, project_uuid, project_name)) =
            company_project(state, &dto.project_uuid, c.company_id).await?
        else {
            return Ok(Envelope::fail("Project not found"));
        };

        let mut webhook_id = None;
        let mut webhook_secret = None;
        if let (Some(token), Some(ws)) = (c.token(), c.workspace())
            && let Some((id, secret)) =
                register_webhook(state, token, ws, &dto.clickup_folder_id).await
        {
            webhook_id = id;
            webhook_secret = secret;
        }

        let row: (String, String, String, String, Option<String>) = sqlx::query_as(
            r#"INSERT INTO "projectClickupMapping"
                 ("projectId", "companyId", "clickupFolderId", "clickupFolderName", "clickupSpaceId",
                  "clickupSpaceName", "clickupWebhookId", "clickupWebhookSecret", status)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 1)
               ON CONFLICT ("projectId", "companyId") DO UPDATE SET
                 "clickupFolderId" = EXCLUDED."clickupFolderId",
                 "clickupFolderName" = EXCLUDED."clickupFolderName",
                 "clickupSpaceId" = EXCLUDED."clickupSpaceId",
                 "clickupSpaceName" = EXCLUDED."clickupSpaceName",
                 "clickupWebhookId" = EXCLUDED."clickupWebhookId",
                 "clickupWebhookSecret" = EXCLUDED."clickupWebhookSecret",
                 status = 1, "updatedAt" = $9
               RETURNING uuid::text, "clickupFolderId", "clickupFolderName", "clickupSpaceName",
                         "clickupWebhookId""#,
        )
        .bind(project_id)
        .bind(c.company_id)
        .bind(&dto.clickup_folder_id)
        .bind(&dto.clickup_folder_name)
        .bind(&dto.clickup_space_id)
        .bind(&dto.clickup_space_name)
        .bind(webhook_id.as_deref())
        .bind(webhook_secret.as_deref())
        .bind(now_naive())
        .fetch_one(&state.db)
        .await?;
        Ok(Envelope::ok(
            "Project linked to ClickUp folder successfully",
            LinkProjectResult {
                uuid: row.0,
                project_uuid,
                project_name,
                clickup_folder_id: row.1,
                clickup_folder_name: row.2,
                clickup_space_name: row.3,
                webhook_registered: row.4.is_some_and(|w| !w.is_empty()),
            },
        ))
    }
    .await;
    caught("clickup-link-project", r)
}

/// Node: `clickup.service.ts:995` `unlinkProject`.
pub async fn unlink_project(state: &AppState, user_id: i32, project_uuid: &str) -> Envelope<()> {
    let r: Res<()> = async {
        let Some(c) = credentials(state, user_id).await? else {
            return Ok(Envelope::fail(NO_COMPANY));
        };
        let Some((project_id, _, _)) = company_project(state, project_uuid, c.company_id).await?
        else {
            return Ok(Envelope::fail("Project not found"));
        };
        let mapping: Option<(i32, Option<String>)> = sqlx::query_as(
            r#"SELECT id, "clickupWebhookId" FROM "projectClickupMapping"
               WHERE "projectId" = $1 AND "companyId" = $2 AND status = 1 LIMIT 1"#,
        )
        .bind(project_id)
        .bind(c.company_id)
        .fetch_optional(&state.db)
        .await?;
        let Some((mapping_id, webhook_id)) = mapping else {
            return Ok(Envelope::fail(
                "No active ClickUp link found for this project",
            ));
        };
        if let (Some(w), Some(token)) = (webhook_id.as_deref().filter(|w| !w.is_empty()), c.token())
        {
            delete_webhook(state, token, w).await;
        }
        sqlx::query(
            r#"UPDATE "projectClickupMapping" SET status = 0, "clickupWebhookId" = NULL,
                 "clickupWebhookSecret" = NULL, "updatedAt" = $2 WHERE id = $1"#,
        )
        .bind(mapping_id)
        .bind(now_naive())
        .execute(&state.db)
        .await?;
        Ok(ok_null("Project unlinked from ClickUp successfully"))
    }
    .await;
    caught("clickup-unlink-project", r)
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkedProject {
    uuid: String,
    project_uuid: String,
    project_name: String,
    clickup_folder_id: String,
    clickup_folder_name: String,
    clickup_space_name: String,
    webhook_active: bool,
    linked_at: JsDate,
}

/// Node: `clickup.service.ts:1054` `getLinkedProjects`.
pub async fn get_linked_projects(state: &AppState, user_id: i32) -> Envelope<Vec<LinkedProject>> {
    let r: Res<Vec<LinkedProject>> = async {
        let Some(c) = credentials(state, user_id).await? else {
            return Ok(Envelope::fail(NO_COMPANY));
        };
        #[allow(clippy::type_complexity)]
        let rows: Vec<(String, String, String, String, String, String, Option<String>, chrono::NaiveDateTime)> =
            sqlx::query_as(
                r#"SELECT m.uuid::text, p.uuid::text, p.name, m."clickupFolderId", m."clickupFolderName",
                          m."clickupSpaceName", m."clickupWebhookId", m."createdAt"
                   FROM "projectClickupMapping" m JOIN project p ON p.id = m."projectId"
                   WHERE m."companyId" = $1 AND m.status = 1
                   ORDER BY m."createdAt" DESC"#,
            )
            .bind(c.company_id)
            .fetch_all(&state.db)
            .await?;
        let data = rows
            .into_iter()
            .map(|r| LinkedProject {
                uuid: r.0,
                project_uuid: r.1,
                project_name: r.2,
                clickup_folder_id: r.3,
                clickup_folder_name: r.4,
                clickup_space_name: r.5,
                webhook_active: r.6.is_some_and(|w| !w.is_empty()),
                linked_at: JsDate(r.7),
            })
            .collect();
        Ok(Envelope::ok("Success", data))
    }
    .await;
    caught("clickup-linked-projects", r)
}

// ───────────────────────── members / users link ─────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct LinkedUserRef {
    uuid: String,
    name: String,
    username: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Member {
    clickup_user_id: String,
    clickup_username: Value,
    clickup_email: Value,
    clickup_avatar: Value,
    linked_user: Option<LinkedUserRef>,
}

/// Node: `clickup.service.ts:1096` `getMembers`.
pub async fn get_members(state: &AppState, user_id: i32) -> Envelope<Vec<Member>> {
    let r: Res<Vec<Member>> = async {
        let Some(c) = credentials(state, user_id).await? else {
            return Ok(Envelope::fail(NO_COMPANY));
        };
        let Some(token) = c.token() else {
            return Ok(Envelope::fail(NO_TOKEN));
        };
        let Some(ws) = c.workspace() else {
            return Ok(Envelope::fail(NO_WORKSPACE));
        };
        let teams_res = match Client::new(state, token).get("/team").await {
            Ok(v) => v,
            Err(HttpErr {
                status: Some(401), ..
            }) => return Ok(Envelope::fail(BAD_TOKEN_UPDATE)),
            Err(e) => return Err(e.into()),
        };
        let teams = match nn(teams_res.get("teams")) {
            None => &[][..],
            Some(v) => js_array(Some(v), "teams", "find")?,
        };
        let Some(workspace) = teams.iter().find(|t| js_str(t.get("id")) == ws) else {
            return Ok(Envelope::fail(
                "Workspace not found. Please re-select your ClickUp workspace.",
            ));
        };

        let mappings: Vec<(String, String, String, String)> = sqlx::query_as(
            r#"SELECT m."clickupUserId", u.uuid::text, u.name, u.username
               FROM "userClickupMapping" m JOIN users u ON u.id = m."userId"
               WHERE m."companyId" = $1 AND m.status = 1
               ORDER BY m.id"#,
        )
        .bind(c.company_id)
        .fetch_all(&state.db)
        .await?;
        let linked: HashMap<String, LinkedUserRef> = mappings
            .into_iter()
            .map(|(cid, uuid, name, username)| {
                (
                    cid,
                    LinkedUserRef {
                        uuid,
                        name,
                        username,
                    },
                )
            })
            .collect();

        let members = match nn(workspace.get("members")) {
            None => &[][..],
            Some(v) => js_array(Some(v), "members", "map")?,
        };
        let data = members
            .iter()
            .map(|m| {
                let user = Some(m).and_then(|x| nn(x.get("user")));
                let pick = |a: &str, b: &str| {
                    nn(field(user, a))
                        .or_else(|| nn(m.get(b)))
                        .cloned()
                        .unwrap_or(Value::Null)
                };
                let id = js_str(nn(field(user, "id")).or_else(|| m.get("id")));
                Member {
                    linked_user: linked.get(&id).cloned(),
                    clickup_user_id: id,
                    clickup_username: pick("username", "username"),
                    clickup_email: pick("email", "email"),
                    clickup_avatar: pick("profilePicture", "profilePicture"),
                }
            })
            .collect();
        Ok(Envelope::ok("Success", data))
    }
    .await;
    caught("clickup-members", r)
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkUserResult {
    uuid: String,
    user_uuid: String,
    user_name: String,
    clickup_user_id: String,
    clickup_username: Option<String>,
    clickup_email: Option<String>,
}

/// Node: `clickup.service.ts:1184` `linkUser`.
pub async fn link_user(state: &AppState, user_id: i32, dto: &LinkUser) -> Envelope<LinkUserResult> {
    let r: Res<LinkUserResult> = async {
        let Some(c) = credentials(state, user_id).await? else {
            return Ok(Envelope::fail(NO_COMPANY));
        };
        let Some((target_id, target_uuid, target_name, _)) =
            company_user(state, &dto.user_uuid, c.company_id).await?
        else {
            return Ok(Envelope::fail("User not found in this company"));
        };
        let conflicting: Option<String> = sqlx::query_scalar(
            r#"SELECT u.name FROM "userClickupMapping" m JOIN users u ON u.id = m."userId"
               WHERE m."clickupUserId" = $1 AND m."companyId" = $2 AND m.status = 1
                 AND m."userId" <> $3
               LIMIT 1"#,
        )
        .bind(&dto.clickup_user_id)
        .bind(c.company_id)
        .bind(target_id)
        .fetch_optional(&state.db)
        .await?;
        if let Some(name) = conflicting {
            return Ok(Envelope::fail(format!(
                "This ClickUp member is already linked to user '{name}'"
            )));
        }
        let row: (String, String, Option<String>, Option<String>) = sqlx::query_as(
            r#"INSERT INTO "userClickupMapping"
                 ("userId", "companyId", "clickupUserId", "clickupUsername", "clickupEmail",
                  "clickupAvatar", status)
               VALUES ($1, $2, $3, $4, $5, $6, 1)
               ON CONFLICT ("userId", "companyId") DO UPDATE SET
                 "clickupUserId" = EXCLUDED."clickupUserId",
                 "clickupUsername" = EXCLUDED."clickupUsername",
                 "clickupEmail" = EXCLUDED."clickupEmail",
                 "clickupAvatar" = EXCLUDED."clickupAvatar",
                 status = 1, "updatedAt" = $7
               RETURNING uuid::text, "clickupUserId", "clickupUsername", "clickupEmail""#,
        )
        .bind(target_id)
        .bind(c.company_id)
        .bind(&dto.clickup_user_id)
        .bind(dto.clickup_username.as_deref())
        .bind(dto.clickup_email.as_deref())
        .bind(dto.clickup_avatar.as_deref())
        .bind(now_naive())
        .fetch_one(&state.db)
        .await?;
        Ok(Envelope::ok(
            "User linked to ClickUp member successfully",
            LinkUserResult {
                uuid: row.0,
                user_uuid: target_uuid,
                user_name: target_name,
                clickup_user_id: row.1,
                clickup_username: row.2,
                clickup_email: row.3,
            },
        ))
    }
    .await;
    caught("clickup-link-user", r)
}

/// Node: `clickup.service.ts:1274` `unlinkUser`.
pub async fn unlink_user(state: &AppState, user_id: i32, user_uuid: &str) -> Envelope<()> {
    let r: Res<()> = async {
        let Some(c) = credentials(state, user_id).await? else {
            return Ok(Envelope::fail(NO_COMPANY));
        };
        let Some((target_id, ..)) = company_user(state, user_uuid, c.company_id).await? else {
            return Ok(Envelope::fail("User not found in this company"));
        };
        let mapping: Option<i32> = sqlx::query_scalar(
            r#"SELECT id FROM "userClickupMapping"
               WHERE "userId" = $1 AND "companyId" = $2 AND status = 1 LIMIT 1"#,
        )
        .bind(target_id)
        .bind(c.company_id)
        .fetch_optional(&state.db)
        .await?;
        let Some(mapping_id) = mapping else {
            return Ok(Envelope::fail("No active ClickUp link found for this user"));
        };
        sqlx::query(
            r#"UPDATE "userClickupMapping" SET status = 0, "updatedAt" = $2 WHERE id = $1"#,
        )
        .bind(mapping_id)
        .bind(now_naive())
        .execute(&state.db)
        .await?;
        Ok(ok_null("User unlinked from ClickUp successfully"))
    }
    .await;
    caught("clickup-unlink-user", r)
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkedUser {
    uuid: String,
    user_uuid: String,
    user_name: String,
    user_username: String,
    clickup_user_id: String,
    clickup_username: Option<String>,
    clickup_email: Option<String>,
    linked_at: JsDate,
}

/// Node: `clickup.service.ts:1325` `getLinkedUsers`.
pub async fn get_linked_users(state: &AppState, user_id: i32) -> Envelope<Vec<LinkedUser>> {
    let r: Res<Vec<LinkedUser>> = async {
        let Some(c) = credentials(state, user_id).await? else {
            return Ok(Envelope::fail(NO_COMPANY));
        };
        #[allow(clippy::type_complexity)]
        let rows: Vec<(
            String,
            String,
            String,
            String,
            String,
            Option<String>,
            Option<String>,
            chrono::NaiveDateTime,
        )> = sqlx::query_as(
            r#"SELECT m.uuid::text, u.uuid::text, u.name, u.username, m."clickupUserId",
                          m."clickupUsername", m."clickupEmail", m."createdAt"
                   FROM "userClickupMapping" m JOIN users u ON u.id = m."userId"
                   WHERE m."companyId" = $1 AND m.status = 1
                   ORDER BY m."createdAt" DESC"#,
        )
        .bind(c.company_id)
        .fetch_all(&state.db)
        .await?;
        let data = rows
            .into_iter()
            .map(|r| LinkedUser {
                uuid: r.0,
                user_uuid: r.1,
                user_name: r.2,
                user_username: r.3,
                clickup_user_id: r.4,
                clickup_username: r.5,
                clickup_email: r.6,
                linked_at: JsDate(r.7),
            })
            .collect();
        Ok(Envelope::ok("Success", data))
    }
    .await;
    caught("clickup-linked-users", r)
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct LinkedProjectItem {
    project_uuid: String,
    project_name: String,
    clickup_folder_id: String,
    clickup_folder_name: String,
}

/// Node: `clickup.service.ts:1364` `getLinkedProjectList`.
pub async fn get_linked_project_list(
    state: &AppState,
    user_id: i32,
) -> Envelope<Vec<LinkedProjectItem>> {
    let r: Res<Vec<LinkedProjectItem>> = async {
        let Some(c) = credentials(state, user_id).await? else {
            return Ok(Envelope::fail(NO_COMPANY));
        };
        let data: Vec<LinkedProjectItem> = sqlx::query_as(
            r#"SELECT p.uuid::text AS "project_uuid", p.name AS "project_name",
                      m."clickupFolderId" AS "clickup_folder_id",
                      m."clickupFolderName" AS "clickup_folder_name"
               FROM "projectClickupMapping" m JOIN project p ON p.id = m."projectId"
               WHERE m."companyId" = $1 AND m.status = 1
               ORDER BY m."createdAt" ASC"#,
        )
        .bind(c.company_id)
        .fetch_all(&state.db)
        .await?;
        Ok(Envelope::ok("Success", data))
    }
    .await;
    caught("clickup-linked-project-list", r)
}

// ───────────────────────── sync-tasks ─────────────────────────

/// Node: `clickup.service.ts:1508` `syncTasks` (juga dipakai `/integrations` agregat).
/// Tidak pernah melempar: semua galat menjadi `{ success:false, message }`.
pub async fn sync_tasks(
    state: &AppState,
    user_id: i32,
    project_uuid: &str,
    include_closed: bool,
    include_subtasks: bool,
) -> Envelope<SyncTotals> {
    let r: Res<SyncTotals> = async {
        let Some(c) = credentials(state, user_id).await? else {
            return Ok(Envelope::fail(NO_COMPANY));
        };
        let Some(token) = c.token() else {
            return Ok(Envelope::fail(NO_TOKEN));
        };
        let mapping: Option<(i32, String, Option<String>, i32)> = match parse_uuid(project_uuid) {
            None => None,
            Some(u) => {
                sqlx::query_as(
                    r#"SELECT m.id, m."clickupFolderId", m."clickupWebhookId", p.id
                       FROM "projectClickupMapping" m JOIN project p ON p.id = m."projectId"
                       WHERE m."companyId" = $1 AND m.status = 1 AND p.uuid = $2 AND p.status = 1
                       LIMIT 1"#,
                )
                .bind(c.company_id)
                .bind(u)
                .fetch_optional(&state.db)
                .await?
            }
        };
        let Some((mapping_id, folder_id, webhook_id, project_id)) = mapping else {
            return Ok(Envelope::fail(
                "Project not found or not linked to a ClickUp folder",
            ));
        };

        if webhook_id.as_deref().is_none_or(str::is_empty)
            && let Some(ws) = c.workspace()
            && let Some((wid, secret)) = register_webhook(state, token, ws, &folder_id).await
        {
            sqlx::query(
                r#"UPDATE "projectClickupMapping" SET "clickupWebhookId" = $2,
                     "clickupWebhookSecret" = $3, "updatedAt" = $4 WHERE id = $1"#,
            )
            .bind(mapping_id)
            .bind(wid.as_deref())
            .bind(secret.as_deref())
            .bind(now_naive())
            .execute(&state.db)
            .await?;
            tracing::info!("Webhook registered for mapping {mapping_id} during sync-tasks");
        }

        match sync_tasks_internal(
            state,
            c.company_id,
            project_id,
            &folder_id,
            token,
            include_closed,
            include_subtasks,
            None,
            false,
        )
        .await
        {
            Ok(t) => Ok(Envelope::ok(
                format!(
                    "Sync complete: {} tasks created, {} tasks updated",
                    t.created, t.updated
                ),
                t,
            )),
            Err(Thrown(m))
                if m == "Invalid ClickUp API token" || m == "ClickUp folder not found" =>
            {
                Ok(Envelope::fail(m))
            }
            Err(e) => Err(e),
        }
    }
    .await;
    caught("clickup-sync-tasks", r)
}

// ───────────────────────── task status ─────────────────────────

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusChanged {
    task_uuid: String,
    status: String,
}

/// `task.findFirst({ where: { uuid, companyId, source: 'clickup' } })`.
async fn clickup_task(
    state: &AppState,
    uuid: &str,
    company_id: i32,
) -> sqlx::Result<Option<(i32, Option<String>, Option<Value>)>> {
    let Some(u) = parse_uuid(uuid) else {
        return Ok(None);
    };
    sqlx::query_as(
        r#"SELECT id, "externalId", "externalMeta" FROM task
           WHERE uuid = $1 AND "companyId" = $2 AND source = 'clickup' LIMIT 1"#,
    )
    .bind(u)
    .bind(company_id)
    .fetch_optional(&state.db)
    .await
}

/// Node: `clickup.service.ts:1602` `changeTaskStatus`.
pub async fn change_task_status(
    state: &AppState,
    user_id: i32,
    task_uuid: &str,
    status: &str,
) -> Envelope<StatusChanged> {
    let r: Res<StatusChanged> = async {
        let Some(c) = credentials(state, user_id).await? else {
            return Ok(Envelope::fail(NO_COMPANY));
        };
        let Some(token) = c.token() else {
            return Ok(Envelope::fail(NO_TOKEN));
        };
        let Some((task_id, external_id, meta)) =
            clickup_task(state, task_uuid, c.company_id).await?
        else {
            return Ok(Envelope::fail("Task not found"));
        };
        let http = Client::new(state, token);

        let list_id = field(meta.as_ref(), "listId");
        let mut resolved = status.to_owned();
        let mut valid: Vec<String> = Vec::new();
        if truthy(list_id) {
            // `listRes.data?.statuses ?? []`; `.map` pada non-array / `s.status` pada elemen
            // null → TypeError yang ditelan → jatuh ke validasi ClickUp.
            if let Ok(list) = http.get(&format!("/list/{}", js_str(list_id))).await
                && let Some(statuses) = match nn(list.get("statuses")) {
                    None => Some(&[][..]),
                    Some(Value::Array(a)) => Some(a.as_slice()),
                    Some(_) => None,
                }
                && statuses.iter().all(|s| !s.is_null())
            {
                let raw: Vec<Option<&Value>> = statuses.iter().map(|s| s.get("status")).collect();
                valid = raw.iter().map(|s| js_str(*s)).collect();
                let wanted = status.to_lowercase();
                let mut matched = None;
                let mut threw = false;
                for s in &raw {
                    match s {
                        Some(Value::String(x)) => {
                            if x.to_lowercase() == wanted {
                                matched = Some(x.clone());
                                break;
                            }
                        }
                        // `toLowerCase` pada non-string → TypeError (ditelan).
                        _ => {
                            threw = true;
                            break;
                        }
                    }
                }
                if !threw {
                    match matched {
                        Some(m) => resolved = m,
                        None => {
                            return Ok(Envelope::fail(format!(
                                "Invalid status '{status}' for this task. Valid statuses: {}",
                                valid.join(", ")
                            )));
                        }
                    }
                }
            }
        }

        let ext = js_str(Some(&external_id.map_or(Value::Null, Value::String)));
        match http
            .put(&format!("/task/{ext}"), &json!({ "status": resolved }))
            .await
        {
            Ok(_) => {}
            Err(HttpErr {
                status: Some(401), ..
            }) => return Ok(Envelope::fail("Invalid ClickUp API token")),
            Err(HttpErr {
                status: Some(404), ..
            }) => return Ok(Envelope::fail("Task not found in ClickUp")),
            Err(HttpErr {
                status: Some(400), ..
            }) => {
                let hint = if valid.is_empty() {
                    String::new()
                } else {
                    format!(" Valid statuses: {}", valid.join(", "))
                };
                return Ok(Envelope::fail(format!(
                    "Invalid status '{status}' for this task.{hint}"
                )));
            }
            Err(e) => return Err(e.into()),
        }

        let now = now_naive();
        sqlx::query(
            r#"UPDATE task SET status = $2, "lastSyncedAt" = $3, "updatedAt" = $3 WHERE id = $1"#,
        )
        .bind(task_id)
        .bind(&resolved)
        .bind(now)
        .execute(&state.db)
        .await?;

        // Komentar pelaku (best-effort).
        let comment: Result<(), Thrown> = async {
            let actor: Option<(String, String)> =
                sqlx::query_as("SELECT name, username FROM users WHERE id = $1 LIMIT 1")
                    .bind(user_id)
                    .fetch_optional(&state.db)
                    .await?;
            let label = actor.map_or_else(
                || format!("user #{user_id}"),
                |(name, username)| format!("{name} ({username})").trim().to_owned(),
            );
            http.post(
                &format!("/task/{ext}/comment"),
                &json!({
                    "comment_text": format!("Status changed to \"{resolved}\" by {label}"),
                    "notify_all": false,
                }),
            )
            .await?;
            Ok(())
        }
        .await;
        if let Err(Thrown(m)) = comment {
            tracing::error!("ClickUp status-change comment failed: {m}");
        }

        Ok(Envelope::ok(
            "Task status updated successfully",
            StatusChanged {
                task_uuid: task_uuid.to_owned(),
                status: resolved,
            },
        ))
    }
    .await;
    caught("clickup-change-task-status", r)
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskStatus {
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    color: Option<Value>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    kind: Option<Value>,
}

/// Node: `clickup.service.ts:1738` `getTaskStatuses`.
pub async fn get_task_statuses(
    state: &AppState,
    user_id: i32,
    task_uuid: &str,
) -> Envelope<Vec<TaskStatus>> {
    let r: Res<Vec<TaskStatus>> = async {
        let Some(c) = credentials(state, user_id).await? else {
            return Ok(Envelope::fail(NO_COMPANY));
        };
        let Some(token) = c.token() else {
            return Ok(Envelope::fail(NO_TOKEN));
        };
        let Some((_, _, meta)) = clickup_task(state, task_uuid, c.company_id).await? else {
            return Ok(Envelope::fail("Task not found"));
        };
        let list_id = field(meta.as_ref(), "listId");
        if !truthy(list_id) {
            return Ok(Envelope::fail(
                "List info not available, please re-sync the task",
            ));
        }
        let list = Client::new(state, token)
            .get(&format!("/list/{}", js_str(list_id)))
            .await?;
        let statuses = match nn(list.get("statuses")) {
            None => &[][..],
            Some(v) => js_array(Some(v), "statuses", "map")?,
        };
        let data = statuses
            .iter()
            .map(|s| TaskStatus {
                name: prop(s, "status"),
                color: prop(s, "color"),
                kind: prop(s, "type"),
            })
            .collect();
        Ok(Envelope::ok("Statuses fetched successfully", data))
    }
    .await;
    caught("clickup-get-task-statuses", r)
}

// ───────────────────────── project tasks ─────────────────────────

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskRow {
    uuid: String,
    source: String,
    external_id: Option<String>,
    external_url: Option<String>,
    external_meta: Option<Box<RawValue>>,
    name: String,
    description: Option<String>,
    status: Option<String>,
    priority: Option<i32>,
    priority_label: Option<String>,
    due_date: Option<JsDate>,
    start_date: Option<JsDate>,
    assignees: Option<Box<RawValue>>,
    tags: Option<Box<RawValue>>,
    last_synced_at: JsDate,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectTasks {
    project_name: String,
    total: i64,
    page: i64,
    limit: i64,
    total_pages: i64,
    tasks: Vec<TaskRow>,
}

#[derive(sqlx::FromRow)]
struct TaskDbRow {
    uuid: String,
    source: String,
    external_id: Option<String>,
    external_url: Option<String>,
    external_meta: Option<String>,
    name: String,
    description: Option<String>,
    status: Option<String>,
    priority: Option<i32>,
    priority_label: Option<String>,
    due_date: Option<chrono::NaiveDateTime>,
    start_date: Option<chrono::NaiveDateTime>,
    assignees: Option<String>,
    tags: Option<String>,
    last_synced_at: chrono::NaiveDateTime,
}

/// Filter `where` `getProjectTasks` (dipakai `count` dan `findMany`).
fn push_task_filter(
    qb: &mut QueryBuilder<'_, Postgres>,
    project_id: i32,
    company_id: i32,
    q: &ProjectTasksQuery,
) {
    qb.push(r#" WHERE "projectId" = "#)
        .push_bind(project_id)
        .push(r#" AND "companyId" = "#)
        .push_bind(company_id)
        .push(" AND source = 'clickup'");
    // Prisma `contains`/`equals` + `mode: 'insensitive'` = ILIKE tanpa escape `%`/`_`.
    if let Some(s) = q.search.as_deref().filter(|s| !s.is_empty()) {
        qb.push(" AND name ILIKE ").push_bind(format!("%{s}%"));
    }
    if let Some(s) = q.status.as_deref().filter(|s| !s.is_empty()) {
        qb.push(" AND status ILIKE ").push_bind(s.to_owned());
    }
    // username/email elemen array `assignees` memuat teks (case-insensitive); lihat mz-int-common.
    if let Some(s) = q.assignee.as_deref().filter(|s| !s.is_empty()) {
        mz_int_common::assignee::push_assignee_filter(qb, s);
    }
    if let Some(d) = q
        .start_date
        .as_deref()
        .filter(|s| !s.is_empty())
        .and_then(parse_js_date)
    {
        qb.push(r#" AND "dueDate" >= "#).push_bind(d);
    }
    if let Some(d) = q
        .end_date
        .as_deref()
        .filter(|s| !s.is_empty())
        .and_then(parse_js_date)
    {
        qb.push(r#" AND "dueDate" <= "#).push_bind(d);
    }
}

/// Node: `clickup.service.ts:1788` `getProjectTasks`.
pub async fn get_project_tasks(
    state: &AppState,
    user_id: i32,
    project_uuid: &str,
    q: &ProjectTasksQuery,
) -> Envelope<ProjectTasks> {
    let r: Res<ProjectTasks> = async {
        let Some(c) = credentials(state, user_id).await? else {
            return Ok(Envelope::fail(NO_COMPANY));
        };
        let Some((project_id, _, project_name)) =
            company_project(state, project_uuid, c.company_id).await?
        else {
            return Ok(Envelope::fail("Project not found"));
        };
        let skip = q.page.saturating_sub(1).saturating_mul(q.limit);

        let mut count_qb: QueryBuilder<Postgres> = QueryBuilder::new("SELECT COUNT(*) FROM task");
        push_task_filter(&mut count_qb, project_id, c.company_id, q);
        let mut rows_qb: QueryBuilder<Postgres> = QueryBuilder::new(
            r#"SELECT uuid::text AS uuid, source, "externalId" AS external_id,
                      "externalUrl" AS external_url, "externalMeta"::text AS external_meta, name,
                      description, status, priority, "priorityLabel" AS priority_label,
                      "dueDate" AS due_date, "startDate" AS start_date,
                      assignees::text AS assignees, tags::text AS tags,
                      "lastSyncedAt" AS last_synced_at
               FROM task"#,
        );
        push_task_filter(&mut rows_qb, project_id, c.company_id, q);
        rows_qb
            .push(r#" ORDER BY "createdAt" ASC OFFSET "#)
            .push_bind(skip)
            .push(" LIMIT ")
            .push_bind(q.limit);

        let count_q = count_qb.build_query_scalar::<i64>();
        let rows_q = rows_qb.build_query_as::<TaskDbRow>();
        let (total, rows) = tokio::join!(count_q.fetch_one(&state.db), rows_q.fetch_all(&state.db));
        let (total, rows) = (total?, rows?);

        let tasks = rows
            .into_iter()
            .map(|r| TaskRow {
                uuid: r.uuid,
                source: r.source,
                external_id: r.external_id,
                external_url: r.external_url,
                external_meta: raw_opt(r.external_meta.as_deref()),
                name: r.name,
                description: r.description,
                status: r.status,
                priority: r.priority,
                priority_label: r.priority_label,
                due_date: r.due_date.map(JsDate),
                start_date: r.start_date.map(JsDate),
                assignees: raw_opt(r.assignees.as_deref()),
                tags: raw_opt(r.tags.as_deref()),
                last_synced_at: JsDate(r.last_synced_at),
            })
            .collect();
        Ok(Envelope::ok(
            "Success",
            ProjectTasks {
                project_name,
                total,
                page: q.page,
                limit: q.limit,
                total_pages: if q.limit > 0 {
                    (total + q.limit - 1) / q.limit
                } else {
                    0
                },
                tasks,
            },
        ))
    }
    .await;
    caught("clickup-get-project-tasks", r)
}
