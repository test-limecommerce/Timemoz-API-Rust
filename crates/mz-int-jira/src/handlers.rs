//! Handler HTTP `JiraController` (`src/integrations/jira/jira.controller.ts`) + DTO.

use axum::Json;
use axum::extract::{RawQuery, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::Response;
use mz_audit::AuditHandle;
use mz_core::extract::{PathParams, Validate, ValidatedJson};
use mz_core::gate::RequestCtx;
use mz_core::iso8601::is_iso8601;
use mz_core::{ApiError, ApiResult, AppState};
use mz_int_common::envelope::Envelope;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::service::{self, LinkProjectInput, LinkUserInput, TaskFilter};
use crate::sync::{SyncCounts, handle_webhook_event};
use crate::util::{
    encode_uri_component, express_redirect, js_parse_int, js_string, query_one, query_pairs, truthy,
};

// ── DTO ──────────────────────────────────────────────────────────────────────

fn must_be_string(errs: &mut Vec<String>, field: &str, v: Option<&Value>) {
    if !matches!(v, Some(Value::String(_))) {
        errs.push(format!("{field} must be a string"));
    }
}

fn optional_string(errs: &mut Vec<String>, field: &str, v: Option<&Value>) {
    if !matches!(v, None | Some(Value::Null | Value::String(_))) {
        errs.push(format!("{field} must be a string"));
    }
}

fn as_string(v: Option<&Value>) -> String {
    v.and_then(Value::as_str).unwrap_or_default().to_owned()
}

fn as_opt_string(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str).map(str::to_owned)
}

/// `LinkProjectDto`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LinkProjectDto {
    #[serde(default)]
    project_uuid: Option<Value>,
    #[serde(default)]
    jira_project_id: Option<Value>,
    #[serde(default)]
    jira_project_key: Option<Value>,
    #[serde(default)]
    jira_project_name: Option<Value>,
}

impl Validate for LinkProjectDto {
    fn validate(&self) -> Vec<String> {
        let mut e = Vec::new();
        must_be_string(&mut e, "projectUuid", self.project_uuid.as_ref());
        must_be_string(&mut e, "jiraProjectId", self.jira_project_id.as_ref());
        must_be_string(&mut e, "jiraProjectKey", self.jira_project_key.as_ref());
        must_be_string(&mut e, "jiraProjectName", self.jira_project_name.as_ref());
        e
    }
}

/// `LinkUserDto`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LinkUserDto {
    #[serde(default)]
    user_uuid: Option<Value>,
    #[serde(default)]
    jira_account_id: Option<Value>,
    #[serde(default)]
    jira_display_name: Option<Value>,
    #[serde(default)]
    jira_email: Option<Value>,
    #[serde(default)]
    jira_avatar_url: Option<Value>,
}

impl Validate for LinkUserDto {
    fn validate(&self) -> Vec<String> {
        let mut e = Vec::new();
        must_be_string(&mut e, "userUuid", self.user_uuid.as_ref());
        must_be_string(&mut e, "jiraAccountId", self.jira_account_id.as_ref());
        optional_string(&mut e, "jiraDisplayName", self.jira_display_name.as_ref());
        optional_string(&mut e, "jiraEmail", self.jira_email.as_ref());
        optional_string(&mut e, "jiraAvatarUrl", self.jira_avatar_url.as_ref());
        e
    }
}

/// `SyncTasksDto`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SyncTasksDto {
    #[serde(default)]
    include_closed: Option<Value>,
}

impl Validate for SyncTasksDto {
    fn validate(&self) -> Vec<String> {
        match &self.include_closed {
            None | Some(Value::Null | Value::Bool(_)) => Vec::new(),
            Some(_) => vec!["includeClosed must be a boolean value".to_owned()],
        }
    }
}

/// `ChangeTaskStatusDto`.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct ChangeTaskStatusDto {
    #[serde(default)]
    status: Option<Value>,
}

impl Validate for ChangeTaskStatusDto {
    fn validate(&self) -> Vec<String> {
        let mut e = Vec::new();
        must_be_string(&mut e, "status", self.status.as_ref());
        e
    }
}

