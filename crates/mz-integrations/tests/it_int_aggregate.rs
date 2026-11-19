//! Uji integrasi `/integrations/*` (agregat) terhadap Postgres nyata. API eksternal tidak
//! disentuh: status hanya membaca kolom company, sync diuji pada jalur "tidak ter-link".
//! Jalankan: `MZ_TEST_DATABASE_URL=... cargo test -p mz-integrations --test it_int_aggregate`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use axum::Router;
use mz_core::AppState;
use mz_testkit::TestApp;
use serde_json::json;
use tokio::sync::Mutex;

static SERIAL: Mutex<()> = Mutex::const_new(());

fn build(state: &AppState) -> Router {
    mz_integrations::router(state).with_state(state.clone())
}

fn uniq() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// Project aktif milik company `c`; mengembalikan (id, uuid).
async fn project(app: &TestApp, c: i32, name: &str) -> (i32, String) {
    let u = uniq();
    let (id, uuid): (i32, String) = sqlx::query_as(&format!(
        r#"INSERT INTO project (code, name, "screenshotPerHour", "projectTypeId", "startDate", status)
           VALUES ('it{u}', '{name}', 3, (SELECT id FROM "projectType" ORDER BY id LIMIT 1), CURRENT_DATE, 1)
           RETURNING id, uuid::text"#
    ))
    .fetch_one(app.db())
    .await
    .unwrap();
    app.exec(&format!(
        r#"INSERT INTO "projectCompany" ("projectId", "companyId", status) VALUES ({id}, {c}, 1)"#
    ))
    .await;
    (id, uuid)
}

#[tokio::test]
async fn status_and_linked_projects() {
    let _g = SERIAL.lock().await;
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let c = app.company().await;

    // Gerbang role: STAFF tidak boleh `status`, boleh `projects/linked`.
    // RolesGuard proyek ini menolak dengan 401 ("Access denied: insufficient permissions").
    app.get("/integrations/status")
        .user(c.staff.id)
        .send()
        .await
        .assert_status(401);
    app.get("/integrations/status")
        .send()
        .await
        .assert_status(401);

    let r = app
        .get("/integrations/status")
        .user(c.admin.id)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json["success"], json!(true));
    assert_eq!(r.json["message"], json!("Success"));
    let keys: Vec<&str> = r.json["data"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys.len(), 3);
    assert!(
        r.text().contains(r#""data":{"clickup":{"connected":"#),
        "{}",
        r.text()
    );

    let (p1, u1) = project(&app, c.id, "IT Agg Satu").await;
    let (p2, u2) = project(&app, c.id, "IT Agg Dua").await;
    app.exec(&format!(
        r#"INSERT INTO "projectClickupMapping" ("projectId", "companyId", "clickupFolderId",
             "clickupFolderName", "clickupSpaceId", "clickupSpaceName")
           VALUES ({p1}, {}, 'f1', 'Folder 1', 's1', 'Space 1')"#,
        c.id
    ))
    .await;
    app.exec(&format!(
        r#"INSERT INTO "projectJiraMapping" ("projectId", "companyId", "jiraProjectId", "jiraProjectKey", "jiraProjectName")
           VALUES ({p1}, {c}, '100', 'AGG', 'Agg Jira'), ({p2}, {c}, '101', 'AGG2', 'Agg Jira 2')"#,
        c = c.id
    ))
    .await;
    app.exec(&format!(
        r#"INSERT INTO "projectDiscordMapping" ("projectId", "companyId", "discordChannelId", "discordChannelName")
           VALUES ({p2}, {}, '900', 'agg-channel')"#,
        c.id
    ))
    .await;

    let r = app
        .get("/integrations/projects/linked")
        .user(c.staff.id)
        .send()
        .await;
    r.assert_status(200);
    let raw = r.text();
    assert!(
        raw.starts_with(r#"{"success":true,"message":"Success","data":["#),
        "{raw}"
    );
    assert!(
        raw.contains(&format!(
            r#"{{"projectUuid":"{u1}","projectName":"IT Agg Satu","clickup":{{"folderId":"f1","folderName":"Folder 1","spaceName":"Space 1"}},"jira":{{"projectId":"100","projectKey":"AGG","projectName":"Agg Jira"}},"discord":null}}"#
        )),
        "{raw}"
    );
    assert!(
        raw.contains(&format!(
            r#"{{"projectUuid":"{u2}","projectName":"IT Agg Dua","clickup":null,"jira":{{"projectId":"101","projectKey":"AGG2","projectName":"Agg Jira 2"}},"discord":{{"channelId":"900","channelName":"agg-channel"}}}}"#
        )),
        "{raw}"
    );
    // ClickUp lebih dulu dari Jira-saja: p1 muncul sebelum p2.
    assert!(raw.find(&u1) < raw.find(&u2));
}

