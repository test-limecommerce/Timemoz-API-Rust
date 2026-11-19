//! Uji integrasi route `integrations/jira/*` (Postgres nyata + server Atlassian tiruan).

#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

mod common;

use common::{CLOUD, SERIAL, SITE, issue, setup};
use serde_json::{Value, json};

#[tokio::test]
async fn it_int_jira_status_and_roles() {
    let _g = SERIAL.lock().await;
    let Some(c) = setup().await else { return };

    let res = c
        .app
        .get("/integrations/jira/status")
        .token(c.admin())
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(
        res.text(),
        r#"{"success":true,"message":"Success","data":{"connected":false,"cloudId":null}}"#
    );

    // RolesGuard: STAFF ditolak (RolesGuard Node melempar 401).
    let res = c
        .app
        .get("/integrations/jira/status")
        .token(&c.fx.staff.token)
        .send()
        .await;
    res.assert_status(401);
    assert_eq!(
        res.json["message"],
        "Access denied: insufficient permissions"
    );
    // Tanpa JWT.
    c.app
        .get("/integrations/jira/status")
        .send()
        .await
        .assert_status(401);

    c.connect(3600, None).await;
    let env = mz_int_jira::get_connection_status(&c.app.state, c.fx.super_admin.id).await;
    assert_eq!(
        serde_json::to_string(&env).unwrap(),
        format!(
            r#"{{"success":true,"message":"Success","data":{{"connected":true,"cloudId":"{CLOUD}"}}}}"#
        )
    );
    // User tanpa role admin di company mana pun.
    let env = mz_int_jira::get_connection_status(&c.app.state, c.fx.staff.id).await;
    assert_eq!(
        serde_json::to_string(&env).unwrap(),
        r#"{"success":false,"message":"Company not found","data":null}"#
    );
}

