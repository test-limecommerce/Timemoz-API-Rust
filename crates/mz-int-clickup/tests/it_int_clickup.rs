//! Uji integrasi `integrations/clickup/*` terhadap Postgres nyata + ClickUp tiruan
//! (`MockServer`). Jalankan:
//! `MZ_TEST_DATABASE_URL=... MZ_TEST_REQUIRE_DB=1 cargo test -p mz-int-clickup --test it_int_clickup`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use axum::Router;
use hmac::{Hmac, Mac};
use mz_core::AppState;
use mz_int_common::mock::MockServer;
use mz_testkit::{Company, TestApp};
use serde_json::{Value, json};
use sha2::Sha256;
use tokio::sync::Mutex;

static SERIAL: Mutex<()> = Mutex::const_new(());

const TOKEN: &str = "pk_test_token";
const WS: &str = "ws1";
const BASE: &str = "/integrations/clickup";

fn build(state: &AppState) -> Router {
    mz_int_clickup::router(state).with_state(state.clone())
}

fn uniq() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

type Orig = (Option<String>, Option<String>);

async fn setup() -> Option<(TestApp, MockServer, Company, Orig)> {
    let mock = MockServer::start().await;
    let app = TestApp::with_vars(build, &[("MZ_CLICKUP_API_BASE", &mock.url())]).await?;
    let fx = app.company().await;
    let orig: Orig = sqlx::query_as(
        r#"SELECT "clickupApiToken", "clickupWorkspaceId" FROM company WHERE id = $1"#,
    )
    .bind(fx.id)
    .fetch_one(app.db())
    .await
    .unwrap();
    set_creds(&app, fx.id, Some(TOKEN), Some(WS)).await;
    Some((app, mock, fx, orig))
}

async fn restore(app: &TestApp, company: i32, orig: &Orig) {
    set_creds(app, company, orig.0.as_deref(), orig.1.as_deref()).await;
}

async fn set_creds(app: &TestApp, company: i32, token: Option<&str>, ws: Option<&str>) {
    sqlx::query(
        r#"UPDATE company SET "clickupApiToken" = $2, "clickupWorkspaceId" = $3 WHERE id = $1"#,
    )
    .bind(company)
    .bind(token)
    .bind(ws)
    .execute(app.db())
    .await
    .unwrap();
}

/// Project aktif milik company fixture → (id, uuid).
async fn project(app: &TestApp, company: i32) -> (i32, String) {
    let u = uniq();
    let row: (i32, String) = sqlx::query_as(
        r#"INSERT INTO project (code, name, "screenshotPerHour", "projectTypeId", "startDate", status)
           VALUES ($1, $1, 0, (SELECT id FROM "projectType" ORDER BY id LIMIT 1), CURRENT_DATE, 1)
           RETURNING id, uuid::text"#,
    )
    .bind(format!("it-cu-{u}"))
    .fetch_one(app.db())
    .await
    .unwrap();
    app.exec(&format!(
        r#"INSERT INTO "projectCompany" ("projectId", "companyId", status) VALUES ({}, {company}, 1)"#,
        row.0
    ))
    .await;
    row
}

fn sign(secret: &str, body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body);
    hex::encode(mac.finalize().into_bytes())
}

fn task(id: &str, name: &str, updated: &str) -> Value {
    json!({
        "id": id, "name": name, "url": format!("https://app.clickup.com/t/{id}"),
        "status": {"status": "to do", "color": "#ccc", "type": "open"},
        "priority": {"priority": "urgent"},
        "due_date": "1767225600000", "date_updated": updated,
        "list": {"id": "L1", "name": "Sprint 1"},
        "assignees": [{"id": 42, "username": "budi", "email": "b@x.id"}],
        "tags": [{"name": "bug"}]
    })
}