/// Body `any` webhook (tanpa validasi).
#[derive(Debug, Default, Deserialize)]
#[serde(transparent)]
pub(crate) struct AnyBody(Value);

impl Validate for AnyBody {}

use mz_core::jsfmt::number as js_number;

/// `@IsInt() @Min(min) [@Max(max)]` — pesan dari dekorator paling bawah ke atas.
fn validate_int(errs: &mut Vec<String>, field: &str, v: f64, max: Option<f64>) {
    if let Some(max) = max
        && v.partial_cmp(&max).is_none_or(|o| o.is_gt())
    {
        errs.push(format!("{field} must not be greater than {max}"));
    }
    if v.partial_cmp(&1.0).is_none_or(|o| o.is_lt()) {
        errs.push(format!("{field} must not be less than 1"));
    }
    if !(v.is_finite() && v.fract() == 0.0) {
        errs.push(format!("{field} must be an integer number"));
    }
}

/// `GetProjectTasksDto` dari query mentah (`ValidationPipe({ transform: true })`).
pub(crate) fn parse_task_query(raw: Option<&str>) -> Result<TaskFilter, ApiError> {
    let pairs = query_pairs(raw);
    let mut errs = Vec::new();
    let string_field = |name: &str, errs: &mut Vec<String>| match query_one(&pairs, name) {
        None => None,
        Some(Ok(v)) => Some(v),
        Some(Err(())) => {
            errs.push(format!("{name} must be a string"));
            None
        }
    };
    let search = string_field("search", &mut errs);
    let status = string_field("status", &mut errs);
    let assignee = string_field("assignee", &mut errs);
    let date_field = |name: &str, errs: &mut Vec<String>| match query_one(&pairs, name) {
        None => None,
        Some(Ok(v)) if is_iso8601(&v) => Some(v),
        Some(_) => {
            errs.push(format!("{name} must be a valid ISO 8601 date string"));
            None
        }
    };
    let start_date = date_field("startDate", &mut errs);
    let end_date = date_field("endDate", &mut errs);
    let num = |name: &str| -> Option<f64> {
        match query_one(&pairs, name)? {
            Ok(one) => Some(js_number(&one)),
            Err(()) => Some(f64::NAN),
        }
    };
    let page = num("page");
    let limit = num("limit");
    if let Some(p) = page {
        validate_int(&mut errs, "page", p, None);
    }
    if let Some(l) = limit {
        validate_int(&mut errs, "limit", l, Some(100.0));
    }
    if !errs.is_empty() {
        return Err(ApiError::Validation(errs));
    }
    #[allow(clippy::cast_possible_truncation)]
    let to_i = |v: Option<f64>, d: i64| v.map_or(d, |f| f as i64);
    Ok(TaskFilter {
        search,
        status,
        assignee,
        start_date,
        end_date,
        page: to_i(page, 1),
        limit: to_i(limit, 20),
    })
}

// ── handler ──────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub(crate) struct WebhookAck {
    ok: bool,
}

/// Node: src/integrations/jira/jira.controller.ts:41 (`handleWebhook`).
pub(crate) async fn webhook(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    ValidatedJson(AnyBody(payload)): ValidatedJson<AnyBody>,
) -> Json<WebhookAck> {
    let pairs = query_pairs(query.as_deref());
    // `!secret` lalu dibandingkan dengan rahasia company: array/objek tak pernah cocok.
    let secret = mz_core::qs::get(&pairs, "s")
        .map(mz_core::qs::QVal::js_string)
        .unwrap_or_default();
    if secret.is_empty() {
        return Json(WebhookAck { ok: false });
    }
    if let Err(err) = handle_webhook_event(&state, &secret, &payload).await {
        tracing::error!("Jira webhook error: {err}");
    }
    Json(WebhookAck { ok: true })
}

/// Node: src/integrations/jira/jira.controller.ts:55 (`getConnectionStatus`).
pub(crate) async fn status(
    State(state): State<AppState>,
    ctx: RequestCtx,
) -> ApiResult<Json<Envelope<service::JiraStatus>>> {
    Ok(Json(
        service::get_connection_status(&state, ctx.require_user_id()?).await,
    ))
}