#[tokio::test]
async fn it_int_jira_connect_guard_and_redirect() {
    let _g = SERIAL.lock().await;
    let Some(c) = setup().await else { return };

    let res = c.app.get("/integrations/jira/connect").send().await;
    res.assert_status(401);
    assert_eq!(res.text(), r#"{"message":"Unauthorized","statusCode":401}"#);

    let res = c
        .app
        .get("/integrations/jira/connect?token=garbage")
        .send()
        .await;
    res.assert_status(401);

    // `jwtService.verify` memeriksa exp.
    let expired = c.sign(json!({ "userId": c.fx.super_admin.id }), Some(-10));
    c.app
        .get(&format!("/integrations/jira/connect?token={expired}"))
        .send()
        .await
        .assert_status(401);

    let tok = c.sign(json!({ "userId": c.fx.super_admin.id }), Some(600));
    let res = c
        .app
        .get(&format!("/integrations/jira/connect?token={tok}"))
        .send()
        .await;
    res.assert_status(302);
    let loc = res.headers.get("location").unwrap().to_str().unwrap();
    assert_eq!(
        loc,
        format!(
            "https://auth.atlassian.com/authorize?response_type=code&client_id=cid\
             &redirect_uri=http%3A%2F%2Fbe.test%2Fintegrations%2Fjira%2Fcallback\
             &scope=read%3Ame%20read%3Ajira-work%20write%3Ajira-work%20offline_access%20read%3Aaccount%20read%3Ajira-user\
             &audience=api.atlassian.com&state={}",
            c.fx.super_admin.id
        )
    );
    assert_eq!(res.text(), format!("Found. Redirecting to {loc}"));
}

#[tokio::test]
async fn it_int_jira_oauth_callback() {
    let _g = SERIAL.lock().await;
    let Some(c) = setup().await else { return };
    let uid = c.fx.super_admin.id;

    // Gagal: Atlassian menolak code.
    c.mock.on(
        "POST",
        "/oauth/token",
        400,
        json!({ "error": "invalid_grant" }),
    );
    let res = c
        .app
        .get(&format!("/integrations/jira/callback?code=bad&state={uid}"))
        .send()
        .await;
    res.assert_status(302);
    assert_eq!(
        res.headers.get("location").unwrap(),
        "http://fe.test/integrations/jira?status=error&message=Request%20failed%20with%20status%20code%20400"
    );

    // Gagal: tidak ada site.
    c.mock.on(
        "POST",
        "/oauth/token",
        200,
        json!({ "access_token": "acc-1", "refresh_token": "ref-1", "expires_in": 3600 }),
    );
    c.mock
        .on("GET", "/oauth/token/accessible-resources", 200, json!([]));
    let res = c
        .app
        .get(&format!("/integrations/jira/callback?code=ok&state={uid}"))
        .send()
        .await;
    assert_eq!(
        res.headers.get("location").unwrap(),
        "http://fe.test/integrations/jira?status=error&message=No%20Jira%20cloud%20sites%20found%20for%20this%20account"
    );

    // Gagal: state bukan user admin.
    let res = c
        .app
        .get(&format!(
            "/integrations/jira/callback?code=ok&state={}",
            c.fx.staff.id
        ))
        .send()
        .await;
    assert_eq!(
        res.headers.get("location").unwrap(),
        "http://fe.test/integrations/jira?status=error&message=Company%20not%20found"
    );

    // Sukses + registrasi webhook.
    c.mock.on(
        "GET",
        "/oauth/token/accessible-resources",
        200,
        json!([{ "id": CLOUD, "url": SITE, "name": "Acme" }]),
    );
    c.mock.on(
        "POST",
        &c.jira("/webhook"),
        200,
        json!({ "webhookRegistrationResult": [{ "createdWebhookId": 77 }] }),
    );
    c.mock.clear_log();
    let audit_before: i64 = c
        .app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "auditLog" WHERE "entityType" = 'company' AND "entityId" = {}"#,
            c.fx.id
        ))
        .await;
    let res = c
        .app
        .get(&format!(
            "/integrations/jira/callback?code=good&state={uid}"
        ))
        .send()
        .await;
    res.assert_status(302);
    assert_eq!(
        res.headers.get("location").unwrap(),
        "http://fe.test/integrations/jira?status=connected"
    );
    let token_req = &c.mock.requests_to("POST", "/oauth/token")[0];
    let body = token_req.json();
    assert_eq!(body["grant_type"], "authorization_code");
    assert_eq!(body["code"], "good");
    assert_eq!(body["client_id"], "cid");
    assert_eq!(
        body["redirect_uri"],
        "http://be.test/integrations/jira/callback"
    );
    let res_req = &c
        .mock
        .requests_to("GET", "/oauth/token/accessible-resources")[0];
    assert_eq!(res_req.header("authorization"), Some("Bearer acc-1"));
    let wh = c.mock.requests_to("POST", &c.jira("/webhook"))[0].json();
    let url = wh["url"].as_str().unwrap();
    assert!(url.starts_with("http://be.test/integrations/jira/webhook?s="));
    assert_eq!(
        wh["webhooks"][0]["events"],
        json!([
            "jira:issue_created",
            "jira:issue_updated",
            "jira:issue_deleted"
        ])
    );

    assert_eq!(
        c.company_col("jiraAccessToken").await.as_deref(),
        Some("acc-1")
    );
    assert_eq!(
        c.company_col("jiraRefreshToken").await.as_deref(),
        Some("ref-1")
    );
    assert_eq!(c.company_col("jiraCloudId").await.as_deref(), Some(CLOUD));
    assert_eq!(c.company_col("jiraSiteUrl").await.as_deref(), Some(SITE));
    assert_eq!(c.company_col("jiraWebhookId").await.as_deref(), Some("77"));
    let secret = c.company_col("jiraWebhookSecret").await.unwrap();
    assert_eq!(secret.len(), 64);
    assert!(url.ends_with(&secret));
    // Tulisan `company` tercatat di audit (token disaring kebijakan kolom).
    let audit_after: i64 = c
        .app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "auditLog" WHERE "entityType" = 'company' AND "entityId" = {}"#,
            c.fx.id
        ))
        .await;
    assert!(
        audit_after > audit_before,
        "{audit_before} -> {audit_after}"
    );
    let leaked: i64 = c
        .app
        .scalar(r#"SELECT COUNT(*) FROM "auditLog" WHERE after::text LIKE '%acc-1%'"#)
        .await;
    assert_eq!(leaked, 0);
}

