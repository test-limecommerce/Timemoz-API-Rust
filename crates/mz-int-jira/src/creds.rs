//! Kredensial Jira per company: `getCompanyCredentials`, `refreshAccessToken`,
//! `getValidAccessToken`, `registerWebhook`, `deleteWebhookFromJira`.

use chrono::NaiveDateTime;
use mz_audit::{AuditHandle, Lookup, Model};
use mz_core::AppState;
use mz_core::time::now_naive;
use mz_int_common::company::admin_company_id;
use reqwest::Method;
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::{Postgres, QueryBuilder};

use crate::http::{self, Error, Res};
use crate::util::{path, truthy};

/// Kolom `company` yang ditulis modul ini.
#[derive(Debug, Clone)]
pub(crate) enum Set {
    Text(&'static str, Option<String>),
    Ts(&'static str, Option<NaiveDateTime>),
    Int(&'static str, Option<i32>),
}

/// `prisma.company.update({ where: { id }, data: { ...sets, updatedAt: new Date() } })`
/// tanpa `select`, lewat audit (model `company` ber-audit).
pub(crate) async fn update_company(
    state: &AppState,
    audit: &AuditHandle,
    company_id: i32,
    sets: &[Set],
) -> Res<()> {
    let pre = audit.pre_read(Model::Company, Lookup::Id(company_id)).await;
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new("UPDATE company SET ");
    for s in sets {
        match s {
            Set::Text(col, v) => {
                qb.push(format!("\"{col}\" = ")).push_bind(v.clone());
            }
            Set::Ts(col, v) => {
                qb.push(format!("\"{col}\" = ")).push_bind(*v);
            }
            Set::Int(col, v) => {
                qb.push(format!("\"{col}\" = ")).push_bind(*v);
            }
        }
        qb.push(", ");
    }
    qb.push("\"updatedAt\" = ").push_bind(now_naive());
    qb.push(" WHERE id = ").push_bind(company_id);
    let done = qb.build().execute(&state.db).await?;
    if done.rows_affected() == 0 {
        // Prisma `update` tanpa baris = P2025.
        return Err(Error::Msg(
            "An operation failed because it depends on one or more records that were required but not found. Record to update not found.".to_owned(),
        ));
    }
    if let Some(row) = audit
        .result_row(&state.db, Model::Company, Lookup::Id(company_id))
        .await
    {
        audit.record_update(Model::Company, pre, &row).await;
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub(crate) struct Creds {
    pub company_id: i32,
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    pub cloud_id: Option<String>,
    pub site_url: Option<String>,
    pub expires_at: Option<NaiveDateTime>,
}

type CredRow = (
    i32,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<NaiveDateTime>,
);

/// `getCompanyCredentials(userId)`.
pub(crate) async fn company_credentials(state: &AppState, user_id: i32) -> Res<Option<Creds>> {
    let Some(company_id) = admin_company_id(state, user_id).await? else {
        return Ok(None);
    };
    let row: Option<CredRow> = sqlx::query_as(
        r#"SELECT id, "jiraAccessToken", "jiraRefreshToken", "jiraCloudId", "jiraSiteUrl",
                  "jiraTokenExpiresAt"
           FROM company WHERE id = $1 LIMIT 1"#,
    )
    .bind(company_id)
    .fetch_optional(&state.db)
    .await?;
    Ok(row.map(
        |(company_id, access_token, refresh_token, cloud_id, site_url, expires_at)| Creds {
            company_id,
            access_token,
            refresh_token,
            cloud_id,
            site_url,
            expires_at,
        },
    ))
}

/// `!expiresAt || expiresAt <= now + 5 menit`.
pub(crate) fn needs_refresh(expires_at: Option<NaiveDateTime>) -> bool {
    let soon = now_naive() + chrono::Duration::minutes(5);
    expires_at.is_none_or(|e| e <= soon)
}

#[derive(Serialize)]
struct RefreshBody<'a> {
    grant_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_secret: Option<String>,
    refresh_token: &'a str,
}

/// Nilai `access_token`/`refresh_token` dari respons token: `undefined` → kolom tidak
/// ditulis (Prisma), `null` → NULL.
fn token_set(col: &'static str, v: Option<&Value>) -> Res<Option<Set>> {
    match v {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(Set::Text(col, None))),
        Some(Value::String(s)) => Ok(Some(Set::Text(col, Some(s.clone())))),
        Some(_) => Err(Error::Msg(format!(
            "Invalid value for argument `{col}`: Expected String or Null."
        ))),
    }
}

/// `new Date(Date.now() + expires_in * 1000)`.
pub(crate) fn expires_at(v: Option<&Value>) -> Res<NaiveDateTime> {
    let secs = match v {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => s.trim().parse::<f64>().ok(),
        Some(Value::Null) => Some(0.0),
        _ => None,
    }
    .filter(|f| f.is_finite())
    .ok_or_else(|| {
        Error::Msg(
            "Invalid value for argument `jiraTokenExpiresAt`: Provided Date object is invalid. Expected Date.".to_owned(),
        )
    })?;
    #[allow(clippy::cast_possible_truncation)]
    let ms = (secs * 1000.0) as i64;
    Ok(now_naive() + chrono::Duration::milliseconds(ms))
}

/// Ambil field token dari `res.data` sebagai string (`undefined` → `"undefined"` seperti
/// template literal Node saat dipakai sebagai Bearer).
fn token_str(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) => "null".to_owned(),
        Some(other) => other.to_string(),
        None => "undefined".to_owned(),
    }
}

/// `refreshAccessToken(companyId, refreshToken)`.
pub(crate) async fn refresh_access_token(
    state: &AppState,
    audit: &AuditHandle,
    company_id: i32,
    refresh_token: &str,
) -> Res<String> {
    let body = RefreshBody {
        grant_type: "refresh_token",
        client_id: state.config.var("JIRA_CLIENT_ID"),
        client_secret: state.config.var("JIRA_CLIENT_SECRET"),
        refresh_token,
    };
    let data = http::send(state.http.post(http::token_url(state)).json(&body)).await?;
    let access = data.get("access_token");
    let mut sets: Vec<Set> = Vec::new();
    sets.extend(token_set("jiraAccessToken", access)?);
    sets.extend(token_set("jiraRefreshToken", data.get("refresh_token"))?);
    sets.push(Set::Ts(
        "jiraTokenExpiresAt",
        Some(expires_at(data.get("expires_in"))?),
    ));
    update_company(state, audit, company_id, &sets).await?;
    Ok(token_str(access))
}

#[derive(Debug, Clone)]
pub(crate) struct Token {
    pub access_token: String,
    pub cloud_id: String,
    pub site_url: Option<String>,
    pub company_id: i32,
}

/// `getValidAccessToken(userId)`.
pub(crate) async fn valid_access_token(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
) -> Res<Option<Token>> {
    let Some(c) = company_credentials(state, user_id).await? else {
        return Ok(None);
    };
    let (Some(access), Some(cloud_id)) = (
        c.access_token.filter(|s| !s.is_empty()),
        c.cloud_id.filter(|s| !s.is_empty()),
    ) else {
        return Ok(None);
    };
    let mut access_token = access;
    if needs_refresh(c.expires_at) {
        let Some(refresh) = c.refresh_token.filter(|s| !s.is_empty()) else {
            return Ok(None);
        };
        access_token = refresh_access_token(state, audit, c.company_id, &refresh).await?;
    }
    Ok(Some(Token {
        access_token,
        cloud_id,
        site_url: c.site_url,
        company_id: c.company_id,
    }))
}

/// `registerWebhook(companyId, accessToken, cloudId)`.
pub(crate) async fn register_webhook(
    state: &AppState,
    audit: &AuditHandle,
    company_id: i32,
    access_token: &str,
    cloud_id: &str,
) -> Res<()> {
    let secret = hex::encode(rand::random::<[u8; 32]>());
    let Some(api_base) = state.config.var("HOST_BE") else {
        tracing::warn!("HOST_BE not set, skipping webhook registration");
        return Ok(());
    };
    let webhook_url = format!("{api_base}/integrations/jira/webhook?s={secret}");
    let data = http::send(
        http::bearer(
            state,
            Method::POST,
            &http::jira_url(state, cloud_id, "/webhook"),
            access_token,
        )
        .json(&json!({
            "url": webhook_url,
            "webhooks": [{
                "events": ["jira:issue_created", "jira:issue_updated", "jira:issue_deleted"],
            }],
        })),
    )
    .await?;
    let result = data
        .get("webhookRegistrationResult")
        .and_then(|r| r.get(0))
        .filter(|r| !r.is_null());
    let created = result.and_then(|r| r.get("createdWebhookId"));
    if !truthy(created) {
        let detail = result
            .and_then(|r| path(r, &["errors"]))
            .unwrap_or(&data)
            .to_string();
        return Err(Error::Msg(format!(
            "Jira webhook registration failed: {detail}"
        )));
    }
    let webhook_id = created
        .and_then(Value::as_i64)
        .and_then(|n| i32::try_from(n).ok())
        .ok_or_else(|| {
            Error::Msg("Invalid value for argument `jiraWebhookId`: Expected Int.".to_owned())
        })?;
    update_company(
        state,
        audit,
        company_id,
        &[
            Set::Int("jiraWebhookId", Some(webhook_id)),
            Set::Text("jiraWebhookSecret", Some(secret)),
        ],
    )
    .await?;
    tracing::info!("Jira webhook registered for company {company_id}, webhookId={webhook_id}");
    Ok(())
}

/// `deleteWebhookFromJira(companyId, accessToken, cloudId, webhookId)`.
pub(crate) async fn delete_webhook(
    state: &AppState,
    audit: &AuditHandle,
    company_id: i32,
    access_token: &str,
    cloud_id: &str,
    webhook_id: i32,
) -> Res<()> {
    http::send(
        http::bearer(
            state,
            Method::DELETE,
            &http::jira_url(state, cloud_id, "/webhook"),
            access_token,
        )
        .json(&json!({ "webhookIds": [webhook_id] })),
    )
    .await?;
    update_company(
        state,
        audit,
        company_id,
        &[
            Set::Int("jiraWebhookId", None),
            Set::Text("jiraWebhookSecret", None),
        ],
    )
    .await?;
    tracing::info!("Jira webhook deleted for company {company_id}, webhookId={webhook_id}");
    Ok(())
}
