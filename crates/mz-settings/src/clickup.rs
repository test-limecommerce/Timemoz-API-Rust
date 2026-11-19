//! ClickUp per company di `SettingsService` (token, workspace, disconnect). `company` adalah model
//! ber-audit: tulisan lewat `AuditHandle`. Panggilan ClickUp lewat `api_base::CLICKUP`.
//! Semua method Node membungkus isinya dalam try/catch → `{ success:false, message, data:null }`.
//! Token tidak pernah dicatat di log.

use std::time::Duration;

use axum::Json;
use axum::extract::State;
use futures::future::try_join_all;
use mz_audit::{AuditHandle, Lookup, Model};
use mz_core::company::find_company_id;
use mz_core::extract::ValidatedJson;
use mz_core::time::now_naive;
use mz_core::{ApiResult, AppState, RequestCtx};
use mz_int_common::api_base;
use mz_int_common::envelope::Envelope;
use reqwest::Method;
use serde::Serialize;
use serde_json::{Value, json};

use crate::dto::{ClickupToken, ClickupWorkspace};

/// axios di Node tidak memasang timeout; batas ini hanya pengaman agar request tidak menggantung.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

const NO_COMPANY: &str = "Company not found";
const INVALID_TOKEN: &str = "Invalid ClickUp API token. Please check your token and try again.";

/// Galat axios: `status` respons (bila ada) + `message`.
struct HttpErr {
    status: Option<u16>,
    message: String,
}

async fn call(
    state: &AppState,
    method: Method,
    token: &str,
    path: &str,
    body: Option<&Value>,
) -> Result<Value, HttpErr> {
    let url = format!("{}{path}", api_base::CLICKUP.get(state));
    let mut req = state
        .http
        .request(method, url)
        .header("Authorization", token)
        .timeout(HTTP_TIMEOUT);
    if let Some(b) = body {
        req = req.json(b);
    }
    let res = req.send().await.map_err(|e| HttpErr {
        status: None,
        message: e.without_url().to_string(),
    })?;
    let status = res.status();
    if !status.is_success() {
        return Err(HttpErr {
            status: Some(status.as_u16()),
            message: format!("Request failed with status code {}", status.as_u16()),
        });
    }
    // axios: body non-JSON jadi string; di sini cukup `null` (hanya `.teams`/`.webhooks` dibaca).
    Ok(res.json::<Value>().await.unwrap_or(Value::Null))
}