#[tokio::test]
async fn it_int_jira_expired_token_refresh_and_projects() {
    let _g = SERIAL.lock().await;
    let Some(c) = setup().await else { return };

    // Belum terhubung.
    let res = c
        .app
        .get("/integrations/jira/projects")
        .token(c.admin())
        .send()
        .await;
    assert_eq!(
        res.text(),
        r#"{"success":false,"message":"Jira not connected","data":null}"#
    );

    // Token kedaluwarsa + refresh gagal → pesan axios.
    c.connect(-60, None).await;
    c.mock.on(
        "POST",
        "/oauth/token",
        401,
        json!({ "error": "unauthorized_client" }),
    );
    let res = c
        .app
        .get("/integrations/jira/projects")
        .token(c.admin())
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(
        res.text(),
        r#"{"success":false,"message":"Request failed with status code 401","data":null}"#
    );
    assert_eq!(
        c.company_col("jiraAccessToken").await.as_deref(),
        Some("old-access")
    );

    // Token kedaluwarsa → refresh sukses → request Jira memakai token baru.
    c.mock_refresh_ok();
    c.mock.on(
        "GET",
        &c.jira("/project/search"),
        200,
        json!({ "values": [
            { "id": "10000", "key": "ENG", "name": "Engineering", "projectTypeKey": "software",
              "avatarUrls": { "48x48": "https://a/48.png" } },
            { "id": "10001", "key": "OPS", "name": "Ops", "projectTypeKey": "business" }
        ]}),
    );
    c.link(&c.project_uuid, "10000", "ENG").await;
    c.mock.clear_log();
    let res = c
        .app
        .get("/integrations/jira/projects")
        .token(c.admin())
        .send()
        .await;
    res.assert_ok();
    let refresh = &c.mock.requests_to("POST", "/oauth/token")[0];
    let body = refresh.json();
    assert_eq!(body["grant_type"], "refresh_token");
    assert_eq!(body["refresh_token"], "old-refresh");
    let search = &c.mock.requests_to("GET", &c.jira("/project/search"))[0];
    assert_eq!(search.header("authorization"), Some("Bearer new-access"));
    assert!(search.query.contains("maxResults=100"));
    assert_eq!(
        c.company_col("jiraAccessToken").await.as_deref(),
        Some("new-access")
    );
    assert_eq!(
        c.company_col("jiraRefreshToken").await.as_deref(),
        Some("new-refresh")
    );
    let fresh: bool = c
        .app
        .scalar(&format!(
            r#"SELECT "jiraTokenExpiresAt" > (NOW() AT TIME ZONE 'UTC') + interval '50 minutes'
               FROM company WHERE id = {}"#,
            c.fx.id
        ))
        .await;
    assert!(fresh);
    let data = res.data().as_array().unwrap();
    assert_eq!(data.len(), 2);
    assert_eq!(
        serde_json::to_string(&data[1]).unwrap(),
        r#"{"id":"10001","key":"OPS","name":"Ops","projectTypeKey":"business","avatarUrl":null,"linkedProject":null}"#
    );
    let first = res.text();
    assert!(first.contains(&format!(
        r#"{{"id":"10000","key":"ENG","name":"Engineering","projectTypeKey":"software","avatarUrl":"https://a/48.png","linkedProject":{{"uuid":"{}","name":"#,
        c.project_uuid
    )));

    // Token masih berlaku → tidak ada refresh.
    c.mock.clear_log();
    c.app
        .get("/integrations/jira/projects")
        .token(c.admin())
        .send()
        .await
        .assert_ok();
    assert!(c.mock.requests_to("POST", "/oauth/token").is_empty());

    // Jira 5xx diteruskan sebagai pesan axios.
    c.mock
        .on("GET", &c.jira("/project/search"), 503, json!({ "x": 1 }));
    let res = c
        .app
        .get("/integrations/jira/projects")
        .token(c.admin())
        .send()
        .await;
    assert_eq!(
        res.text(),
        r#"{"success":false,"message":"Request failed with status code 503","data":null}"#
    );
}

