//! Fixture bersama uji integrasi Jira (company 2 template + server tiruan Atlassian).

#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use axum::Router;
use mz_core::AppState;
use mz_int_common::mock::MockServer;
use mz_testkit::{Company, TestApp};
use serde_json::{Map, Value, json};

pub static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub const CLOUD: &str = "cloud-1";
pub const SITE: &str = "https://acme.atlassian.net";

fn build(state: &AppState) -> Router {
    mz_int_jira::router(state).with_state(state.clone())
}

pub struct Ctx {
    pub app: TestApp,
    pub mock: MockServer,
    pub fx: Company,
    pub project_uuid: String,
    pub project2_uuid: String,
}

/// TestApp + mock + company bersih (tanpa koneksi Jira, mapping, task jira).
pub async fn setup() -> Option<Ctx> {
    let mock = MockServer::start().await;
    let url = mock.url();
    let app = TestApp::with_vars(
        build,
        &[
            ("MZ_ATLASSIAN_API_BASE", &url),
            ("MZ_ATLASSIAN_AUTH_BASE", &url),
            ("JIRA_CLIENT_ID", "cid"),
            ("JIRA_CLIENT_SECRET", "csecret"),
            (
                "JIRA_REDIRECT_URI",
                "http://be.test/integrations/jira/callback",
            ),
            ("FRONTEND_URL", "http://fe.test"),
        ],
    )
    .await?;
    let fx = app.company().await;
    let id = fx.id;
    app.exec(&format!(
        r#"UPDATE company SET "jiraAccessToken" = NULL, "jiraRefreshToken" = NULL,
             "jiraCloudId" = NULL, "jiraSiteUrl" = NULL, "jiraTokenExpiresAt" = NULL,
             "jiraWebhookId" = NULL, "jiraWebhookSecret" = NULL WHERE id = {id};
           -- Isolasi cron: dump template memuat company lain yang terhubung Jira (token
           -- kedaluwarsa), yang ikut di-refresh/di-sync oleh cron. DB ini salinan per uji.
           UPDATE company SET "jiraAccessToken" = NULL, "jiraRefreshToken" = NULL,
             "jiraCloudId" = NULL, "jiraTokenExpiresAt" = NULL, "jiraWebhookId" = NULL
             WHERE id <> {id} AND ("jiraAccessToken" IS NOT NULL OR "jiraCloudId" IS NOT NULL);
           DELETE FROM "projectJiraMapping" WHERE "companyId" = {id};
           DELETE FROM "userJiraMapping" WHERE "companyId" = {id};
           DELETE FROM task WHERE "companyId" = {id} AND source = 'jira';
           INSERT INTO project (code, name, "screenshotPerHour", "projectTypeId", "startDate", status)
             SELECT 'JIRA-IT-2', 'Jira IT Two', p."screenshotPerHour", p."projectTypeId", p."startDate", 1
             FROM project p JOIN "projectCompany" pc ON pc."projectId" = p.id
             WHERE pc."companyId" = {id} AND pc.status = 1 AND p.status = 1
               AND NOT EXISTS (SELECT 1 FROM project WHERE code = 'JIRA-IT-2')
             ORDER BY p.id LIMIT 1;
           INSERT INTO "projectCompany" ("projectId", "companyId", status)
             SELECT p.id, {id}, 1 FROM project p WHERE p.code = 'JIRA-IT-2'
               AND NOT EXISTS (SELECT 1 FROM "projectCompany" pc WHERE pc."projectId" = p.id);"#
    ))
    .await;
    let project_uuid: String = app
        .scalar(&format!(
            r#"SELECT p.uuid::text FROM project p JOIN "projectCompany" pc ON pc."projectId" = p.id
               WHERE pc."companyId" = {id} AND pc.status = 1 AND p.status = 1 AND p.code <> 'JIRA-IT-2'
               ORDER BY p.id LIMIT 1"#
        ))
        .await;
    let project2_uuid: String = app
        .scalar("SELECT uuid::text FROM project WHERE code = 'JIRA-IT-2'")
        .await;
    Some(Ctx {
        app,
        mock,
        fx,
        project_uuid,
        project2_uuid,
    })
}

impl Ctx {
    pub fn admin(&self) -> &str {
        &self.fx.super_admin.token
    }

    /// Hubungkan company: token berlaku `expires_secs` dari sekarang (negatif = kedaluwarsa).
    pub async fn connect(&self, expires_secs: i64, webhook_id: Option<i32>) {
        let wh = webhook_id.map_or("NULL".to_owned(), |w| w.to_string());
        self.app
            .exec(&format!(
                r#"UPDATE company SET "jiraAccessToken" = 'old-access', "jiraRefreshToken" = 'old-refresh',
                     "jiraCloudId" = '{CLOUD}', "jiraSiteUrl" = '{SITE}',
                     "jiraTokenExpiresAt" = (NOW() AT TIME ZONE 'UTC') + interval '{expires_secs} seconds',
                     "jiraWebhookId" = {wh}, "jiraWebhookSecret" = 'sekret-{id}'
                   WHERE id = {id}"#,
                id = self.fx.id
            ))
            .await;
    }

    pub fn jira(&self, path: &str) -> String {
        format!("/ex/jira/{CLOUD}/rest/api/3{path}")
    }

    /// `POST /oauth/token` membalas token baru.
    pub fn mock_refresh_ok(&self) {
        self.mock.on(
            "POST",
            "/oauth/token",
            200,
            json!({ "access_token": "new-access", "refresh_token": "new-refresh", "expires_in": 3600 }),
        );
    }

    pub async fn link(&self, project_uuid: &str, jira_id: &str, key: &str) -> Value {
        let res = self
            .app
            .post("/integrations/jira/projects/link")
            .token(self.admin())
            .json(&json!({
                "projectUuid": project_uuid, "jiraProjectId": jira_id,
                "jiraProjectKey": key, "jiraProjectName": format!("{key} name"),
            }))
            .send()
            .await;
        res.assert_status(200);
        res.json.clone()
    }

    pub async fn company_col(&self, col: &str) -> Option<String> {
        self.app
            .scalar(&format!(
                r#"SELECT "{col}"::text FROM company WHERE id = {}"#,
                self.fx.id
            ))
            .await
    }

    pub fn sign(&self, payload: Value, exp: Option<i64>) -> String {
        let map: Map<String, Value> = payload.as_object().cloned().unwrap_or_default();
        mz_int_common::nest_jwt::sign(&self.app.state, map, exp).unwrap()
    }
}

/// Issue Jira tiruan.
pub fn issue(id: &str, key: &str, project_id: &str, updated: &str) -> Value {
    json!({
        "id": id,
        "key": key,
        "fields": {
            "summary": format!("Issue {key}"),
            "description": {"type": "doc", "content": [{"type": "paragraph", "content": [{"type": "text", "text": "Body"}]}]},
            "status": {"name": "To Do"},
            "priority": {"name": "High"},
            "duedate": "2026-04-30",
            "assignee": {"accountId": "acc-1", "displayName": "John", "emailAddress": "john@x.test"},
            "labels": ["backend"],
            "issuetype": {"name": "Bug"},
            "timeoriginalestimate": 3600,
            "project": {"id": project_id, "key": "ENG"},
            "updated": updated,
        }
    })
}