#[tokio::test]
async fn it_int_clickup_workspaces_gates_and_failures() {
    let _g = SERIAL.lock().await;
    let Some((app, mock, fx, orig)) = setup().await else {
        return;
    };
    mock.on(
        "GET",
        "/team",
        200,
        json!({"teams": [{"id": "ws1", "name": "Acme", "color": "#f00", "avatar": null, "members": []}]}),
    );
    let res = app
        .get(&format!("{BASE}/workspaces"))
        .token(&fx.admin.token)
        .send()
        .await;
    res.assert_status(200).assert_ok();
    assert_eq!(
        res.text(),
        r##"{"success":true,"message":"Success","data":[{"id":"ws1","name":"Acme","color":"#f00","avatar":null}]}"##
    );
    let sent = mock.requests_to("GET", "/team");
    assert_eq!(sent.last().unwrap().header("authorization"), Some(TOKEN));

    // STAFF ditolak RolesGuard (401 "Access denied: insufficient permissions" di proyek ini);
    // `projects/linked` terbuka untuk STAFF.
    let r = app
        .get(&format!("{BASE}/workspaces"))
        .token(&fx.staff.token)
        .send()
        .await;
    r.assert_status(401);
    assert_eq!(
        r.json["message"],
        serde_json::json!("Access denied: insufficient permissions")
    );
    app.get(&format!("{BASE}/projects/linked"))
        .token(&fx.staff.token)
        .send()
        .await
        .assert_status(200);
    app.get(&format!("{BASE}/workspaces"))
        .send()
        .await
        .assert_status(401);

    // Galat ClickUp → pesan tetap.
    mock.on("GET", "/team/ws1/space", 500, json!({"err": "boom"}));
    let res = app
        .get(&format!("{BASE}/workspaces/ws1/spaces"))
        .token(&fx.admin.token)
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(
        res.json,
        json!({"success": false, "message": "Failed to fetch ClickUp spaces", "data": null})
    );

    // Folder: 401 → pesan khusus; 503 → pesan axios.
    mock.on(
        "GET",
        "/team/ws1/space",
        401,
        json!({"err": "Token invalid"}),
    );
    let res = app
        .get(&format!("{BASE}/folders"))
        .token(&fx.admin.token)
        .send()
        .await;
    assert_eq!(
        res.json["message"],
        "Invalid ClickUp API token. Please update your token."
    );
    mock.on("GET", "/team/ws1/space", 503, json!({}));
    let res = app
        .get(&format!("{BASE}/folders"))
        .token(&fx.admin.token)
        .send()
        .await;
    assert_eq!(res.json["message"], "Request failed with status code 503");

    // Lists: folderless + list di dalam folder.
    mock.on(
        "GET",
        "/space/S1/list",
        200,
        json!({"lists": [{"id": "L0", "name": "Loose", "task_count": 3}]}),
    );
    mock.on(
        "GET",
        "/space/S1/folder",
        200,
        json!({"folders": [{"id": "F1", "name": "Alpha"}]}),
    );
    mock.on(
        "GET",
        "/folder/F1/list",
        200,
        json!({"lists": [{"id": "L1", "name": "Sprint"}]}),
    );
    let res = app
        .get(&format!("{BASE}/spaces/S1/lists"))
        .token(&fx.admin.token)
        .send()
        .await;
    res.assert_ok();
    assert_eq!(
        res.text(),
        r#"{"success":true,"message":"Success","data":[{"id":"L0","name":"Loose","folder":null,"taskCount":3},{"id":"L1","name":"Sprint","folder":{"id":"F1","name":"Alpha"}}]}"#
    );

    // Tanpa token.
    set_creds(&app, fx.id, None, None).await;
    let res = app
        .get(&format!("{BASE}/workspaces"))
        .token(&fx.admin.token)
        .send()
        .await;
    assert_eq!(res.json["message"], "ClickUp token not configured");
    let res = app
        .get(&format!("{BASE}/members"))
        .token(&fx.admin.token)
        .send()
        .await;
    assert_eq!(res.json["message"], "ClickUp API token not configured");
    let status = mz_int_clickup::get_connection_status(&app.state, fx.admin.id).await;
    assert_eq!(
        serde_json::to_string(&status).unwrap(),
        r#"{"success":true,"message":"Success","data":{"connected":false,"workspaceId":null}}"#
    );
    // User tanpa role admin di company mana pun.
    let status = mz_int_clickup::get_connection_status(&app.state, fx.staff.id).await;
    assert_eq!(status.message, "Company not found");
    restore(&app, fx.id, &orig).await;
}

