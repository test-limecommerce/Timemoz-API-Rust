//! Port `src/integrations/integrations.controller.ts` + `integrations.service.ts`: empat route
//! agregat `/integrations/*` di atas ClickUp, Jira, dan Discord.
//!
//! Kelas controller: `@RequireFeature('integrations')`, `@SkipThrottle()` (tanpa limiter),
//! tiap method `@HttpCode(200)` + `@Roles(...)` + `@UseGuards(JwtAuthGuard, RolesGuard)`.
//! `ValidationPipe({ transform: true })` global (tanpa whitelist). Semua method service
//! menangkap error sendiri dan membalas `{ success:false, message, data:null }` dengan 200.

use axum::extract::{Path, RawQuery, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use mz_audit::AuditHandle;
use mz_core::enums::Role;
use mz_core::extract::{Validate, ValidatedJson, check_int_range as check_query_int};
use mz_core::gate::guard;
use mz_core::jsfmt::number as js_number;
use mz_core::qs::QVal;
use mz_core::time::JsDate;
use mz_core::{ApiResult, AppState, RequestCtx, RouteSpec};
use mz_int_common::envelope::Envelope;
use mz_int_common::jsonb;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json::value::RawValue;
use sqlx::{Postgres, QueryBuilder};

use mz_core::iso8601::{DateArg, parse_date_arg};

const FEATURE: &str = "integrations";

/// `GET status`: `@Roles(SYSTEM_ADMIN, SUPER_ADMIN, ADMIN)`.
const ADMIN_SPEC: RouteSpec = RouteSpec::jwt()
    .roles(&[Role::SYSTEM_ADMIN, Role::SUPER_ADMIN, Role::ADMIN])
    .feature(FEATURE);
/// `GET projects/linked`, `GET projects/:projectUuid/tasks`: + MANAGER, STAFF.
const MEMBER_SPEC: RouteSpec = RouteSpec::jwt()
    .roles(&[
        Role::SYSTEM_ADMIN,
        Role::SUPER_ADMIN,
        Role::ADMIN,
        Role::MANAGER,
        Role::STAFF,
    ])
    .feature(FEATURE);
/// `POST projects/:projectUuid/sync-tasks`: + MANAGER.
const SYNC_SPEC: RouteSpec = RouteSpec::jwt()
    .roles(&[
        Role::SYSTEM_ADMIN,
        Role::SUPER_ADMIN,
        Role::ADMIN,
        Role::MANAGER,
    ])
    .feature(FEATURE);

pub fn router(state: &AppState) -> Router<AppState> {
    Router::new()
        .route(
            "/integrations/status",
            guard(state, ADMIN_SPEC, get(h_status)),
        )
        .route(
            "/integrations/projects/linked",
            guard(state, MEMBER_SPEC, get(h_linked)),
        )
        .route(
            "/integrations/projects/{projectUuid}/tasks",
            guard(state, MEMBER_SPEC, get(h_tasks)),
        )
        .route(
            "/integrations/projects/{projectUuid}/sync-tasks",
            guard(state, SYNC_SPEC, post(h_sync)),
        )
}

/// `{ success:false, message: error.message, data:null }` untuk error tak terduga (Prisma di
/// Node; teks pesan Prisma tidak bisa ditiru persis, lihat deviasi).
fn unexpected<T>(err: impl std::fmt::Display) -> Envelope<T> {
    tracing::error!(error = %err, "integrations");
    Envelope::fail(err.to_string())
}

// ── GET /integrations/status ────────────────────────────────────────────────

/// `x.data ?? default` (urutan kunci default = literal Node).
#[derive(Serialize)]
#[serde(untagged)]
enum OrDefault<A, B> {
    Got(A),
    Default(B),
}

fn or_default<A, B>(data: Option<A>, default: B) -> OrDefault<A, B> {
    match data {
        Some(a) => OrDefault::Got(a),
        None => OrDefault::Default(default),
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ClickupOff {
    connected: bool,
    workspace_id: Option<()>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct JiraOff {
    connected: bool,
    cloud_id: Option<()>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DiscordOff {
    connected: bool,
    guild_id: Option<()>,
    guild_name: Option<()>,
}

#[derive(Serialize)]
struct StatusData<C, J, D> {
    clickup: C,
    jira: J,
    discord: D,
}

/// Node: `src/integrations/integrations.controller.ts:31` → `integrations.service.ts:18`
/// (`getIntegrationsStatus`). Ketiga `getConnectionStatus` menangkap error sendiri, jadi
/// cabang `catch` agregat tidak pernah tercapai.
async fn h_status(State(state): State<AppState>, ctx: RequestCtx) -> ApiResult<Response> {
    let user_id = ctx.require_user_id()?;
    let (clickup, jira, discord) = tokio::join!(
        mz_int_clickup::get_connection_status(&state, user_id),
        mz_int_jira::get_connection_status(&state, user_id),
        mz_int_discord::get_connection_status(&state, user_id),
    );
    let body = Envelope::ok(
        "Success",
        StatusData {
            clickup: or_default(
                clickup.data,
                ClickupOff {
                    connected: false,
                    workspace_id: None,
                },
            ),
            jira: or_default(
                jira.data,
                JiraOff {
                    connected: false,
                    cloud_id: None,
                },
            ),
            discord: or_default(
                discord.data,
                DiscordOff {
                    connected: false,
                    guild_id: None,
                    guild_name: None,
                },
            ),
        },
    );
    Ok(json_body(&body))
}

/// `Json<T>` menserialisasi struct langsung (urutan field terjaga, beda dengan `Value`).
fn json_body<T: Serialize>(body: &T) -> Response {
    Json(body).into_response()
}

// ── GET /integrations/projects/linked ───────────────────────────────────────

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ClickupLink {
    folder_id: String,
    folder_name: String,
    space_name: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct JiraLink {
    project_id: String,
    project_key: String,
    project_name: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DiscordLink {
    channel_id: String,
    channel_name: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LinkedProject {
    project_uuid: String,
    project_name: String,
    clickup: Option<ClickupLink>,
    jira: Option<JiraLink>,
    discord: Option<DiscordLink>,
}

/// `project: { select: { uuid, name } }` Prisma = query kedua `WHERE id IN (...)`.
async fn project_names(
    state: &AppState,
    ids: &[i32],
) -> sqlx::Result<std::collections::HashMap<i32, (String, String)>> {
    let rows: Vec<(i32, String, String)> =
        sqlx::query_as(r#"SELECT id, uuid::text, name FROM project WHERE id = ANY($1)"#)
            .bind(ids)
            .fetch_all(&state.db)
            .await?;
    Ok(rows.into_iter().map(|(id, u, n)| (id, (u, n))).collect())
}

/// Map JS (`projectMap`): urutan sisip = pertama kali uuid terlihat; `set` pada kunci yang
/// sudah ada mengganti nilainya di posisi semula.
fn slot<'a>(list: &'a mut Vec<LinkedProject>, uuid: &str, name: &str) -> &'a mut LinkedProject {
    let idx = match list.iter().position(|p| p.project_uuid == uuid) {
        Some(i) => i,
        None => {
            list.push(LinkedProject {
                project_uuid: uuid.to_owned(),
                project_name: name.to_owned(),
                clickup: None,
                jira: None,
                discord: None,
            });
            list.len() - 1
        }
    };
    #[allow(clippy::indexing_slicing)]
    &mut list[idx]
}

async fn linked_projects(
    state: &AppState,
    user_id: i32,
) -> sqlx::Result<Envelope<Vec<LinkedProject>>> {
    let Some(company_id) = mz_core::company::find_company_id(&state.db, user_id).await? else {
        return Ok(Envelope::fail("Company not found"));
    };
    // Tanpa ORDER BY, sama dengan `findMany` Node (urutan fisik tabel).
    let clickup: Vec<(i32, String, String, String)> = sqlx::query_as(
        r#"SELECT "projectId", "clickupFolderId", "clickupFolderName", "clickupSpaceName"
             FROM "projectClickupMapping" WHERE "companyId" = $1 AND status = 1"#,
    )
    .bind(company_id)
    .fetch_all(&state.db)
    .await?;
    let jira: Vec<(i32, String, String, String)> = sqlx::query_as(
        r#"SELECT "projectId", "jiraProjectId", "jiraProjectKey", "jiraProjectName"
             FROM "projectJiraMapping" WHERE "companyId" = $1 AND status = 1"#,
    )
    .bind(company_id)
    .fetch_all(&state.db)
    .await?;
    let discord: Vec<(i32, String, String)> = sqlx::query_as(
        r#"SELECT "projectId", "discordChannelId", "discordChannelName"
             FROM "projectDiscordMapping" WHERE "companyId" = $1 AND status = 1"#,
    )
    .bind(company_id)
    .fetch_all(&state.db)
    .await?;
    let mut ids: Vec<i32> = clickup.iter().map(|r| r.0).collect();
    ids.extend(jira.iter().map(|r| r.0));
    ids.extend(discord.iter().map(|r| r.0));
    let projects = project_names(state, &ids).await?;
    let name_of = |id: i32| projects.get(&id).cloned().unwrap_or_default();

    let mut list: Vec<LinkedProject> = Vec::new();
    for (pid, folder_id, folder_name, space_name) in clickup {
        let (uuid, name) = name_of(pid);
        let p = slot(&mut list, &uuid, &name);
        // `projectMap.set(...)` menimpa entri utuh (jira/discord kembali null).
        p.project_name = name;
        p.jira = None;
        p.discord = None;
        p.clickup = Some(ClickupLink {
            folder_id,
            folder_name,
            space_name: Some(space_name),
        });
    }
    for (pid, project_id, project_key, project_name) in jira {
        let (uuid, name) = name_of(pid);
        slot(&mut list, &uuid, &name).jira = Some(JiraLink {
            project_id,
            project_key,
            project_name,
        });
    }
    for (pid, channel_id, channel_name) in discord {
        let (uuid, name) = name_of(pid);
        slot(&mut list, &uuid, &name).discord = Some(DiscordLink {
            channel_id,
            channel_name,
        });
    }
    Ok(Envelope::ok("Success", list))
}

/// Node: `src/integrations/integrations.controller.ts:40` → `getAllLinkedProjects`.
async fn h_linked(State(state): State<AppState>, ctx: RequestCtx) -> ApiResult<Response> {
    let user_id = ctx.require_user_id()?;
    match linked_projects(&state, user_id).await {
        Ok(body) => Ok(json_body(&body)),
        Err(err) => Ok(json_body(&unexpected::<()>(err))),
    }
}

// ── GET /integrations/projects/:projectUuid/tasks ───────────────────────────

/// `GetAllTasksDto` hasil validasi.
#[derive(Debug, Default, PartialEq)]
struct TasksQuery {
    source: Option<String>,
    search: Option<String>,
    status: Option<String>,
    assignee: Option<String>,
    start_date: Option<DateArg>,
    end_date: Option<DateArg>,
    page: i64,
    limit: i64,
}

/// `@IsOptional @IsString` (+ `@IsIn` untuk `source`). Urutan pesan per properti = dekorator
/// dari bawah ke atas (IsIn lalu IsString).
fn opt_string(
    errors: &mut Vec<String>,
    field: &str,
    v: Option<&QVal>,
    one_of: Option<&[&str]>,
) -> Option<String> {
    let s = match v? {
        QVal::One(s) => Some(s.clone()),
        QVal::Many(_) | QVal::Obj => None,
    };
    let before = errors.len();
    if let Some(allowed) = one_of
        && !s.as_deref().is_some_and(|s| allowed.contains(&s))
    {
        errors.push(format!(
            "{field} must be one of the following values: {}",
            allowed.join(", ")
        ));
    }
    if s.is_none() {
        errors.push(format!("{field} must be a string"));
    }
    s.filter(|_| errors.len() == before)
}

/// `@IsOptional @IsDateString`.
fn opt_date(errors: &mut Vec<String>, field: &str, v: Option<&QVal>) -> Option<DateArg> {
    let parsed = match v? {
        QVal::One(s) => parse_date_arg(s),
        QVal::Many(_) | QVal::Obj => None,
    };
    if parsed.is_none() {
        errors.push(format!("{field} must be a valid ISO 8601 date string"));
    }
    parsed
}

/// `@IsOptional @Type(() => Number) @IsInt @Min(1)` (tanpa Max): pesan Min lalu IsInt.
fn opt_int_min1(errors: &mut Vec<String>, field: &str, v: Option<&QVal>, default: i64) -> i64 {
    let Some(v) = v else {
        return default;
    };
    let num = match v {
        QVal::One(s) => Some(js_number(s)),
        QVal::Many(_) | QVal::Obj => None,
    };
    let before = errors.len();
    if num.is_none_or(|n| n.is_nan() || n < 1.0) {
        errors.push(format!("{field} must not be less than 1"));
    }
    let int = num.filter(|n| n.is_finite() && n.fract() == 0.0);
    if int.is_none() {
        errors.push(format!("{field} must be an integer number"));
    }
    #[allow(clippy::cast_possible_truncation)]
    int.filter(|_| errors.len() == before)
        .map_or(default, |n| n as i64)
}

fn parse_tasks_query(raw: Option<&str>) -> Result<TasksQuery, Vec<String>> {
    let map = mz_core::qs::parse(raw.unwrap_or_default());
    let get = |k: &str| mz_core::qs::get(&map, k);
    let mut errors = Vec::new();
    let source = opt_string(
        &mut errors,
        "source",
        get("source"),
        Some(&["clickup", "jira"]),
    );
    let search = opt_string(&mut errors, "search", get("search"), None);
    let status = opt_string(&mut errors, "status", get("status"), None);
    let assignee = opt_string(&mut errors, "assignee", get("assignee"), None);
    let start_date = opt_date(&mut errors, "startDate", get("startDate"));
    let end_date = opt_date(&mut errors, "endDate", get("endDate"));
    let page = opt_int_min1(&mut errors, "page", get("page"), 1);
    let limit = match get("limit") {
        None => 20,
        Some(v) => {
            let num = match v {
                QVal::One(s) => Some(js_number(s)),
                QVal::Many(_) | QVal::Obj => None,
            };
            check_query_int(&mut errors, "limit", num, 1, 100).unwrap_or(20)
        }
    };
    if errors.is_empty() {
        Ok(TasksQuery {
            source,
            search,
            status,
            assignee,
            start_date,
            end_date,
            page,
            limit,
        })
    } else {
        Err(errors)
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TaskRow {
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

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TasksPage {
    project_name: String,
    total: i64,
    page: i64,
    limit: i64,
    total_pages: i64,
    tasks: Vec<TaskRow>,
}

/// `new Date(x)` Invalid Date → Prisma melempar (pesan terakhirnya ditiru).
const INVALID_DATE: &str = "Provided Date object is invalid. Expected Date.";

fn push_where(
    qb: &mut QueryBuilder<'_, Postgres>,
    project_id: i32,
    company_id: i32,
    q: &TasksQuery,
) {
    qb.push(r#" WHERE "projectId" = "#)
        .push_bind(project_id)
        .push(r#" AND "companyId" = "#)
        .push_bind(company_id);
    match &q.source {
        Some(s) => {
            qb.push(" AND source = ").push_bind(s.clone());
        }
        None => {
            qb.push(" AND source IN ('clickup', 'jira')");
        }
    }
    // `search ? {...} : {}`: string kosong = tanpa filter. Wildcard user TIDAK di-escape
    // (sama dengan Prisma `contains` + `mode: insensitive`).
    if let Some(s) = q.search.as_deref().filter(|s| !s.is_empty()) {
        qb.push(" AND name ILIKE ").push_bind(format!("%{s}%"));
    }
    if let Some(s) = q.status.as_deref().filter(|s| !s.is_empty()) {
        qb.push(" AND status ILIKE ").push_bind(s.to_owned());
    }
    if let Some(s) = q.assignee.as_deref().filter(|s| !s.is_empty()) {
        mz_int_common::assignee::push_assignee_filter(qb, s);
    }
    if let Some(DateArg::Ok(d)) = q.start_date {
        qb.push(r#" AND "dueDate" >= "#).push_bind(d);
    }
    if let Some(DateArg::Ok(d)) = q.end_date {
        qb.push(r#" AND "dueDate" <= "#).push_bind(d);
    }
}

async fn all_tasks(
    state: &AppState,
    user_id: i32,
    project_uuid: &str,
    q: &TasksQuery,
) -> sqlx::Result<Envelope<TasksPage>> {
    let Some(company_id) = mz_core::company::find_company_id(&state.db, user_id).await? else {
        return Ok(Envelope::fail("Company not found"));
    };
    // Prisma memvalidasi `@db.Uuid` sebelum query (P2023); Node menambahkan prefiks
    // "Invalid `prisma...` invocation" yang tidak ditiru.
    let uuid = match mz_core::prisma::parse_uuid(project_uuid) {
        Ok(u) => u,
        Err(msg) => return Ok(Envelope::fail(msg)),
    };
    let project: Option<(i32, String)> = sqlx::query_as(
        r#"SELECT p.id, p.name FROM project p
            WHERE p.uuid = $1 AND p.status = 1
              AND EXISTS (SELECT 1 FROM "projectCompany" pc
                           WHERE pc."projectId" = p.id AND pc."companyId" = $2 AND pc.status = 1)
            LIMIT 1"#,
    )
    .bind(uuid)
    .bind(company_id)
    .fetch_optional(&state.db)
    .await?;
    let Some((project_id, project_name)) = project else {
        return Ok(Envelope::fail("Project not found"));
    };
    if matches!(q.start_date, Some(DateArg::Unreadable))
        || matches!(q.end_date, Some(DateArg::Unreadable))
    {
        return Ok(Envelope::fail(format!(
            "Invalid value for argument `{}`: {INVALID_DATE}",
            if matches!(q.start_date, Some(DateArg::Unreadable)) {
                "gte"
            } else {
                "lte"
            }
        )));
    }

    let mut count = QueryBuilder::<Postgres>::new("SELECT COUNT(*) FROM task");
    push_where(&mut count, project_id, company_id, q);
    let total: i64 = count.build_query_scalar().fetch_one(&state.db).await?;

    let mut list = QueryBuilder::<Postgres>::new(
        r#"SELECT uuid::text, source, "externalId", "externalUrl", "externalMeta"::text,
                  name, description, status, priority, "priorityLabel", "dueDate", "startDate",
                  assignees::text, tags::text, "lastSyncedAt"
             FROM task"#,
    );
    push_where(&mut list, project_id, company_id, q);
    let skip = (q.page.saturating_sub(1)).saturating_mul(q.limit);
    list.push(r#" ORDER BY source ASC, "createdAt" ASC LIMIT "#)
        .push_bind(q.limit)
        .push(" OFFSET ")
        .push_bind(skip);
    type Row = (
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        String,
        Option<String>,
        Option<String>,
        Option<i32>,
        Option<String>,
        Option<chrono::NaiveDateTime>,
        Option<chrono::NaiveDateTime>,
        Option<String>,
        Option<String>,
        chrono::NaiveDateTime,
    );
    let rows: Vec<Row> = list.build_query_as().fetch_all(&state.db).await?;
    let tasks = rows
        .into_iter()
        .map(|r| TaskRow {
            uuid: r.0,
            source: r.1,
            external_id: r.2,
            external_url: r.3,
            external_meta: jsonb::raw_opt(r.4.as_deref()),
            name: r.5,
            description: r.6,
            status: r.7,
            priority: r.8,
            priority_label: r.9,
            due_date: r.10.map(JsDate),
            start_date: r.11.map(JsDate),
            assignees: jsonb::raw_opt(r.12.as_deref()),
            tags: jsonb::raw_opt(r.13.as_deref()),
            last_synced_at: JsDate(r.14),
        })
        .collect();
    // `Math.ceil(total / limit)`.
    let total_pages = (total + q.limit - 1) / q.limit;
    Ok(Envelope::ok(
        "Success",
        TasksPage {
            project_name,
            total,
            page: q.page,
            limit: q.limit,
            total_pages,
            tasks,
        },
    ))
}

/// Node: `src/integrations/integrations.controller.ts:54` → `getAllTasks`.
async fn h_tasks(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(project_uuid): Path<String>,
    RawQuery(raw): RawQuery,
) -> ApiResult<Response> {
    let user_id = ctx.require_user_id()?;
    let q = parse_tasks_query(raw.as_deref()).map_err(mz_core::ApiError::Validation)?;
    match all_tasks(&state, user_id, &project_uuid, &q).await {
        Ok(body) => Ok(json_body(&body)),
        Err(err) => Ok(json_body(&unexpected::<()>(err))),
    }
}

// ── POST /integrations/projects/:projectUuid/sync-tasks ─────────────────────

/// `SyncAllTasksDto`: `@IsOptional @IsBoolean` (tanpa `@Type`, jadi string "true" ditolak).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SyncAllTasksDto {
    include_closed: Option<Value>,
    include_subtasks: Option<Value>,
}

impl Validate for SyncAllTasksDto {
    fn validate(&self) -> Vec<String> {
        let mut errors = Vec::new();
        for (field, v) in [
            ("includeClosed", &self.include_closed),
            ("includeSubtasks", &self.include_subtasks),
        ] {
            if v.as_ref().is_some_and(|v| !v.is_boolean()) {
                errors.push(format!("{field} must be a boolean value"));
            }
        }
        errors
    }
}

#[derive(Serialize)]
struct Synced<T> {
    synced: bool,
    #[serde(flatten)]
    data: Option<T>,
}

#[derive(Serialize)]
struct SyncFailed {
    synced: bool,
    error: String,
}

#[derive(Serialize)]
#[serde(untagged)]
enum SyncPart<T> {
    Ok(Synced<T>),
    Failed(SyncFailed),
}

#[derive(Serialize)]
struct SyncAll<C, J> {
    clickup: Option<SyncPart<C>>,
    jira: Option<SyncPart<J>>,
}

const CLICKUP_NOT_LINKED: &str = "Project not found or not linked to a ClickUp folder";
const JIRA_NOT_LINKED: &str = "Project not found or not linked to a Jira project";

fn part<T>(env: Envelope<T>, not_linked: &str) -> Option<SyncPart<T>> {
    if !env.success && env.message == not_linked {
        return None;
    }
    Some(if env.success {
        SyncPart::Ok(Synced {
            synced: true,
            data: env.data,
        })
    } else {
        SyncPart::Failed(SyncFailed {
            synced: false,
            error: env.message,
        })
    })
}

/// Node: `src/integrations/integrations.controller.ts:87` → `syncAllTasks`.
async fn h_sync(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    Path(project_uuid): Path<String>,
    ValidatedJson(dto): ValidatedJson<SyncAllTasksDto>,
) -> ApiResult<Response> {
    let user_id = ctx.require_user_id()?;
    let include_closed = dto
        .include_closed
        .as_ref()
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let include_subtasks = dto
        .include_subtasks
        .as_ref()
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let (clickup, jira) = tokio::join!(
        mz_int_clickup::sync_tasks(
            &state,
            user_id,
            &project_uuid,
            include_closed,
            include_subtasks
        ),
        // Refresh token Jira menulis `company` (ber-audit): pakai handle request.
        mz_int_jira::sync_tasks_with(&state, &audit, user_id, &project_uuid, include_closed),
    );
    let clickup = part(clickup, CLICKUP_NOT_LINKED);
    let jira = part(jira, JIRA_NOT_LINKED);
    if clickup.is_none() && jira.is_none() {
        return Ok(json_body(&Envelope::<()>::fail(
            "Project is not linked to any integration",
        )));
    }
    Ok(json_body(&Envelope::ok(
        "Sync complete",
        SyncAll { clickup, jira },
    )))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn tasks_query_matches_nest_pipe() {
        let q = parse_tasks_query(None).unwrap();
        assert_eq!((q.page, q.limit, q.source), (1, 20, None));
        let q = parse_tasks_query(Some(
            "source=jira&search=&page=2&limit=100&startDate=2026-04-01",
        ))
        .unwrap();
        assert_eq!((q.page, q.limit), (2, 100));
        assert_eq!(q.search.as_deref(), Some(""));
        assert!(matches!(q.start_date, Some(DateArg::Ok(_))));
        assert_eq!(
            parse_tasks_query(Some(
                "source=x&search=a&search=b&startDate=nope&page=0&limit=abc"
            ))
            .unwrap_err(),
            vec![
                "source must be one of the following values: clickup, jira",
                "search must be a string",
                "startDate must be a valid ISO 8601 date string",
                "page must not be less than 1",
                "limit must not be greater than 100",
                "limit must not be less than 1",
                "limit must be an integer number",
            ]
        );
        assert_eq!(
            parse_tasks_query(Some("page=1.5&limit=")).unwrap_err(),
            vec![
                "page must be an integer number",
                "limit must not be less than 1",
            ]
        );
        // Lolos isISO8601 tapi `new Date` Invalid Date.
        let q = parse_tasks_query(Some("endDate=20260401")).unwrap();
        assert_eq!(q.end_date, Some(DateArg::Unreadable));
    }

    #[test]
    fn sync_dto_rejects_non_boolean() {
        let dto: SyncAllTasksDto = serde_json::from_value(
            serde_json::json!({ "includeClosed": "true", "includeSubtasks": null }),
        )
        .unwrap();
        assert_eq!(
            dto.validate(),
            vec!["includeClosed must be a boolean value"]
        );
    }
}
