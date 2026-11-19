//! Uji integrasi cron Jira (`scheduledSyncAllProjects`, `scheduledRefreshWebhooks`).

#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

mod common;

use common::{SERIAL, issue, setup};
use serde_json::{Value, json};

#[tokio::test]
async fn it_int_jira_cron_sync_all_projects() {
    let _g = SERIAL.lock().await;
    let Some(c) = setup().await else { return };
    let jobs = mz_int_jira::cron_jobs();
    assert_eq!(
        jobs.iter().map(|j| j.name).collect::<Vec<_>>(),
        vec![
            "jira-scheduled-sync-all-projects",
            "jira-scheduled-refresh-webhooks"
        ]
    );

    // Token kedaluwarsa: di-refresh SEKALI untuk dua mapping company yang sama.
    c.connect(-60, None).await;
    c.link(&c.project_uuid, "10000", "ENG").await;
    c.link(&c.project2_uuid, "10001", "OPS").await;
    c.mock_refresh_ok();
    c.mock.on(
        "GET",
        &c.jira("/search/jql"),
        200,
        json!({ "issues": [issue("301", "ENG-30", "10000", "2026-04-30T10:00:00.000+0000")], "nextPageToken": null }),
    );
    c.mock.clear_log();
    mz_int_jira::scheduled_sync_all_projects(c.app.state.clone())
        .await
        .unwrap();
    assert_eq!(c.mock.requests_to("POST", "/oauth/token").len(), 1);
    let searches = c.mock.requests_to("GET", &c.jira("/search/jql"));
    assert_eq!(searches.len(), 2);
    for s in &searches {
        assert_eq!(s.header("authorization"), Some("Bearer new-access"));
    }
    // Issue yang sama (externalId 301) untuk dua project: baris pertama dibuat, sync
    // kedua memindahkannya (lookup per company, bukan per project — sama dengan Node).
    let n: i64 = c
        .app
        .scalar(r#"SELECT COUNT(*) FROM task WHERE "externalId" = '301' AND source = 'jira'"#)
        .await;
    assert_eq!(n, 1);
    assert_eq!(
        c.company_col("jiraAccessToken").await.as_deref(),
        Some("new-access")
    );

    // Refresh gagal → company dilewati, cron tidak gagal.
    c.connect(-60, None).await;
    c.mock
        .on("POST", "/oauth/token", 500, json!({ "error": "boom" }));
    c.mock.clear_log();
    mz_int_jira::scheduled_sync_all_projects(c.app.state.clone())
        .await
        .unwrap();
    assert!(c.mock.requests_to("GET", &c.jira("/search/jql")).is_empty());

    // Error search per mapping ditelan.
    c.connect(3600, None).await;
    c.mock
        .on("GET", &c.jira("/search/jql"), 500, json!({ "x": 1 }));
    mz_int_jira::scheduled_sync_all_projects(c.app.state.clone())
        .await
        .unwrap();
}

#[tokio::test]
async fn it_int_jira_cron_refresh_webhooks() {
    let _g = SERIAL.lock().await;
    let Some(c) = setup().await else { return };

    // Sukses: PUT /webhook/refresh dengan id webhook.
    c.connect(3600, Some(88)).await;
    c.mock
        .on("PUT", &c.jira("/webhook/refresh"), 200, json!({}));
    c.mock.clear_log();
    mz_int_jira::scheduled_refresh_webhooks(c.app.state.clone())
        .await
        .unwrap();
    let put = c.mock.requests_to("PUT", &c.jira("/webhook/refresh"));
    assert_eq!(put.len(), 1);
    assert_eq!(put[0].json(), json!({ "webhookIds": [88] }));
    assert_eq!(put[0].header("authorization"), Some("Bearer old-access"));

    // 410 → bersihkan + daftar ulang (token kedaluwarsa di-refresh dulu).
    c.connect(-60, Some(88)).await;
    c.mock_refresh_ok();
    c.mock
        .on("PUT", &c.jira("/webhook/refresh"), 410, json!({}));
    c.mock.on(
        "POST",
        &c.jira("/webhook"),
        200,
        json!({ "webhookRegistrationResult": [{ "createdWebhookId": 99 }] }),
    );
    mz_int_jira::scheduled_refresh_webhooks(c.app.state.clone())
        .await
        .unwrap();
    assert_eq!(c.company_col("jiraWebhookId").await.as_deref(), Some("99"));
    let secret = c.company_col("jiraWebhookSecret").await.unwrap();
    assert_ne!(secret, format!("sekret-{}", c.fx.id));
    assert_eq!(
        c.company_col("jiraAccessToken").await.as_deref(),
        Some("new-access")
    );

    // Registrasi ulang gagal (tanpa createdWebhookId): webhook lama sudah dibersihkan.
    c.connect(3600, Some(88)).await;
    c.mock.on(
        "POST",
        &c.jira("/webhook"),
        200,
        json!({ "webhookRegistrationResult": [{ "errors": ["nope"] }] }),
    );
    mz_int_jira::scheduled_refresh_webhooks(c.app.state.clone())
        .await
        .unwrap();
    assert_eq!(c.company_col("jiraWebhookId").await, None);

    // Error lain (500) → baris tidak diubah.
    c.connect(3600, Some(88)).await;
    c.mock
        .on("PUT", &c.jira("/webhook/refresh"), 500, Value::Null);
    mz_int_jira::scheduled_refresh_webhooks(c.app.state.clone())
        .await
        .unwrap();
    assert_eq!(c.company_col("jiraWebhookId").await.as_deref(), Some("88"));
}

/// `trackWorklog` (dipanggil `mz-timelines` sesudah timeline berubah).
#[tokio::test]
async fn it_int_jira_track_worklog() {
    let _g = SERIAL.lock().await;
    let Some(c) = setup().await else { return };
    let st = c.app.state();
    let user = c.fx.super_admin.id;
    let worklog = c.jira("/issue/ENG-7/worklog");

    // Belum terhubung → None tanpa request.
    assert_eq!(
        mz_int_jira::track_worklog(st, user, "ENG-7", 0, 120_000, None, None).await,
        None
    );
    assert!(c.mock.requests().is_empty());

    // Token kedaluwarsa → refresh (server tiruan, bukan Atlassian sungguhan) → POST worklog.
    c.connect(-60, None).await;
    c.mock_refresh_ok();
    c.mock
        .on("POST", &worklog, 201, serde_json::json!({ "id": "30001" }));
    let start = 1_778_148_000_000; // 2026-05-07T10:00:00Z
    let id = mz_int_jira::track_worklog(
        st,
        user,
        "ENG-7",
        start,
        start + 90_400,
        Some("rapat"),
        None,
    )
    .await;
    assert_eq!(id.as_deref(), Some("30001"));
    let req = &c.mock.requests_to("POST", &worklog)[0];
    assert_eq!(req.header("authorization"), Some("Bearer new-access"));
    let body = req.json();
    assert_eq!(body["timeSpentSeconds"], 90);
    assert_eq!(body["started"], "2026-05-07T10:00:00.000+0000");
    let username: String = c
        .app
        .scalar(&format!("SELECT username FROM users WHERE id = {user}"))
        .await;
    assert_eq!(
        body["comment"]["content"][0]["content"][0]["text"],
        format!("rapat | Tracked by: {username}")
    );
    assert!(body.get("author").is_none());
    assert_eq!(
        c.company_col("jiraAccessToken").await.as_deref(),
        Some("new-access")
    );

    // Sudah ada → PUT; < 60 detik → None tanpa request.
    c.mock.on(
        "PUT",
        &format!("{worklog}/30001"),
        200,
        serde_json::json!({}),
    );
    c.mock.clear_log();
    let id = mz_int_jira::track_worklog(
        st,
        user,
        "ENG-7",
        start,
        start + 61_000,
        None,
        Some("30001"),
    )
    .await;
    assert_eq!(id.as_deref(), Some("30001"));
    assert_eq!(
        c.mock.requests_to("PUT", &format!("{worklog}/30001")).len(),
        1
    );
    c.mock.clear_log();
    assert_eq!(
        mz_int_jira::track_worklog(st, user, "ENG-7", start, start + 59_000, None, None).await,
        None
    );
    assert!(c.mock.requests_to("POST", &worklog).is_empty());

    // Galat Jira → None.
    c.mock.on(
        "POST",
        &worklog,
        400,
        serde_json::json!({ "errorMessages": ["x"] }),
    );
    assert_eq!(
        mz_int_jira::track_worklog(st, user, "ENG-7", start, start + 120_000, None, None).await,
        None
    );
}