/// `PermissionInterceptor`/`SubscriptionLockInterceptor` untuk `req.user` yang
/// dipasang `JiraConnectGuard` (gerbang umum tidak tahu guard khusus ini).
async fn connect_interceptors(state: &AppState, user_id: i32) -> ApiResult<()> {
    let lock = mz_core::subscription_lock::get_state(state, user_id).await?;
    if lock.locked {
        return Err(mz_core::subscription_lock::locked_error(&lock));
    }
    let company_id = mz_core::company::find_company_id(&state.db, user_id).await?;
    let access = match company_id {
        Some(c) => mz_core::permissions::effective_access(state, user_id, c).await?,
        None => None,
    };
    let is_custom = match (&access, company_id) {
        (Some(a), _) => !a.is_system,
        (None, Some(_)) => false,
        (None, None) => mz_core::permissions::holds_custom_role(&state.db, user_id).await?,
    };
    mz_core::permissions::decide(
        Some("integrations"),
        mz_core::enums::action_for_method(&Method::GET),
        company_id,
        access.as_deref(),
        is_custom,
    )
}

/// Node: src/integrations/jira/jira.controller.ts:63 (`initiateOAuth`, `JiraConnectGuard`).
pub(crate) async fn connect(
    State(state): State<AppState>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> ApiResult<Response> {
    // JiraConnectGuard: `req.query.token` → `jwtService.verify` → `req.user = { id: payload.userId }`.
    let pairs = query_pairs(query.as_deref());
    let token = match query_one(&pairs, "token") {
        Some(Ok(t)) if !t.is_empty() => t,
        _ => return Err(ApiError::Bare(StatusCode::UNAUTHORIZED)),
    };
    let payload = mz_int_common::nest_jwt::verify(&state, &token)
        .ok_or(ApiError::Bare(StatusCode::UNAUTHORIZED))?;
    let user = payload.get("userId");
    if truthy(user)
        && let Some(uid) = user
            .and_then(Value::as_i64)
            .and_then(|n| i32::try_from(n).ok())
    {
        connect_interceptors(&state, uid).await?;
    }
    let user_str = user.map_or_else(|| "undefined".to_owned(), js_string);
    let scope = "read:me read:jira-work write:jira-work offline_access read:account read:jira-user";
    let client_id = state
        .config
        .var("JIRA_CLIENT_ID")
        .unwrap_or_else(|| "undefined".to_owned());
    let redirect_uri = state
        .config
        .var("JIRA_REDIRECT_URI")
        .unwrap_or_else(|| "undefined".to_owned());
    let url = format!(
        "https://auth.atlassian.com/authorize?response_type=code&client_id={client_id}&redirect_uri={}&scope={}&audience=api.atlassian.com&state={user_str}",
        encode_uri_component(&redirect_uri),
        encode_uri_component(scope),
    );
    Ok(express_redirect(&headers, &url))
}

/// Node: src/integrations/jira/jira.controller.ts:80 (`handleCallback`).
pub(crate) async fn callback(
    State(state): State<AppState>,
    audit: AuditHandle,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    let pairs = query_pairs(query.as_deref());
    let q = |k: &str| mz_core::qs::get(&pairs, k);
    // `parseInt(state)` = `parseInt(String(state))`.
    let user_id = q("state")
        .map(mz_core::qs::QVal::js_string)
        .as_deref()
        .and_then(js_parse_int);
    let code = q("code").and_then(|v| match v {
        mz_core::qs::QVal::One(s) => Some(s.clone()),
        mz_core::qs::QVal::Many(list) => list.first().cloned(),
        mz_core::qs::QVal::Obj => None,
    });
    let result = service::handle_oauth_callback(&state, &audit, user_id, code).await;
    let frontend = state
        .config
        .var("FRONTEND_URL")
        .unwrap_or_else(|| "http://localhost:3001".to_owned());
    let url = if result.success {
        format!("{frontend}/integrations/jira?status=connected")
    } else {
        let msg = if result.message.is_empty() {
            // `result.message ?? 'OAuth failed'` — hanya `undefined`/`null` yang diganti.
            String::new()
        } else {
            encode_uri_component(&result.message)
        };
        format!("{frontend}/integrations/jira?status=error&message={msg}")
    };
    express_redirect(&headers, &url)
}

/// Node: src/integrations/jira/jira.controller.ts:95 (`disconnect`).
pub(crate) async fn disconnect(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
) -> ApiResult<Json<Envelope<()>>> {
    Ok(Json(
        service::disconnect(&state, &audit, ctx.require_user_id()?).await,
    ))
}

/// Node: src/integrations/jira/jira.controller.ts:104 (`getProjects`).
pub(crate) async fn projects(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
) -> ApiResult<Json<Envelope<Vec<service::JiraProjectItem>>>> {
    Ok(Json(
        service::get_projects(&state, &audit, ctx.require_user_id()?).await,
    ))
}

/// Node: src/integrations/jira/jira.controller.ts:113 (`getLinkedProjects`).
pub(crate) async fn linked_projects(
    State(state): State<AppState>,
    ctx: RequestCtx,
) -> ApiResult<Json<Envelope<Vec<service::LinkedProjectRow>>>> {
    Ok(Json(
        service::get_linked_projects(&state, ctx.require_user_id()?).await,
    ))
}

/// Node: src/integrations/jira/jira.controller.ts:122 (`linkProject`).
pub(crate) async fn link_project(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedJson(dto): ValidatedJson<LinkProjectDto>,
) -> ApiResult<Json<Envelope<service::LinkProjectData>>> {
    let input = LinkProjectInput {
        project_uuid: as_string(dto.project_uuid.as_ref()),
        jira_project_id: as_string(dto.jira_project_id.as_ref()),
        jira_project_key: as_string(dto.jira_project_key.as_ref()),
        jira_project_name: as_string(dto.jira_project_name.as_ref()),
    };
    Ok(Json(
        service::link_project(&state, ctx.require_user_id()?, input).await,
    ))
}

/// Node: src/integrations/jira/jira.controller.ts:131 (`unlinkProject`).
pub(crate) async fn unlink_project(
    State(state): State<AppState>,
    ctx: RequestCtx,
    PathParams(project_uuid): PathParams<String>,
) -> ApiResult<Json<Envelope<()>>> {
    Ok(Json(
        service::unlink_project(&state, ctx.require_user_id()?, &project_uuid).await,
    ))
}

/// Node: src/integrations/jira/jira.controller.ts:141 (`syncTasks`).
pub(crate) async fn sync_tasks(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    PathParams(project_uuid): PathParams<String>,
    ValidatedJson(dto): ValidatedJson<SyncTasksDto>,
) -> ApiResult<Json<Envelope<SyncCounts>>> {
    let include_closed = dto
        .include_closed
        .as_ref()
        .and_then(Value::as_bool)
        .unwrap_or(true);
    Ok(Json(
        service::sync_tasks_with(
            &state,
            &audit,
            ctx.require_user_id()?,
            &project_uuid,
            include_closed,
        )
        .await,
    ))
}

/// Node: src/integrations/jira/jira.controller.ts:155 (`getProjectTasks`).
pub(crate) async fn project_tasks(
    State(state): State<AppState>,
    ctx: RequestCtx,
    PathParams(project_uuid): PathParams<String>,
    RawQuery(query): RawQuery,
) -> ApiResult<Json<Envelope<service::TasksPage>>> {
    let filter = parse_task_query(query.as_deref())?;
    Ok(Json(
        service::get_project_tasks(&state, ctx.require_user_id()?, &project_uuid, filter).await,
    ))
}

/// Node: src/integrations/jira/jira.controller.ts:186 (`getTaskStatuses`).
pub(crate) async fn task_statuses(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    PathParams(task_uuid): PathParams<String>,
) -> ApiResult<Json<Envelope<Vec<service::TransitionItem>>>> {
    Ok(Json(
        service::get_task_statuses(&state, &audit, ctx.require_user_id()?, &task_uuid).await,
    ))
}

/// Node: src/integrations/jira/jira.controller.ts:196 (`changeTaskStatus`).
pub(crate) async fn change_task_status(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    PathParams(task_uuid): PathParams<String>,
    ValidatedJson(dto): ValidatedJson<ChangeTaskStatusDto>,
) -> ApiResult<Json<Envelope<service::StatusChange>>> {
    let status = as_string(dto.status.as_ref());
    Ok(Json(
        service::change_task_status(&state, &audit, ctx.require_user_id()?, &task_uuid, status)
            .await,
    ))
}

/// Node: src/integrations/jira/jira.controller.ts:210 (`getMembers`).
pub(crate) async fn members(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
) -> ApiResult<Json<Envelope<Vec<service::MemberItem>>>> {
    Ok(Json(
        service::get_members(&state, &audit, ctx.require_user_id()?).await,
    ))
}

/// Node: src/integrations/jira/jira.controller.ts:219 (`linkUser`).
pub(crate) async fn link_user(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedJson(dto): ValidatedJson<LinkUserDto>,
) -> ApiResult<Json<Envelope<service::LinkUserData>>> {
    let input = LinkUserInput {
        user_uuid: as_string(dto.user_uuid.as_ref()),
        jira_account_id: as_string(dto.jira_account_id.as_ref()),
        jira_display_name: as_opt_string(dto.jira_display_name.as_ref()),
        jira_email: as_opt_string(dto.jira_email.as_ref()),
        jira_avatar_url: as_opt_string(dto.jira_avatar_url.as_ref()),
    };
    Ok(Json(
        service::link_user(&state, ctx.require_user_id()?, input).await,
    ))
}

/// Node: src/integrations/jira/jira.controller.ts:228 (`unlinkUser`).
pub(crate) async fn unlink_user(
    State(state): State<AppState>,
    ctx: RequestCtx,
    PathParams(user_uuid): PathParams<String>,
) -> ApiResult<Json<Envelope<()>>> {
    Ok(Json(
        service::unlink_user(&state, ctx.require_user_id()?, &user_uuid).await,
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use serde_json::json;

    fn msgs(e: ApiError) -> Vec<String> {
        match e {
            ApiError::Validation(m) => m,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn task_query_defaults_and_errors() {
        let f = parse_task_query(None).unwrap();
        assert_eq!((f.page, f.limit), (1, 20));
        let f = parse_task_query(Some("page=2&limit=50&search=bug&startDate=2026-04-01")).unwrap();
        assert_eq!((f.page, f.limit), (2, 50));
        assert_eq!(f.search.as_deref(), Some("bug"));
        assert_eq!(
            msgs(parse_task_query(Some("limit=101&page=0&startDate=x")).unwrap_err()),
            vec![
                "startDate must be a valid ISO 8601 date string",
                "page must not be less than 1",
                "limit must not be greater than 100",
            ]
        );
        assert_eq!(
            msgs(parse_task_query(Some("page=abc&search=a&search=b")).unwrap_err()),
            vec![
                "search must be a string",
                "page must not be less than 1",
                "page must be an integer number",
            ]
        );
        assert_eq!(
            msgs(parse_task_query(Some("limit=1.5")).unwrap_err()),
            vec!["limit must be an integer number"]
        );
    }

    #[test]
    fn body_dtos() {
        let d: LinkProjectDto =
            serde_json::from_value(json!({"projectUuid": "x", "jiraProjectId": 1})).unwrap();
        assert_eq!(
            d.validate(),
            vec![
                "jiraProjectId must be a string",
                "jiraProjectKey must be a string",
                "jiraProjectName must be a string",
            ]
        );
        let u: LinkUserDto = serde_json::from_value(
            json!({"userUuid": "x", "jiraAccountId": "a", "jiraEmail": null}),
        )
        .unwrap();
        assert!(u.validate().is_empty());
        let s: SyncTasksDto = serde_json::from_value(json!({"includeClosed": "yes"})).unwrap();
        assert_eq!(s.validate(), vec!["includeClosed must be a boolean value"]);
        let c: ChangeTaskStatusDto = serde_json::from_value(json!({})).unwrap();
        assert_eq!(c.validate(), vec!["status must be a string"]);
    }
}