#[tokio::test]
async fn it_int_clickup_link_sync_status_webhook() {
    let _g = SERIAL.lock().await;
    let Some((app, mock, fx, orig)) = setup().await else {
        return;
    };
    let (project_id, project_uuid) = project(&app, fx.id).await;
    mock.on(
        "POST",
        "/team/ws1/webhook",
        200,
        json!({"id": "wh1", "webhook": {"id": "wh1", "secret": "sec1"}}),
    );
    let res = app
        .post(&format!("{BASE}/projects/link"))
        .token(&fx.admin.token)
        .json(&json!({
            "projectUuid": project_uuid, "clickupFolderId": "F1", "clickupFolderName": "Alpha",
            "clickupSpaceId": "S1", "clickupSpaceName": "Eng"
        }))
        .send()
        .await;
    res.assert_status(200).assert_ok();
    assert_eq!(res.data()["webhookRegistered"], true);
    assert_eq!(res.data()["projectUuid"], project_uuid.as_str());
    let hook = mock.requests_to("POST", "/team/ws1/webhook");
    let body = hook.last().unwrap().json();
    assert_eq!(
        body["endpoint"],
        "http://be.test/integrations/clickup/webhook"
    );
    assert_eq!(body["folder_id"], "F1");
    assert_eq!(body["events"].as_array().unwrap().len(), 8);

    // Proyek tak dikenal / bukan UUID.
    let res = app
        .post(&format!("{BASE}/projects/link"))
        .token(&fx.admin.token)
        .json(&json!({
            "projectUuid": "nope", "clickupFolderId": "F1", "clickupFolderName": "A",
            "clickupSpaceId": "S", "clickupSpaceName": "E"
        }))
        .send()
        .await;
    assert_eq!(res.json["message"], "Project not found");

    // Sync tasks: 2 task baru, lalu tidak berubah.
    mock.on(
        "GET",
        "/folder/F1/list",
        200,
        json!({"lists": [{"id": "L1", "name": "Sprint 1"}]}),
    );
    mock.on(
        "GET",
        "/list/L1/task",
        200,
        json!({"tasks": [task("t1", "Login bug", "100"), task("t2", "Signup", "200")], "last_page": true}),
    );
    let res = app
        .post(&format!("{BASE}/projects/{project_uuid}/sync-tasks"))
        .token(&fx.admin.token)
        .json(&json!({}))
        .send()
        .await;
    res.assert_ok();
    assert_eq!(
        res.text(),
        r#"{"success":true,"message":"Sync complete: 2 tasks created, 0 tasks updated","data":{"created":2,"updated":0,"unchanged":0,"total":2}}"#
    );
    let q = mock.requests_to("GET", "/list/L1/task");
    assert_eq!(
        q.last().unwrap().query,
        "archived=false&include_closed=true&subtasks=false&page=0"
    );
    let res = mz_int_clickup::sync_tasks(&app.state, fx.admin.id, &project_uuid, false, true).await;
    let v = serde_json::to_value(&res).unwrap();
    assert_eq!(
        v["data"],
        json!({"created": 0, "updated": 0, "unchanged": 2, "total": 2})
    );
    assert!(
        mock.requests_to("GET", "/list/L1/task")
            .last()
            .unwrap()
            .query
            .contains("include_closed=false&subtasks=true")
    );

    // Daftar task tersimpan.
    let res = app
        .get(&format!(
            "{BASE}/projects/{project_uuid}/tasks?search=LOGIN&limit=10"
        ))
        .token(&fx.admin.token)
        .send()
        .await;
    res.assert_ok();
    let d = res.data();
    assert_eq!(d["total"], 1);
    assert_eq!(d["totalPages"], 1);
    assert_eq!(d["limit"], 10);
    let t = &d["tasks"][0];
    assert_eq!(t["externalId"], "t1");
    assert_eq!(t["priority"], 1);
    assert_eq!(t["dueDate"], "2026-01-01T00:00:00.000Z");
    assert_eq!(t["externalMeta"]["listId"], "L1");
    assert_eq!(t["assignees"][0]["externalUserId"], "42");
    // `assignee`: username/email elemen array memuat teks, case-insensitive.
    for (q, want) in [
        ("budi", 2),
        ("BUD", 2),
        ("B@X.ID", 2),
        ("%25", 0),
        ("zzz", 0),
    ] {
        let res = app
            .get(&format!(
                "{BASE}/projects/{project_uuid}/tasks?assignee={q}"
            ))
            .token(&fx.admin.token)
            .send()
            .await;
        assert_eq!(res.data()["total"], want, "{q}");
    }
    let res = app
        .get(&format!("{BASE}/projects/{project_uuid}/tasks?page=0"))
        .token(&fx.admin.token)
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(res.json["message"], json!(["page must not be less than 1"]));

    let t1_uuid: String = app
        .scalar(&format!(
            r#"SELECT uuid::text FROM task WHERE "projectId" = {project_id} AND "externalId" = 't1'"#
        ))
        .await;

    // Status yang tersedia + ganti status.
    mock.on(
        "GET",
        "/list/L1",
        200,
        json!({"statuses": [{"status": "to do", "color": "#ccc", "type": "open"},
                            {"status": "In Progress", "color": "#00f", "type": "custom"}]}),
    );
    let res = app
        .get(&format!("{BASE}/tasks/{t1_uuid}/statuses"))
        .token(&fx.admin.token)
        .send()
        .await;
    assert_eq!(
        res.text(),
        r##"{"success":true,"message":"Statuses fetched successfully","data":[{"name":"to do","color":"#ccc","type":"open"},{"name":"In Progress","color":"#00f","type":"custom"}]}"##
    );
    mock.on("PUT", "/task/t1", 200, json!({}));
    mock.on("POST", "/task/t1/comment", 200, json!({"id": 1}));
    let res = app
        .patch(&format!("{BASE}/tasks/{t1_uuid}/status"))
        .token(&fx.admin.token)
        .json(&json!({"status": "in progress"}))
        .send()
        .await;
    res.assert_ok();
    assert_eq!(
        res.data(),
        &json!({"taskUuid": t1_uuid, "status": "In Progress"})
    );
    let put = mock.requests_to("PUT", "/task/t1");
    assert_eq!(put.last().unwrap().json(), json!({"status": "In Progress"}));
    let comment = mock.requests_to("POST", "/task/t1/comment");
    let text = comment.last().unwrap().json()["comment_text"].clone();
    assert!(
        text.as_str()
            .unwrap()
            .starts_with("Status changed to \"In Progress\" by "),
        "{text}"
    );
    let st: String = app
        .scalar(&format!("SELECT status FROM task WHERE uuid = '{t1_uuid}'"))
        .await;
    assert_eq!(st, "In Progress");
    let res = app
        .patch(&format!("{BASE}/tasks/{t1_uuid}/status"))
        .token(&fx.admin.token)
        .json(&json!({"status": "done"}))
        .send()
        .await;
    assert_eq!(
        res.json["message"],
        "Invalid status 'done' for this task. Valid statuses: to do, In Progress"
    );
    mock.on("PUT", "/task/t1", 404, json!({"err": "not found"}));
    let res = app
        .patch(&format!("{BASE}/tasks/{t1_uuid}/status"))
        .token(&fx.admin.token)
        .json(&json!({"status": "to do"}))
        .send()
        .await;
    assert_eq!(res.json["message"], "Task not found in ClickUp");
    app.patch(&format!("{BASE}/tasks/{t1_uuid}/status"))
        .token(&fx.admin.token)
        .json(&json!({}))
        .send()
        .await
        .assert_status(400);

    // Webhook: tanda tangan salah diabaikan; taskUpdated mengambil ulang task; taskDeleted menghapus.
    let body = json!({"webhook_id": "wh1", "event": "taskDeleted", "task_id": "t2"}).to_string();
    let res = app
        .post(&format!("{BASE}/webhook"))
        .raw("application/json", body.clone())
        .header("x-signature", "deadbeef")
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(res.text(), r#"{"received":true}"#);
    let n: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM task WHERE "projectId" = {project_id}"#
        ))
        .await;
    assert_eq!(n, 2);

    mock.on("GET", "/task/t1", 200, task("t1", "Login bug v2", "300"));
    let upd = json!({"webhook_id": "wh1", "event": "taskUpdated", "task_id": "t1"}).to_string();
    app.post(&format!("{BASE}/webhook"))
        .raw("application/json", upd.clone())
        .header("x-signature", &sign("sec1", upd.as_bytes()))
        .send()
        .await
        .assert_status(200);
    let name: String = app
        .scalar(&format!(
            r#"SELECT name FROM task WHERE "projectId" = {project_id} AND "externalId" = 't1'"#
        ))
        .await;
    assert_eq!(name, "Login bug v2");

    app.post(&format!("{BASE}/webhook"))
        .raw("text/plain", body.clone())
        .header("x-signature", &sign("sec1", body.as_bytes()))
        .send()
        .await
        .assert_status(200);
    let n: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM task WHERE "projectId" = {project_id}"#
        ))
        .await;
    assert_eq!(n, 1);
    // Payload `null` → TypeError tak tertangani di Node (500).
    app.post(&format!("{BASE}/webhook"))
        .raw("application/json", "null")
        .send()
        .await
        .assert_status(500);
    app.post(&format!("{BASE}/webhook"))
        .raw("application/json", "not json")
        .send()
        .await
        .assert_status(200);

    // Galat sync: folder 404, lalu task 500.
    mock.on("GET", "/folder/F1/list", 404, json!({}));
    let res = app
        .post(&format!("{BASE}/projects/{project_uuid}/sync-tasks"))
        .token(&fx.admin.token)
        .json(&json!({"includeClosed": true}))
        .send()
        .await;
    assert_eq!(
        res.json,
        json!({"success": false, "message": "ClickUp folder not found", "data": null})
    );
    mock.on(
        "GET",
        "/folder/F1/list",
        200,
        json!({"lists": [{"id": "L1"}]}),
    );
    mock.on("GET", "/list/L1/task", 500, json!({}));
    let res = app
        .post(&format!("{BASE}/projects/{project_uuid}/sync-tasks"))
        .token(&fx.admin.token)
        .json(&json!({}))
        .send()
        .await;
    assert_eq!(res.json["message"], "Request failed with status code 500");
    let res = app
        .post(&format!("{BASE}/projects/{project_uuid}/sync-tasks"))
        .token(&fx.admin.token)
        .json(&json!({"includeClosed": "yes"}))
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        json!(["includeClosed must be a boolean value"])
    );

    // Linked projects / folders.
    let res = app
        .get(&format!("{BASE}/linked-projects"))
        .token(&fx.admin.token)
        .send()
        .await;
    res.assert_ok();
    let linked = res.data().as_array().unwrap();
    let mine = linked
        .iter()
        .find(|m| m["projectUuid"] == project_uuid.as_str())
        .unwrap();
    assert_eq!(mine["webhookActive"], true);
    mock.on(
        "GET",
        "/team/ws1/space",
        200,
        json!({"spaces": [{"id": "S1", "name": "Eng"}, {"id": "S2", "name": "Ops"}]}),
    );
    mock.on(
        "GET",
        "/space/S1/folder",
        200,
        json!({"folders": [{"id": "F1", "name": "Alpha", "task_count": "5"}]}),
    );
    mock.on("GET", "/space/S2/folder", 500, json!({}));
    let res = app
        .get(&format!("{BASE}/folders"))
        .token(&fx.admin.token)
        .send()
        .await;
    res.assert_ok();
    let f = &res.data()[0];
    assert_eq!(f["space"], json!({"id": "S1", "name": "Eng"}));
    assert_eq!(f["taskCount"], "5");
    assert!(
        f["linkedProjects"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["uuid"] == project_uuid.as_str())
    );
    assert_eq!(res.data().as_array().unwrap().len(), 1);

    // Unlink: webhook dihapus di ClickUp, mapping status 0.
    mock.on("DELETE", "/webhook/wh1", 200, json!({}));
    let res = app
        .delete(&format!("{BASE}/projects/{project_uuid}/link"))
        .token(&fx.admin.token)
        .send()
        .await;
    assert_eq!(
        res.text(),
        r#"{"success":true,"message":"Project unlinked from ClickUp successfully","data":null}"#
    );
    assert_eq!(mock.requests_to("DELETE", "/webhook/wh1").len(), 1);
    let res = app
        .delete(&format!("{BASE}/projects/{project_uuid}/link"))
        .token(&fx.admin.token)
        .send()
        .await;
    assert_eq!(
        res.json["message"],
        "No active ClickUp link found for this project"
    );
    let res = mz_int_clickup::sync_tasks(&app.state, fx.admin.id, &project_uuid, true, false).await;
    assert_eq!(
        res.message,
        "Project not found or not linked to a ClickUp folder"
    );

    // Link ulang dengan pendaftaran webhook gagal → tetap tertaut, webhookRegistered false.
    mock.on("POST", "/team/ws1/webhook", 500, json!({}));
    let res = app
        .post(&format!("{BASE}/projects/link"))
        .token(&fx.admin.token)
        .json(&json!({
            "projectUuid": project_uuid, "clickupFolderId": "F9", "clickupFolderName": "Beta",
            "clickupSpaceId": "S1", "clickupSpaceName": "Eng"
        }))
        .send()
        .await;
    res.assert_ok();
    assert_eq!(res.data()["webhookRegistered"], false);
    assert_eq!(res.data()["clickupFolderId"], "F9");
    restore(&app, fx.id, &orig).await;
}