#[tokio::test]
async fn tasks_listing_and_validation() {
    let _g = SERIAL.lock().await;
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let c = app.company().await;
    let (p, u) = project(&app, c.id, "IT Agg Tasks").await;
    app.exec(&format!(
        r#"INSERT INTO task ("projectId", "companyId", source, "externalId", name, status, "dueDate",
                             assignees, "externalMeta", tags, "createdAt", "lastSyncedAt")
           VALUES
           ({p}, {c}, 'jira', 'J-1', 'Login bug_x', 'In Progress', '2026-04-10 00:00:00',
            '[{{"username": "John Doe", "email": "jd@x.test"}}, 5]', '{{"url": "u", "id": 1, "key": "J-1"}}', '["a"]', '2026-01-01', '2026-01-02'),
           ({p}, {c}, 'clickup', 'C-1', 'Other task', 'done', NULL,
            '"john"', NULL, NULL, '2026-01-02', '2026-01-02'),
           ({p}, {c}, 'manual', 'M-1', 'Manual task', NULL, NULL, NULL, NULL, NULL, '2026-01-03', '2026-01-02')"#,
        c = c.id
    ))
    .await;
    let path = format!("/integrations/projects/{u}/tasks");

    // Tanpa filter: hanya clickup/jira, urut source lalu createdAt.
    let r = app.get(&path).user(c.staff.id).send().await;
    r.assert_status(200);
    let d = &r.json["data"];
    assert_eq!(d["total"], json!(2));
    assert_eq!(d["totalPages"], json!(1));
    assert_eq!(d["projectName"], json!("IT Agg Tasks"));
    assert_eq!(d["tasks"][0]["source"], json!("clickup"));
    assert_eq!(d["tasks"][1]["externalId"], json!("J-1"));
    // jsonb dikirim dengan urutan kunci Postgres (kunci pendek dulu), tanpa spasi.
    assert!(
        r.text()
            .contains(r#""externalMeta":{"id":1,"key":"J-1","url":"u"}"#),
        "{}",
        r.text()
    );
    assert!(r.text().contains(r#""dueDate":"2026-04-10T00:00:00.000Z""#));

    // search = ILIKE tanpa escape (`_` wildcard), status insensitive persis.
    let r = app
        .get(&format!("{path}?search=BUG_&status=in%20progress"))
        .user(c.staff.id)
        .send()
        .await;
    assert_eq!(r.json["data"]["total"], json!(1));
    // assignee: username/email elemen array memuat teks (case-insensitive); nilai string
    // (C-1) dan `%`/`_` (bukan wildcard) tidak cocok.
    for (q, want) in [("JOHN", 1), ("x.test", 1), ("%25", 0), ("_", 0), ("zzz", 0)] {
        let r = app
            .get(&format!("{path}?assignee={q}"))
            .user(c.staff.id)
            .send()
            .await;
        assert_eq!(r.json["data"]["total"], json!(want), "{q}");
        if want == 1 {
            assert_eq!(r.json["data"]["tasks"][0]["externalId"], json!("J-1"));
        }
    }
    // Rentang dueDate.
    let r = app
        .get(&format!(
            "{path}?startDate=2026-04-10&endDate=2026-04-10T00:00:00Z&limit=1&page=1"
        ))
        .user(c.staff.id)
        .send()
        .await;
    assert_eq!(r.json["data"]["total"], json!(1));
    assert_eq!(r.json["data"]["limit"], json!(1));

    // Validasi query → 400 ValidationPipe.
    let r = app
        .get(&format!("{path}?source=manual&limit=101"))
        .user(c.staff.id)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        json!([
            "source must be one of the following values: clickup, jira",
            "limit must not be greater than 100"
        ])
    );

    // Project tidak ada / uuid rusak → 200 success:false.
    let r = app
        .get(&format!(
            "/integrations/projects/{}/tasks",
            uuid::Uuid::new_v4()
        ))
        .user(c.staff.id)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(
        r.text(),
        r#"{"success":false,"message":"Project not found","data":null}"#
    );
    let r = app
        .get("/integrations/projects/zz/tasks")
        .user(c.staff.id)
        .send()
        .await;
    assert_eq!(r.json["success"], json!(false));
    assert_eq!(
        r.text(),
        r#"{"success":false,"message":"Inconsistent column data: Error creating UUID, invalid character: expected an optional prefix of `urn:uuid:` followed by [0-9a-fA-F-], found `z` at 1","data":null}"#
    );
}

#[tokio::test]
async fn sync_all_not_linked_and_validation() {
    let _g = SERIAL.lock().await;
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let c = app.company().await;
    let (_, u) = project(&app, c.id, "IT Agg Sync").await;
    let path = format!("/integrations/projects/{u}/sync-tasks");

    app.post(&path)
        .user(c.staff.id)
        .send()
        .await
        .assert_status(401);
    let r = app
        .post(&path)
        .user(c.admin.id)
        .json(&json!({ "includeClosed": "true" }))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        json!(["includeClosed must be a boolean value"])
    );

    // Company tanpa kredensial: kedua service gagal SEBELUM cek link → bukan "not linked".
    app.exec(&format!(
        r#"UPDATE company SET "clickupApiToken" = NULL, "jiraAccessToken" = NULL WHERE id = {}"#,
        c.id
    ))
    .await;
    let r = app.post(&path).user(c.admin.id).send().await;
    r.assert_status(200);
    assert_eq!(
        r.text(),
        r#"{"success":true,"message":"Sync complete","data":{"clickup":{"synced":false,"error":"ClickUp API token not configured"},"jira":{"synced":false,"error":"Jira not connected"}}}"#
    );
}