/// `UPDATE company SET ...` + audit `company update` (Prisma `company.update`).
async fn update_company(
    state: &AppState,
    audit: &AuditHandle,
    company_id: i32,
    sets: &[(&str, Option<&str>)],
) -> Result<(), sqlx::Error> {
    let pre = audit.pre_read(Model::Company, Lookup::Id(company_id)).await;
    let mut qb = sqlx::QueryBuilder::<sqlx::Postgres>::new("UPDATE company SET ");
    for (col, val) in sets {
        qb.push(format!(r#""{col}" = "#))
            .push_bind(val.map(str::to_owned));
        qb.push(", ");
    }
    qb.push(r#""updatedAt" = "#).push_bind(now_naive());
    qb.push(" WHERE id = ").push_bind(company_id);
    let res = qb.build().execute(&state.db).await?;
    if res.rows_affected() == 0 {
        return Err(sqlx::Error::RowNotFound);
    }
    if let Some(row) = audit
        .result_row(&state.db, Model::Company, Lookup::Id(company_id))
        .await
    {
        audit.record_update(Model::Company, pre, &row).await;
    }
    Ok(())
}

async fn company_creds(
    state: &AppState,
    company_id: i32,
) -> sqlx::Result<(Option<String>, Option<String>)> {
    Ok(sqlx::query_as(
        r#"SELECT "clickupApiToken", "clickupWorkspaceId" FROM company WHERE id = $1"#,
    )
    .bind(company_id)
    .fetch_optional(&state.db)
    .await?
    .unwrap_or((None, None)))
}

/// `${team.id}`.
fn js_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "null".into(),
        Value::Number(n) => n
            .as_f64()
            .map_or_else(|| n.to_string(), mz_core::jsfmt::num_string),
        other => other.to_string(),
    }
}

fn list_of<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v.get(key)
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// Hapus webhook ClickUp milik kita (endpoint == `webhook_url`) di satu workspace.
async fn delete_our_webhooks(
    state: &AppState,
    token: &str,
    team_id: &str,
    webhook_url: &str,
) -> Result<(), HttpErr> {
    let existing = call(
        state,
        Method::GET,
        token,
        &format!("/team/{team_id}/webhook"),
        None,
    )
    .await?;
    let ours = list_of(&existing, "webhooks")
        .iter()
        .filter(|w| w.get("endpoint").and_then(Value::as_str) == Some(webhook_url));
    try_join_all(ours.map(|w| {
        let id = w.get("id").map(js_string).unwrap_or_default();
        async move {
            call(
                state,
                Method::DELETE,
                token,
                &format!("/webhook/{id}"),
                None,
            )
            .await
        }
    }))
    .await?;
    Ok(())
}

/// Node: src/settings/settings.controller.ts:183 (`POST company/clickup-token`, 200)
pub async fn save_token(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    ValidatedJson(body): ValidatedJson<ClickupToken>,
) -> ApiResult<Json<Envelope<()>>> {
    let uid = ctx.require_user_id()?;
    let token = body
        .0
        .get("apiToken")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    Ok(Json(
        match save_token_inner(&state, &audit, uid, &token).await {
            Ok(env) => env,
            Err(e) => Envelope::fail(e.to_string()),
        },
    ))
}

async fn save_token_inner(
    state: &AppState,
    audit: &AuditHandle,
    uid: i32,
    token: &str,
) -> sqlx::Result<Envelope<()>> {
    let Some(company_id) = find_company_id(&state.db, uid).await? else {
        return Ok(Envelope::fail(NO_COMPANY));
    };
    let Ok(teams_res) = call(state, Method::GET, token, "/team", None).await else {
        return Ok(Envelope::fail(INVALID_TOKEN));
    };
    update_company(
        state,
        audit,
        company_id,
        &[("clickupApiToken", Some(token))],
    )
    .await?;

    // Daftarkan webhook per workspace (non-kritis: kegagalan tidak membatalkan simpan token).
    if let Some(webhook_url) = state.config.var("CLICKUP_WEBHOOK_URL") {
        for team in list_of(&teams_res, "teams") {
            let team_id = team.get("id").map(js_string).unwrap_or_default();
            let register = async {
                // Hapus webhook lama ber-URL sama supaya tidak dobel.
                delete_our_webhooks(state, token, &team_id, &webhook_url).await?;
                let body = json!({
                    "endpoint": webhook_url,
                    "events": [
                        "taskCreated", "taskUpdated", "taskDeleted", "taskStatusUpdated",
                        "taskAssigneeUpdated", "taskDueDateUpdated", "taskPriorityUpdated",
                        "taskTagUpdated"
                    ],
                });
                call(
                    state,
                    Method::POST,
                    token,
                    &format!("/team/{team_id}/webhook"),
                    Some(&body),
                )
                .await
                .map(|_| ())
            };
            if let Err(e) = register.await {
                tracing::warn!(team_id, error = %e.message, "settings-clickup-webhook-register gagal");
            }
        }
    }
    Ok(Envelope {
        success: true,
        message: "ClickUp API token saved successfully".into(),
        data: None,
    })
}

#[derive(Serialize)]
pub struct HasToken {
    #[serde(rename = "hasToken")]
    has_token: bool,
}

/// Node: src/settings/settings.controller.ts:194 (`GET company/clickup-token`, 200)
pub async fn get_token(
    State(state): State<AppState>,
    ctx: RequestCtx,
) -> ApiResult<Json<Envelope<HasToken>>> {
    let uid = ctx.require_user_id()?;
    let res: sqlx::Result<Envelope<HasToken>> = async {
        let Some(company_id) = find_company_id(&state.db, uid).await? else {
            return Ok(Envelope::fail(NO_COMPANY));
        };
        let (token, _) = company_creds(&state, company_id).await?;
        Ok(Envelope::ok(
            "Success",
            HasToken {
                has_token: token.is_some_and(|t| !t.is_empty()),
            },
        ))
    }
    .await;
    Ok(Json(res.unwrap_or_else(|e| Envelope::fail(e.to_string()))))
}

/// Node: src/settings/settings.controller.ts:205 (`DELETE company/clickup`, 200)
pub async fn disconnect(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
) -> ApiResult<Json<Envelope<()>>> {
    let uid = ctx.require_user_id()?;
    Ok(Json(match disconnect_inner(&state, &audit, uid).await {
        Ok(env) => env,
        Err(e) => Envelope::fail(e.to_string()),
    }))
}

async fn disconnect_inner(
    state: &AppState,
    audit: &AuditHandle,
    uid: i32,
) -> sqlx::Result<Envelope<()>> {
    let Some(company_id) = find_company_id(&state.db, uid).await? else {
        return Ok(Envelope::fail(NO_COMPANY));
    };
    let (token, workspace) = company_creds(state, company_id).await?;

    // Hapus webhook di ClickUp (non-kritis).
    if let (Some(token), Some(workspace), Some(webhook_url)) = (
        token.filter(|t| !t.is_empty()),
        workspace.filter(|w| !w.is_empty()),
        state.config.var("CLICKUP_WEBHOOK_URL"),
    ) && let Err(e) = delete_our_webhooks(state, &token, &workspace, &webhook_url).await
    {
        tracing::warn!(error = %e.message, "settings-clickup-webhook-delete gagal");
    }

    // Hapus semua data hasil sync company ini (Node: Promise.all, bukan transaksi).
    sqlx::query(r#"DELETE FROM task WHERE "companyId" = $1 AND source = 'clickup'"#)
        .bind(company_id)
        .execute(&state.db)
        .await?;
    sqlx::query(r#"DELETE FROM "projectClickupMapping" WHERE "companyId" = $1"#)
        .bind(company_id)
        .execute(&state.db)
        .await?;
    sqlx::query(r#"DELETE FROM "userClickupMapping" WHERE "companyId" = $1"#)
        .bind(company_id)
        .execute(&state.db)
        .await?;

    update_company(
        state,
        audit,
        company_id,
        &[("clickupApiToken", None), ("clickupWorkspaceId", None)],
    )
    .await?;
    Ok(Envelope {
        success: true,
        message: "ClickUp disconnected and all synced data removed".into(),
        data: None,
    })
}

// `id`/`name`/`color`/`avatar` yang `undefined` di JS tidak muncul di JSON; `null` tetap.
#[derive(Serialize)]
pub struct WorkspaceOut {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    color: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    avatar: Option<Value>,
    #[serde(rename = "isSelected")]
    is_selected: bool,
}

/// Node: src/settings/settings.controller.ts:219 (`GET company/clickup-workspaces`, 200)
pub async fn workspaces(
    State(state): State<AppState>,
    ctx: RequestCtx,
) -> ApiResult<Json<Envelope<Vec<WorkspaceOut>>>> {
    let uid = ctx.require_user_id()?;
    let out = match workspaces_inner(&state, uid).await {
        Ok(env) => env,
        Err(msg) => Envelope::fail(msg),
    };
    Ok(Json(out))
}

async fn workspaces_inner(
    state: &AppState,
    uid: i32,
) -> Result<Envelope<Vec<WorkspaceOut>>, String> {
    let Some(company_id) = find_company_id(&state.db, uid)
        .await
        .map_err(|e| e.to_string())?
    else {
        return Ok(Envelope::fail(NO_COMPANY));
    };
    let (token, selected) = company_creds(state, company_id)
        .await
        .map_err(|e| e.to_string())?;
    let Some(token) = token.filter(|t| !t.is_empty()) else {
        return Ok(Envelope::fail(
            "ClickUp API token not configured. Please save your token first.",
        ));
    };
    let res = match call(state, Method::GET, &token, "/team", None).await {
        Ok(v) => v,
        Err(e) if e.status == Some(401) => {
            return Ok(Envelope::fail(
                "Invalid ClickUp API token. Please update your token.",
            ));
        }
        Err(e) => return Err(e.message),
    };
    let data = list_of(&res, "teams")
        .iter()
        .map(|team| {
            let is_selected = matches!(
                (team.get("id"), selected.as_deref()),
                (Some(Value::String(id)), Some(sel)) if id == sel
            );
            // Struct (bukan `Value`) supaya urutan kunci = literal Node.
            WorkspaceOut {
                id: team.get("id").cloned(),
                name: team.get("name").cloned(),
                color: team.get("color").cloned(),
                avatar: team.get("avatar").cloned(),
                is_selected,
            }
        })
        .collect();
    Ok(Envelope::ok("Success", data))
}

/// Node: src/settings/settings.controller.ts:230 (`POST company/clickup-workspace`, 200)
pub async fn save_workspace(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    ValidatedJson(body): ValidatedJson<ClickupWorkspace>,
) -> ApiResult<Json<Envelope<()>>> {
    let uid = ctx.require_user_id()?;
    let workspace = body
        .0
        .get("workspaceId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let res: sqlx::Result<Envelope<()>> = async {
        let Some(company_id) = find_company_id(&state.db, uid).await? else {
            return Ok(Envelope::fail(NO_COMPANY));
        };
        update_company(
            &state,
            &audit,
            company_id,
            &[("clickupWorkspaceId", Some(&workspace))],
        )
        .await?;
        Ok(Envelope {
            success: true,
            message: "ClickUp workspace saved successfully".into(),
            data: None,
        })
    }
    .await;
    Ok(Json(res.unwrap_or_else(|e| Envelope::fail(e.to_string()))))
}