#[tokio::test]
async fn it_int_clickup_members_users_and_projects() {
    let _g = SERIAL.lock().await;
    let Some((app, mock, fx, orig)) = setup().await else {
        return;
    };
    let cu_id = format!("cu{}", &uniq()[..8]);
    mock.on(
        "GET",
        "/team",
        200,
        json!({"teams": [{"id": "ws1", "members": [
            {"user": {"id": cu_id, "username": "budi", "email": "b@x.id", "profilePicture": null}},
            {"user": {"id": 77, "username": "ani"}}
        ]}]}),
    );
    let res = app
        .post(&format!("{BASE}/users/link"))
        .token(&fx.admin.token)
        .json(
            &json!({"userUuid": fx.staff.uuid, "clickupUserId": cu_id, "clickupUsername": "budi"}),
        )
        .send()
        .await;
    res.assert_ok();
    assert_eq!(res.data()["userUuid"], fx.staff.uuid.as_str());
    assert_eq!(res.data()["clickupEmail"], Value::Null);

    // Member yang sama ke user lain → konflik.
    let res = app
        .post(&format!("{BASE}/users/link"))
        .token(&fx.admin.token)
        .json(&json!({"userUuid": fx.staff2.uuid, "clickupUserId": cu_id}))
        .send()
        .await;
    assert!(
        res.json["message"]
            .as_str()
            .unwrap()
            .starts_with("This ClickUp member is already linked to user '"),
        "{}",
        res.text()
    );
    let res = app
        .post(&format!("{BASE}/users/link"))
        .token(&fx.admin.token)
        .json(&json!({"userUuid": 5}))
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        json!([
            "userUuid must be a string",
            "clickupUserId must be a string"
        ])
    );

    let res = app
        .get(&format!("{BASE}/members"))
        .token(&fx.admin.token)
        .send()
        .await;
    res.assert_ok();
    let members = res.data().as_array().unwrap();
    assert_eq!(members[0]["linkedUser"]["uuid"], fx.staff.uuid.as_str());
    assert_eq!(members[1]["clickupUserId"], "77");
    assert_eq!(members[1]["clickupEmail"], Value::Null);
    assert_eq!(members[1]["linkedUser"], Value::Null);

    let res = app
        .get(&format!("{BASE}/linked-users"))
        .token(&fx.admin.token)
        .send()
        .await;
    assert!(
        res.data()
            .as_array()
            .unwrap()
            .iter()
            .any(|u| u["clickupUserId"] == cu_id.as_str()
                && u["userUsername"] == fx.staff.username.as_str())
    );

    let res = app
        .delete(&format!("{BASE}/users/{}/link", fx.staff.uuid))
        .token(&fx.admin.token)
        .send()
        .await;
    assert_eq!(
        res.json["message"],
        "User unlinked from ClickUp successfully"
    );
    let res = app
        .delete(&format!("{BASE}/users/{}/link", fx.staff.uuid))
        .token(&fx.admin.token)
        .send()
        .await;
    assert_eq!(
        res.json["message"],
        "No active ClickUp link found for this user"
    );

    // Workspace tidak ada di daftar tim.
    set_creds(&app, fx.id, Some(TOKEN), Some("other")).await;
    let res = app
        .get(&format!("{BASE}/members"))
        .token(&fx.admin.token)
        .send()
        .await;
    assert_eq!(
        res.json["message"],
        "Workspace not found. Please re-select your ClickUp workspace."
    );

    // sync-projects: buat lalu lewati.
    let list_id = format!("L{}", &uniq()[..10]);
    let body = json!({"lists": [{"clickupListId": list_id, "name": "From ClickUp"}]});
    let res = app
        .post(&format!("{BASE}/sync-projects"))
        .token(&fx.admin.token)
        .json(&body)
        .send()
        .await;
    assert_eq!(
        res.text(),
        r#"{"success":true,"message":"Synced: 1 created, 0 already exist","data":{"created":["From ClickUp"],"skipped":[]}}"#
    );
    let res = app
        .post(&format!("{BASE}/sync-projects"))
        .token(&fx.admin.token)
        .json(&body)
        .send()
        .await;
    assert_eq!(res.json["message"], "Synced: 0 created, 1 already exist");
    let code: String = app
        .scalar(&format!(
            r#"SELECT code FROM project WHERE "clickupListId" = '{list_id}'"#
        ))
        .await;
    assert_eq!(code, format!("CU-{list_id}"));
    let res = app
        .get(&format!("{BASE}/synced-projects"))
        .token(&fx.admin.token)
        .send()
        .await;
    res.assert_ok();
    let first = &res.data()[0];
    assert_eq!(first["clickupListId"], list_id.as_str());
    assert!(first["createdAt"].as_str().unwrap().ends_with('Z'));
    let res = app
        .post(&format!("{BASE}/sync-projects"))
        .token(&fx.admin.token)
        .json(&json!({"lists": [{"name": 1}]}))
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        json!([
            "lists.0.clickupListId must be a string",
            "lists.0.name must be a string"
        ])
    );
    restore(&app, fx.id, &orig).await;
}