#[tokio::test]
async fn it_int_jira_link_projects_and_users() {
    let _g = SERIAL.lock().await;
    let Some(c) = setup().await else { return };

    // Validasi body.
    let res = c
        .app
        .post("/integrations/jira/projects/link")
        .token(c.admin())
        .json(&json!({ "projectUuid": 5 }))
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        json!([
            "projectUuid must be a string",
            "jiraProjectId must be a string",
            "jiraProjectKey must be a string",
            "jiraProjectName must be a string"
        ])
    );

    let v = c.link(&c.project_uuid, "10000", "ENG").await;
    assert_eq!(v["success"], true);
    assert_eq!(v["message"], "Project linked to Jira project successfully");
    let keys: Vec<&String> = v["data"].as_object().unwrap().keys().collect();
    assert_eq!(keys.len(), 6);
    // Upsert: link ulang project yang sama.
    let v = c.link(&c.project_uuid, "10000", "ENG").await;
    assert_eq!(v["success"], true);
    // Konflik: project Jira sama ke project lain.
    let v = c.link(&c.project2_uuid, "10000", "ENG").await;
    assert_eq!(v["success"], false);
    assert!(
        v["message"]
            .as_str()
            .unwrap()
            .starts_with("This Jira project is already linked to project '")
    );
    let v = c
        .link("00000000-0000-0000-0000-000000000000", "1", "X")
        .await;
    assert_eq!(v["message"], "Project not found");

    let res = c
        .app
        .get("/integrations/jira/linked-projects")
        .token(c.admin())
        .send()
        .await;
    res.assert_ok();
    let rows = res.data().as_array().unwrap();
    assert_eq!(rows.len(), 1);
    let row = res.text();
    assert!(row.contains(r#""jiraProjectId":"10000","jiraProjectKey":"ENG","jiraProjectName":"ENG name","linkedAt":""#));

    // Unlink.
    let path = format!("/integrations/jira/projects/{}/link", c.project_uuid);
    let res = c.app.delete(&path).token(c.admin()).send().await;
    assert_eq!(
        res.text(),
        r#"{"success":true,"message":"Project unlinked from Jira successfully","data":null}"#
    );
    let res = c.app.delete(&path).token(c.admin()).send().await;
    assert_eq!(
        res.json["message"],
        "No active Jira link found for this project"
    );

    // Users: members + link + konflik + unlink.
    c.connect(3600, None).await;
    c.mock.on(
        "GET",
        &c.jira("/users/search"),
        200,
        json!([
            { "accountId": "acc-1", "accountType": "atlassian", "displayName": "John",
              "emailAddress": "john@x.test", "avatarUrls": { "48x48": "https://a/j.png" } },
            { "accountId": "bot-1", "accountType": "app", "displayName": "Bot" }
        ]),
    );
    let res = c
        .app
        .post("/integrations/jira/users/link")
        .token(c.admin())
        .json(&json!({ "userUuid": c.fx.staff.uuid, "jiraAccountId": "acc-1", "jiraDisplayName": "John" }))
        .send()
        .await;
    res.assert_ok();
    assert_eq!(
        res.text(),
        format!(
            r#"{{"success":true,"message":"User linked to Jira member successfully","data":{{"uuid":"{}","userUuid":"{}","userName":{},"jiraAccountId":"acc-1","jiraDisplayName":"John","jiraEmail":null}}}}"#,
            res.data()["uuid"].as_str().unwrap(),
            c.fx.staff.uuid,
            res.data()["userName"]
        )
    );
    let res = c
        .app
        .post("/integrations/jira/users/link")
        .token(c.admin())
        .json(&json!({ "userUuid": c.fx.staff2.uuid, "jiraAccountId": "acc-1" }))
        .send()
        .await;
    assert!(
        res.json["message"]
            .as_str()
            .unwrap()
            .starts_with("This Jira member is already linked to user '")
    );
    let res = c
        .app
        .get("/integrations/jira/members")
        .token(c.admin())
        .send()
        .await;
    res.assert_ok();
    let members = res.data().as_array().unwrap();
    assert_eq!(members.len(), 1, "accountType != atlassian disaring");
    assert_eq!(members[0]["linkedUser"]["uuid"], c.fx.staff.uuid.as_str());
    assert!(res.text().contains(
        r#"{"jiraAccountId":"acc-1","jiraDisplayName":"John","jiraEmail":"john@x.test","jiraAvatarUrl":"https://a/j.png","linkedUser":{"uuid":"#
    ));
    let path = format!("/integrations/jira/users/{}/link", c.fx.staff.uuid);
    let res = c.app.delete(&path).token(c.admin()).send().await;
    assert_eq!(res.json["message"], "User unlinked from Jira successfully");
    let res = c
        .app
        .delete("/integrations/jira/users/not-a-uuid/link")
        .token(c.admin())
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(res.json["success"], false);
}

#[tokio::test]
async fn it_int_jira_sync_tasks_statuses_and_webhook() {
    let _g = SERIAL.lock().await;
    let Some(c) = setup().await else { return };
    let sync_path = format!("/integrations/jira/projects/{}/sync-tasks", c.project_uuid);

    // Belum di-link.
    c.connect(3600, None).await;
    let res = c.app.post(&sync_path).token(c.admin()).send().await;
    assert_eq!(
        res.json["message"],
        "Project not found or not linked to a Jira project"
    );
    // Validasi body.
    let res = c
        .app
        .post(&sync_path)
        .token(c.admin())
        .json(&json!({ "includeClosed": "yes" }))
        .send()
        .await;
    res.assert_status(400);

    c.link(&c.project_uuid, "10000", "ENG").await;
    c.mock.on(
        "GET",
        &c.jira("/search/jql"),
        200,
        json!({
            "issues": [
                issue("101", "ENG-1", "10000", "2026-04-30T10:00:00.000+0700"),
                issue("102", "ENG-2", "10000", "2026-04-30T10:00:00.000+0700"),
                { "id": "103" }
            ],
            "isLast": true
        }),
    );
    let res = c
        .app
        .post(&sync_path)
        .token(c.admin())
        .json(&json!({ "includeClosed": false }))
        .send()
        .await;
    assert_eq!(
        res.text(),
        r#"{"success":true,"message":"Sync complete: 2 tasks created, 0 tasks updated","data":{"created":2,"updated":0,"skipped":0,"total":2}}"#
    );
    let req = &c.mock.requests_to("GET", &c.jira("/search/jql"))[0];
    assert!(req.query.contains("statusCategory"), "{}", req.query);
    assert_eq!(req.header("authorization"), Some("Bearer old-access"));

    // Sync ulang: `updated` jauh sebelum lastSyncedAt → dilewati.
    let res = c.app.post(&sync_path).token(c.admin()).send().await;
    assert_eq!(
        res.json["data"],
        json!({ "created": 0, "updated": 0, "skipped": 2, "total": 2 })
    );
    // Lewat agregat (fungsi pub).
    let env =
        mz_int_jira::sync_tasks(&c.app.state, c.fx.super_admin.id, &c.project_uuid, true).await;
    assert!(env.success);

    // Daftar task + filter.
    let tasks_path = format!("/integrations/jira/projects/{}/tasks", c.project_uuid);
    let res = c
        .app
        .get(&format!("{tasks_path}?search=eng-2&limit=10"))
        .token(c.admin())
        .send()
        .await;
    res.assert_ok();
    let d = res.data();
    assert_eq!(d["total"], 1);
    assert_eq!(d["totalPages"], 1);
    let text = res.text();
    assert!(
        text.contains(r#""externalMeta":{"issueKey":"ENG-1"#.replace("ENG-1", "ENG-2").as_str()),
        "{text}"
    );
    assert!(
        text.contains(
            r#""assignees":[{"email":"john@x.test","username":"John","externalUserId":"acc-1"}]"#
        ),
        "{text}"
    );
    assert!(
        text.contains(r#""dueDate":"2026-04-30T00:00:00.000Z","startDate":null"#),
        "{text}"
    );
    assert!(text.contains(&format!(r#""externalUrl":"{SITE}/browse/ENG-2""#)));
    // `assignee`: username/email elemen array memuat teks, case-insensitive.
    for (q, want) in [("John", 2), ("JOHN@x", 2), ("%25", 0), ("nobody", 0)] {
        let res = c
            .app
            .get(&format!("{tasks_path}?assignee={q}"))
            .token(c.admin())
            .send()
            .await;
        assert_eq!(res.data()["total"], want, "{q}");
    }
    let res = c
        .app
        .get(&format!(
            "{tasks_path}?status=to%20do&startDate=2026-04-01&endDate=2026-05-01"
        ))
        .token(c.admin())
        .send()
        .await;
    assert_eq!(res.data()["total"], 2);
    let res = c
        .app
        .get(&format!("{tasks_path}?limit=500"))
        .token(c.admin())
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        json!(["limit must not be greater than 100"])
    );

    // Status transisi.
    let task_uuid: String = c
        .app
        .scalar(r#"SELECT uuid::text FROM task WHERE "externalId" = '101' AND source = 'jira'"#)
        .await;
    let tr_path = c.jira("/issue/101/transitions");
    c.mock.on(
        "GET",
        &tr_path,
        200,
        json!({ "transitions": [
            { "id": "11", "name": "In Progress", "to": { "statusCategory": { "colorName": "yellow" } } },
            { "id": "31", "name": "Done", "to": { "statusCategory": { "colorName": "green" } } }
        ]}),
    );
    c.mock.on("POST", &tr_path, 204, Value::Null);
    let res = c
        .app
        .get(&format!("/integrations/jira/tasks/{task_uuid}/statuses"))
        .token(c.admin())
        .send()
        .await;
    assert_eq!(
        res.text(),
        r#"{"success":true,"message":"Statuses fetched successfully","data":[{"id":"11","name":"In Progress","color":"yellow"},{"id":"31","name":"Done","color":"green"}]}"#
    );
    let status_path = format!("/integrations/jira/tasks/{task_uuid}/status");
    let res = c
        .app
        .patch(&status_path)
        .token(c.admin())
        .json(&json!({ "status": "done" }))
        .send()
        .await;
    assert_eq!(
        res.text(),
        format!(
            r#"{{"success":true,"message":"Task status updated successfully","data":{{"taskUuid":"{task_uuid}","status":"done"}}}}"#
        )
    );
    assert_eq!(
        c.mock.requests_to("POST", &tr_path)[0].json(),
        json!({ "transition": { "id": "31" } })
    );
    let st: String = c
        .app
        .scalar(r#"SELECT status FROM task WHERE "externalId" = '101' AND source = 'jira'"#)
        .await;
    assert_eq!(st, "done");
    let res = c
        .app
        .patch(&status_path)
        .token(c.admin())
        .json(&json!({ "status": "Blocked" }))
        .send()
        .await;
    assert_eq!(
        res.json["message"],
        "Transition 'Blocked' not available for this issue"
    );
    let res = c
        .app
        .patch(&status_path)
        .token(c.admin())
        .json(&json!({}))
        .send()
        .await;
    res.assert_status(400);
    let res = c
        .app
        .get("/integrations/jira/tasks/00000000-0000-0000-0000-000000000000/statuses")
        .token(c.admin())
        .send()
        .await;
    assert_eq!(res.json["message"], "Task not found");

    // 410 dari search → pesan khusus Node.
    c.mock.on(
        "GET",
        &c.jira("/search/jql"),
        410,
        json!({ "errorMessages": ["gone a", "gone b"] }),
    );
    let res = c.app.post(&sync_path).token(c.admin()).send().await;
    assert_eq!(
        res.json["message"],
        "Jira sync failed for project 'ENG' (410 Gone): gone a; gone b"
    );

    // Webhook.
    let res = c
        .app
        .post("/integrations/jira/webhook")
        .json(&json!({}))
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(res.text(), r#"{"ok":false}"#);
    let secret = format!("sekret-{}", c.fx.id);
    let created = json!({
        "webhookEvent": "jira:issue_created",
        "issue": issue("201", "ENG-9", "10000", "2026-05-01T00:00:00.000+0000"),
    });
    let res = c
        .app
        .post(&format!("/integrations/jira/webhook?s={secret}"))
        .json(&created)
        .send()
        .await;
    assert_eq!(res.text(), r#"{"ok":true}"#);
    let n: i64 = c
        .app
        .scalar(r#"SELECT COUNT(*) FROM task WHERE "externalId" = '201' AND source = 'jira'"#)
        .await;
    assert_eq!(n, 1);
    // `issue_updated` untuk issue yang belum ada tidak membuat baris.
    let upd = json!({
        "webhookEvent": "jira:issue_updated",
        "issue": issue("202", "ENG-10", "10000", "2026-05-01T00:00:00.000+0000"),
    });
    c.app
        .post(&format!("/integrations/jira/webhook?s={secret}"))
        .json(&upd)
        .send()
        .await
        .assert_status(200);
    let n: i64 = c
        .app
        .scalar(r#"SELECT COUNT(*) FROM task WHERE "externalId" = '202'"#)
        .await;
    assert_eq!(n, 0);
    let del = json!({ "webhookEvent": "jira:issue_deleted", "issue": { "id": 201 } });
    c.app
        .post(&format!("/integrations/jira/webhook?s={secret}"))
        .json(&del)
        .send()
        .await
        .assert_status(200);
    let st: String = c
        .app
        .scalar(r#"SELECT status FROM task WHERE "externalId" = '201' AND source = 'jira'"#)
        .await;
    assert_eq!(st, "Deleted");
    // Secret tak dikenal → tetap ok.
    let res = c
        .app
        .post("/integrations/jira/webhook?s=nope")
        .json(&created)
        .send()
        .await;
    assert_eq!(res.text(), r#"{"ok":true}"#);
    // JSON rusak → 400 body-parser.
    c.app
        .post(&format!("/integrations/jira/webhook?s={secret}"))
        .raw("application/json", "{bad")
        .send()
        .await
        .assert_status(400);
}

#[tokio::test]
async fn it_int_jira_disconnect() {
    let _g = SERIAL.lock().await;
    let Some(c) = setup().await else { return };

    // Token kedaluwarsa → refresh → hapus webhook → kolom dikosongkan.
    c.connect(-60, Some(55)).await;
    c.link(&c.project_uuid, "10000", "ENG").await;
    c.mock_refresh_ok();
    c.mock.on("DELETE", &c.jira("/webhook"), 202, Value::Null);
    let res = c
        .app
        .delete("/integrations/jira/disconnect")
        .token(c.admin())
        .send()
        .await;
    assert_eq!(
        res.text(),
        r#"{"success":true,"message":"Jira disconnected successfully","data":null}"#
    );
    let del = &c.mock.requests_to("DELETE", &c.jira("/webhook"))[0];
    assert_eq!(del.json(), json!({ "webhookIds": [55] }));
    assert_eq!(del.header("authorization"), Some("Bearer new-access"));
    for col in [
        "jiraAccessToken",
        "jiraRefreshToken",
        "jiraCloudId",
        "jiraTokenExpiresAt",
        "jiraWebhookId",
        "jiraWebhookSecret",
    ] {
        assert_eq!(c.company_col(col).await, None, "{col}");
    }
    // `jiraSiteUrl` tidak dikosongkan Node.
    assert_eq!(c.company_col("jiraSiteUrl").await.as_deref(), Some(SITE));
    let active: i64 = c
        .app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "projectJiraMapping" WHERE "companyId" = {} AND status = 1"#,
            c.fx.id
        ))
        .await;
    assert_eq!(active, 0);

    // Hapus webhook gagal → tetap terputus.
    c.connect(3600, Some(56)).await;
    c.mock
        .on("DELETE", &c.jira("/webhook"), 500, json!({ "err": 1 }));
    let res = c
        .app
        .delete("/integrations/jira/disconnect")
        .token(c.admin())
        .send()
        .await;
    assert_eq!(res.json["success"], true);
    assert_eq!(c.company_col("jiraWebhookId").await, None);
}