/// `trackTaskTimeComment` (dipanggil `mz-timelines` sesudah timeline berubah).
#[tokio::test]
async fn it_int_clickup_track_time_comment() {
    let _g = SERIAL.lock().await;
    let Some((app, mock, fx, orig)) = setup().await else {
        return;
    };
    let st = app.state();
    let user = fx.super_admin.id;
    let (name, username): (Option<String>, Option<String>) =
        sqlx::query_as("SELECT name, username FROM users WHERE id = $1")
            .bind(user)
            .fetch_one(app.db())
            .await
            .unwrap();
    let actor = format!(
        "{}{}",
        name.unwrap_or_default(),
        username.map(|u| format!(" ({u})")).unwrap_or_default()
    );

    // Baru: POST komentar; `id` angka → String(id).
    mock.on("POST", "/task/T1/comment", 200, json!({ "id": 9001 }));
    let id = mz_int_clickup::track_task_time_comment(
        st,
        user,
        "T1",
        0,
        3_725_000,
        Some(" rapat "),
        None,
    )
    .await;
    assert_eq!(id.as_deref(), Some("9001"));
    let req = &mock.requests_to("POST", "/task/T1/comment")[0];
    assert_eq!(req.header("authorization"), Some(TOKEN));
    assert_eq!(
        req.json(),
        json!({
            "comment_text": format!("\u{23f1}\u{fe0f} {} tracked 1h 2m 5s on this task via Timemoz\nNote: rapat", actor.trim()),
            "notify_all": false
        })
    );

    // Sudah ada: PUT ke komentar yang sama, id dikembalikan apa adanya.
    mock.on("PUT", "/comment/9001", 200, json!({}));
    let id = mz_int_clickup::track_task_time_comment(st, user, "T1", 0, 60_000, None, Some("9001"))
        .await;
    assert_eq!(id.as_deref(), Some("9001"));
    assert_eq!(
        mock.requests_to("PUT", "/comment/9001")[0].json()["comment_text"],
        json!(format!(
            "\u{23f1}\u{fe0f} {} tracked 1m on this task via Timemoz",
            actor.trim()
        ))
    );

    // Galat ClickUp → None (Node `catch → null`).
    mock.on("POST", "/task/T2/comment", 500, json!({ "err": "boom" }));
    assert_eq!(
        mz_int_clickup::track_task_time_comment(st, user, "T2", 0, 60_000, None, None).await,
        None
    );

    // Tanpa token: tidak ada request keluar.
    set_creds(&app, fx.id, None, Some(WS)).await;
    mock.clear_log();
    assert_eq!(
        mz_int_clickup::track_task_time_comment(st, user, "T1", 0, 60_000, None, None).await,
        None
    );
    assert!(mock.requests().is_empty());

    restore(&app, fx.id, &orig).await;
}
