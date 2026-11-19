//! Uji integrasi endpoint data `mz-mcp-api` (`/mcp-api/{users,projects,tasks,trackings,reports,
//! calendar}`) terhadap Postgres nyata: isolasi company (audit C5), scoping role, validasi, bentuk
//! respons, cache laporan.
//! Jalankan: `MZ_TEST_DATABASE_URL=... MZ_TEST_REQUIRE_DB=1 cargo test -p mz-mcp-api --test it_mcp_api_data`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_arguments,
    clippy::type_complexity,
    clippy::useless_format
)]

use axum::Router;
use mz_core::AppState;
use mz_testkit::{Company, OtherCompany, TestApp, TestResponse};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// `trackings_endpoints` menambah user ke company fixture, sedangkan `users_endpoints`
/// membandingkan `meta.total` dengan isi daftar user company yang sama. Keduanya memegang
/// kunci ini supaya penambahan user tidak jatuh di antara query daftar dan query hitung.
static COMPANY_USERS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn build(state: &AppState) -> Router {
    mz_mcp_api::router(state).with_state(state.clone())
}

fn uniq() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

fn msg(r: &TestResponse) -> (u16, Value) {
    (r.status.as_u16(), r.json["message"].clone())
}

/// PAT untuk `user` (disimpan sebagai hash). -> token mentah.
async fn pat(app: &TestApp, user: i32) -> String {
    let plain = format!("mcp_pat_{}", uniq());
    app.exec(&format!(
        r#"INSERT INTO "mcpAccessToken" ("userId", name, "tokenHash", "tokenPrefix", status)
           VALUES ({user}, 'it', '{}', '{}', 1)"#,
        hex::encode(Sha256::digest(plain.as_bytes())),
        &plain[..12]
    ))
    .await;
    plain
}

struct Tokens {
    staff: String,
    staff2: String,
    manager: String,
    admin: String,
    other_admin: String,
}

async fn tokens(app: &TestApp, c: &Company, o: &OtherCompany) -> Tokens {
    Tokens {
        staff: pat(app, c.staff.id).await,
        staff2: pat(app, c.staff2.id).await,
        manager: pat(app, c.manager.id).await,
        admin: pat(app, c.admin.id).await,
        other_admin: pat(app, o.admin.id).await,
    }
}

async fn project_in(app: &TestApp, company: i32, tag: &str, status: i32) -> (i32, String) {
    let row: (i32, String) = sqlx::query_as(
        r#"INSERT INTO project (code, name, "screenshotPerHour", "totalHour", "projectTypeId", "startDate", status)
           VALUES ($1, $2, 0, 5, (SELECT id FROM "projectType" ORDER BY id LIMIT 1), '2026-01-02', $3)
           RETURNING id, uuid::text"#,
    )
    .bind(format!("MC-{tag}"))
    .bind(format!("McpProj {tag}"))
    .bind(status)
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

struct Seed {
    p1: (i32, String),
    p2: (i32, String),
    p3: (i32, String),
    client: (i32, String),
    t1: (i32, String),
    tag: String,
}

/// P1 (staff anggota, klien, budget 10 jam / terbakar 2 jam), P2 (company sama, staff BUKAN
/// anggota), P3 (company lain). Tracking staff di `year-06-15`: P1 1 jam (task T1) + 30 menit
/// (non-billable, tanpa task), P2 45 menit.
async fn seed(app: &TestApp, c: &Company, o: &OtherCompany, year: i32) -> Seed {
    let tag = uniq();
    let p1 = project_in(app, c.id, &format!("{tag}-1"), 1).await;
    let p2 = project_in(app, c.id, &format!("{tag}-2"), 1).await;
    let p3 = project_in(app, o.id, &format!("{tag}-3"), 1).await;
    app.exec(&format!(
        r#"INSERT INTO "projectUser" ("projectId", "userId", status) VALUES ({}, {}, 1);
           INSERT INTO "projectBudget" ("projectId", "budgetHours", "currentBurnedSeconds", "alertThreshold")
           VALUES ({}, 10, 7200, 80);"#,
        p1.0, c.staff.id, p1.0
    ))
    .await;
    let client: (i32, String) = sqlx::query_as(
        r#"INSERT INTO client (code, name, status) VALUES ($1, $2, 1) RETURNING id, uuid::text"#,
    )
    .bind(format!("CL-{tag}"))
    .bind(format!("Client {tag}"))
    .fetch_one(app.db())
    .await
    .unwrap();
    app.exec(&format!(
        r#"INSERT INTO "projectClient" ("projectId", "clientId", status) VALUES ({}, {}, 1)"#,
        p1.0, client.0
    ))
    .await;
    let t1: (i32, String) = sqlx::query_as(
        r#"INSERT INTO task ("projectId", "companyId", source, name, status, priority)
           VALUES ($1, $2, 'manual', $3, 'In Progress', 2) RETURNING id, uuid::text"#,
    )
    .bind(p1.0)
    .bind(c.id)
    .bind(format!("Task one {tag}"))
    .fetch_one(app.db())
    .await
    .unwrap();
    app.exec(&format!(
        r#"INSERT INTO task ("projectId", "companyId", source, name, status)
           VALUES ({}, {}, 'manual', 'Task two {tag}', 'Done');"#,
        p2.0, c.id
    ))
    .await;
    let tr: i32 = app
        .scalar(&format!(
            r#"INSERT INTO tracking ("takenDate", "userId", status) VALUES ('{year}-06-15', {}, 1) RETURNING id"#,
            c.staff.id
        ))
        .await;
    app.exec(&format!(
        r#"INSERT INTO "trackingTimeline" (note, "startAt", "endAt", "trackingId", "projectId", status, billable, "taskId")
           VALUES ('a', '{year}-06-15 08:00:00', '{year}-06-15 09:00:00', {tr}, {}, 1, true, {});
           INSERT INTO "trackingTimeline" (note, "startAt", "endAt", "trackingId", "projectId", status, billable)
           VALUES ('b', '{year}-06-15 10:00:00', '{year}-06-15 10:30:00', {tr}, {}, 1, false);
           INSERT INTO "trackingTimeline" (note, "startAt", "endAt", "trackingId", "projectId", status)
           VALUES ('c', '{year}-06-15 11:00:00', '{year}-06-15 11:45:00', {tr}, {}, 1);"#,
        p1.0, t1.0, p1.0, p2.0
    ))
    .await;
    Seed {
        p1,
        p2,
        p3,
        client,
        t1,
        tag,
    }
}

fn uuids(r: &TestResponse) -> Vec<String> {
    r.json["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["uuid"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn users_endpoints() {
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let _g = COMPANY_USERS.lock().await;
    let c = app.company().await;
    let o = app.other_company_admin().await;
    let t = tokens(&app, &c, &o).await;

    // me
    let r = app.get("/mcp-api/users/me").token(&t.staff).send().await;
    r.assert_status(200);
    assert_eq!(
        r.json["data"],
        json!({
            "uuid": c.staff.uuid,
            "name": r.json["data"]["name"],
            "username": c.staff.username,
            "roles": ["STAFF"],
            "company": { "uuid": c.uuid, "name": r.json["data"]["company"]["name"] }
        })
    );
    let r = app.get("/mcp-api/users/me").token(&t.manager).send().await;
    assert_eq!(r.json["data"]["roles"], json!(["MANAGER"]));

    // list: STAFF ditolak, MANAGER+ lolos
    let r = app.get("/mcp-api/users").token(&t.staff).send().await;
    assert_eq!(msg(&r), (403, json!("MANAGER+ role required")));
    let r = app.get("/mcp-api/users").token(&t.manager).send().await;
    r.assert_status(200);
    r.assert_no_sql_error();
    let ids = uuids(&r);
    for u in [&c.staff, &c.manager, &c.admin, &c.super_admin, &c.staff2] {
        assert!(ids.contains(&u.uuid), "{} hilang", u.username);
    }
    assert!(!ids.contains(&o.admin.uuid), "user company lain bocor");
    assert_eq!(r.json["meta"]["page"], json!(1));
    assert_eq!(r.json["meta"]["perPage"], json!(500));
    assert_eq!(r.json["meta"]["total"], json!(ids.len()));
    let me = r.json["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["uuid"] == json!(c.manager.uuid))
        .unwrap();
    assert_eq!(me["roles"], json!(["MANAGER"]));
    assert_eq!(me["username"], json!(c.manager.username));

    // keyword / role / pagination
    let r = app
        .get(&format!(
            "/mcp-api/users?keyword={}",
            c.staff.username.to_uppercase()
        ))
        .token(&t.admin)
        .send()
        .await;
    assert!(uuids(&r).contains(&c.staff.uuid));
    let r = app
        .get("/mcp-api/users?role=MANAGER")
        .token(&t.admin)
        .send()
        .await;
    r.assert_no_sql_error();
    let ids = uuids(&r);
    assert!(ids.contains(&c.manager.uuid) && !ids.contains(&c.staff.uuid));
    let r = app
        .get("/mcp-api/users?role=STAFF&perPage=1&page=2")
        .token(&t.admin)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json["meta"]["perPage"], json!(1));
    assert_eq!(r.json["data"].as_array().unwrap().len(), 1);
    assert!(r.json["meta"]["total"].as_i64().unwrap() >= 2);
    // validasi
    let r = app
        .get("/mcp-api/users?role=BOSS&perPage=0&page=x&keyword=a&keyword=b")
        .token(&t.admin)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        json!([
            "keyword must be a string",
            "role must be one of the following values: SYSTEM_ADMIN, SUPER_ADMIN, ADMIN, MANAGER, STAFF, GUEST",
            "page must not be less than 1",
            "page must be an integer number",
            "perPage must not be less than 1"
        ])
    );
    // company lain hanya melihat dirinya
    let r = app.get("/mcp-api/users").token(&t.other_admin).send().await;
    assert_eq!(uuids(&r), vec![o.admin.uuid.clone()]);
}

#[tokio::test]
async fn projects_and_tasks_endpoints() {
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let c = app.company().await;
    let o = app.other_company_admin().await;
    let t = tokens(&app, &c, &o).await;
    let s = seed(&app, &c, &o, 2001).await;
    let name = |n: i32| format!("McpProj {}-{n}", s.tag);

    // list: STAFF hanya project tempat ia anggota
    let r = app
        .get(&format!("/mcp-api/projects?search={}", s.tag))
        .token(&t.staff)
        .send()
        .await;
    r.assert_status(200);
    r.assert_no_sql_error();
    assert_eq!(uuids(&r), vec![s.p1.1.clone()]);
    let p = &r.json["data"][0];
    assert_eq!(p["name"], json!(name(1)));
    assert_eq!(p["status"], json!("active"));
    assert_eq!(p["billable"], json!(true));
    assert_eq!(p["totalHourTracked"], json!(5));
    assert_eq!(
        p["client"],
        json!({ "uuid": s.client.1, "name": format!("Client {}", s.tag) })
    );
    assert_eq!(p["company"]["uuid"], json!(c.uuid));
    assert_eq!(
        r.json["meta"],
        json!({ "page": 1, "perPage": 500, "total": 1 })
    );
    // MANAGER: kedua project company, bukan P3
    let r = app
        .get(&format!("/mcp-api/projects?search={}", s.tag))
        .token(&t.manager)
        .send()
        .await;
    let mut got = uuids(&r);
    got.sort();
    let mut want = vec![s.p1.1.clone(), s.p2.1.clone()];
    want.sort();
    assert_eq!(got, want);
    // clientUuid (tak dikenal diabaikan), archived, validasi
    let r = app
        .get(&format!(
            "/mcp-api/projects?search={}&clientUuid={}",
            s.tag, s.client.1
        ))
        .token(&t.manager)
        .send()
        .await;
    assert_eq!(uuids(&r), vec![s.p1.1.clone()]);
    let r = app
        .get(&format!(
            "/mcp-api/projects?search={}&clientUuid={}",
            s.tag,
            uuid::Uuid::new_v4()
        ))
        .token(&t.manager)
        .send()
        .await;
    assert_eq!(uuids(&r).len(), 2);
    let r = app
        .get(&format!(
            "/mcp-api/projects?search={}&status=archived",
            s.tag
        ))
        .token(&t.manager)
        .send()
        .await;
    assert!(uuids(&r).is_empty());
    let r = app
        .get("/mcp-api/projects?status=x&clientUuid=y")
        .token(&t.manager)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        json!([
            "status must be one of the following values: active, archived",
            "clientUuid must be a UUID"
        ])
    );

    // detail
    let r = app
        .get(&format!("/mcp-api/projects/{}", s.p1.1))
        .token(&t.staff)
        .send()
        .await;
    r.assert_status(200);
    r.assert_no_sql_error();
    let d = &r.json["data"];
    assert_eq!(d["uuid"], json!(s.p1.1));
    assert_eq!(d["startDate"], json!("2026-01-02T00:00:00.000Z"));
    assert_eq!(d["totalHourTracked"], json!(5));
    assert_eq!(
        d["budget"],
        json!({ "totalHour": 10, "usedHour": 2, "alertThreshold": 80 })
    );
    assert_eq!(
        d["members"],
        json!([{ "uuid": c.staff.uuid, "name": d["members"][0]["name"] }])
    );
    assert_eq!(d["tasksCount"], json!({ "total": 1, "open": 1, "done": 0 }));
    assert_eq!(d["client"]["uuid"], json!(s.client.1));
    // STAFF non-anggota -> 404; MANAGER -> ok; company lain -> 404 (dan sebaliknya)
    let r = app
        .get(&format!("/mcp-api/projects/{}", s.p2.1))
        .token(&t.staff)
        .send()
        .await;
    assert_eq!(msg(&r), (404, json!("Project not found")));
    let r = app
        .get(&format!("/mcp-api/projects/{}", s.p2.1))
        .token(&t.manager)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json["data"]["budget"], Value::Null);
    assert_eq!(r.json["data"]["client"], Value::Null);
    assert_eq!(
        r.json["data"]["tasksCount"],
        json!({ "total": 1, "open": 0, "done": 1 })
    );
    let r = app
        .get(&format!("/mcp-api/projects/{}", s.p1.1))
        .token(&t.other_admin)
        .send()
        .await;
    assert_eq!(r.status.as_u16(), 404);
    let r = app
        .get(&format!("/mcp-api/projects/{}", s.p3.1))
        .token(&t.admin)
        .send()
        .await;
    assert_eq!(r.status.as_u16(), 404);
    app.get(&format!("/mcp-api/projects/{}", s.p3.1))
        .token(&t.other_admin)
        .send()
        .await
        .assert_status(200);
    app.get("/mcp-api/projects/not-a-uuid")
        .token(&t.manager)
        .send()
        .await
        .assert_status(400);

    // tasks: STAFF hanya project anggota
    let r = app
        .get(&format!("/mcp-api/tasks?keyword={}", s.tag))
        .token(&t.staff)
        .send()
        .await;
    r.assert_status(200);
    r.assert_no_sql_error();
    assert_eq!(uuids(&r), vec![s.t1.1.clone()]);
    let task = &r.json["data"][0];
    assert_eq!(task["status"], json!("In Progress"));
    assert_eq!(task["priority"], json!(2));
    assert_eq!(task["source"], json!("manual"));
    assert_eq!(task["dueDate"], Value::Null);
    assert_eq!(task["externalUrl"], Value::Null);
    assert_eq!(task["project"], json!({ "uuid": s.p1.1, "name": name(1) }));
    let r = app
        .get(&format!("/mcp-api/tasks?keyword={}", s.tag))
        .token(&t.manager)
        .send()
        .await;
    assert_eq!(uuids(&r).len(), 2);
    assert_eq!(r.json["meta"]["total"], json!(2));
    let r = app
        .get(&format!("/mcp-api/tasks?keyword={}&status=DONE", s.tag))
        .token(&t.manager)
        .send()
        .await;
    assert_eq!(uuids(&r).len(), 1);
    let r = app
        .get(&format!("/mcp-api/tasks?projectUuid={}", s.p3.1))
        .token(&t.manager)
        .send()
        .await;
    assert_eq!(msg(&r), (404, json!("Project not found")));
    let r = app
        .get(&format!("/mcp-api/tasks?projectUuid={}", s.p2.1))
        .token(&t.staff)
        .send()
        .await;
    r.assert_status(200);
    assert!(uuids(&r).is_empty());
    let r = app
        .get("/mcp-api/tasks?projectUuid=x&assigneeUuid=y&page=0")
        .token(&t.manager)
        .send()
        .await;
    assert_eq!(
        r.json["message"],
        json!([
            "projectUuid must be a UUID",
            "assigneeUuid must be a UUID",
            "page must not be less than 1"
        ])
    );
}

#[tokio::test]
async fn trackings_endpoints() {
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let _g = COMPANY_USERS.lock().await;
    let c = app.company().await;
    let o = app.other_company_admin().await;
    let t = tokens(&app, &c, &o).await;
    let s = seed(&app, &c, &o, 2002).await;
    let q = "dateFrom=2002-06-15&dateTo=2002-06-15";

    // summary per hari: STAFF = dirinya (3 timeline = 8100 dtk)
    let r = app
        .get(&format!("/mcp-api/trackings/summary?{q}"))
        .token(&t.staff)
        .send()
        .await;
    r.assert_status(200);
    r.assert_no_sql_error();
    assert_eq!(
        r.json,
        json!({
            "data": [{ "date": "2002-06-15", "totalSeconds": 8100, "totalHours": 2.25 }],
            "meta": { "totalSeconds": 8100, "totalHours": 2.25 }
        })
    );
    // filter project
    let r = app
        .get(&format!(
            "/mcp-api/trackings/summary?{q}&projectUuid={}",
            s.p1.1
        ))
        .token(&t.staff)
        .send()
        .await;
    assert_eq!(
        r.json["meta"],
        json!({ "totalSeconds": 5400, "totalHours": 1.5 })
    );
    // project company lain / user company lain -> 404
    let r = app
        .get(&format!(
            "/mcp-api/trackings/summary?{q}&projectUuid={}",
            s.p3.1
        ))
        .token(&t.manager)
        .send()
        .await;
    assert_eq!(msg(&r), (404, json!("Project not found")));
    let r = app
        .get(&format!(
            "/mcp-api/trackings/summary?{q}&userUuid={}",
            o.admin.uuid
        ))
        .token(&t.manager)
        .send()
        .await;
    assert_eq!(msg(&r), (404, json!("User not found")));
    // STAFF lain user -> 403; MANAGER: user tertentu & seluruh company
    let r = app
        .get(&format!(
            "/mcp-api/trackings/summary?{q}&userUuid={}",
            c.staff.uuid
        ))
        .token(&t.staff2)
        .send()
        .await;
    assert_eq!(
        msg(&r),
        (403, json!("STAFF can only query their own trackings"))
    );
    let r = app
        .get(&format!(
            "/mcp-api/trackings/summary?{q}&userUuid={}",
            c.staff.uuid
        ))
        .token(&t.manager)
        .send()
        .await;
    assert_eq!(r.json["meta"]["totalSeconds"], json!(8100));
    let r = app
        .get(&format!("/mcp-api/trackings/summary?{q}"))
        .token(&t.manager)
        .send()
        .await;
    assert_eq!(r.json["meta"]["totalSeconds"], json!(8100));
    // company lain tidak melihat data
    let r = app
        .get(&format!("/mcp-api/trackings/summary?{q}"))
        .token(&t.other_admin)
        .send()
        .await;
    assert_eq!(r.json["data"], json!([]));
    assert_eq!(
        r.json["meta"],
        json!({ "totalSeconds": 0, "totalHours": 0 })
    );
    // groupBy bukan day -> baris mentah
    let r = app
        .get(&format!("/mcp-api/trackings/summary?{q}&groupBy=project"))
        .token(&t.staff)
        .send()
        .await;
    r.assert_status(200);
    let rows = r.json["data"].as_array().unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0]["startAt"], json!("2002-06-15T08:00:00.000Z"));
    assert_eq!(rows[0]["projectId"], json!(s.p1.0));
    assert_eq!(rows[0]["taskId"], json!(s.t1.0));
    assert_eq!(rows[1]["taskId"], Value::Null);
    assert_eq!(rows[0]["tracking"]["userId"], json!(c.staff.id));
    assert_eq!(
        rows[0]["tracking"]["takenDate"],
        json!("2002-06-15T00:00:00.000Z")
    );
    assert!(r.json.get("meta").is_none());
    // aturan rentang
    let r = app
        .get("/mcp-api/trackings/summary?dateFrom=2002-06-16&dateTo=2002-06-15")
        .token(&t.staff)
        .send()
        .await;
    assert_eq!(msg(&r), (400, json!("dateFrom must be <= dateTo")));
    let r = app
        .get("/mcp-api/trackings/summary?dateFrom=2002-01-01&dateTo=2003-06-15")
        .token(&t.staff)
        .send()
        .await;
    assert_eq!(msg(&r), (400, json!("Date range max 366 days")));
    let r = app
        .get("/mcp-api/trackings/summary?groupBy=x&userUuid=y")
        .token(&t.staff)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        json!([
            "dateFrom must be a valid ISO 8601 date string",
            "dateFrom should not be empty",
            "dateTo must be a valid ISO 8601 date string",
            "dateTo should not be empty",
            "userUuid must be a UUID",
            "groupBy must be one of the following values: user, project, task, day"
        ])
    );

    // recent: user baru (staff fixture punya riwayat template yang lebih baru)
    let fresh: i32 = app
        .scalar(&format!(
            r#"INSERT INTO users (username, password, name, status) VALUES ('mcprc{}', 'x', 'Fresh', 1) RETURNING id"#,
            uniq()
        ))
        .await;
    app.exec(&format!(
        r#"INSERT INTO "userCompany" ("userId", "companyId", status, "idleTimeInSecond", "isDefault")
           VALUES ({fresh}, {}, 1, 300, true);
           INSERT INTO "userAccessLevel" ("userId", "accessLevelId", "companyId", status)
           VALUES ({fresh}, (SELECT id FROM "accessLevel" WHERE code = 'STAFF' AND "isSystem" ORDER BY id LIMIT 1), {}, 1);"#,
        c.id, c.id
    ))
    .await;
    let fresh_uuid: String = app
        .scalar(&format!("SELECT uuid::text FROM users WHERE id = {fresh}"))
        .await;
    let ftr: i32 = app
        .scalar(&format!(
            r#"INSERT INTO tracking ("takenDate", "userId", status) VALUES ('2002-06-15', {fresh}, 1) RETURNING id"#
        ))
        .await;
    app.exec(&format!(
        r#"INSERT INTO "trackingTimeline" (note, "startAt", "endAt", "trackingId", "projectId", status, "taskId")
           VALUES ('a', '2002-06-15 08:00:00', '2002-06-15 09:00:00', {ftr}, {}, 1, {});
           INSERT INTO "trackingTimeline" (note, "startAt", "endAt", "trackingId", "projectId", status)
           VALUES ('b', '2002-06-15 10:00:00', '2002-06-15 10:30:00', {ftr}, {}, 1);
           INSERT INTO "trackingTimeline" (note, "startAt", "endAt", "trackingId", "projectId", status)
           VALUES ('c', '2002-06-15 11:00:00', '2002-06-15 11:45:00', {ftr}, {}, 1);
           INSERT INTO "trackingTimeline" (note, "startAt", "endAt", "trackingId", "projectId", status)
           VALUES ('z-other-company', '2002-06-15 12:00:00', '2002-06-15 12:10:00', {ftr}, {}, 1);"#,
        s.p1.0, s.t1.0, s.p1.0, s.p2.0, s.p3.0
    ))
    .await;
    let fresh_pat = pat(&app, fresh).await;
    let r = app
        .get("/mcp-api/trackings/recent?limit=50")
        .token(&fresh_pat)
        .send()
        .await;
    r.assert_status(200);
    r.assert_no_sql_error();
    let rows = r.json["data"].as_array().unwrap();
    // timeline di project company lain tidak ikut (audit C5)
    assert_eq!(rows.len(), 3);
    let mine: Vec<&Value> = rows.iter().collect();
    assert_eq!(mine[0]["note"], json!("c")); // terbaru dulu
    assert_eq!(mine[0]["takenDate"], json!("2002-06-15"));
    assert_eq!(mine[0]["durationSeconds"], json!(2700));
    assert_eq!(mine[0]["startAt"], json!("2002-06-15T11:00:00.000Z"));
    assert_eq!(mine[0]["task"], Value::Null);
    assert_eq!(mine[0]["user"]["uuid"], json!(fresh_uuid));
    assert_eq!(mine[0]["project"]["uuid"], json!(s.p2.1));
    assert_eq!(mine[2]["task"]["uuid"], json!(s.t1.1));
    // manager membaca user tertentu di company sama
    let r = app
        .get(&format!("/mcp-api/trackings/recent?userUuid={fresh_uuid}"))
        .token(&t.manager)
        .send()
        .await;
    assert_eq!(r.json["data"].as_array().unwrap().len(), 3);
    let r = app
        .get("/mcp-api/trackings/recent?limit=1")
        .token(&fresh_pat)
        .send()
        .await;
    assert_eq!(r.json["data"].as_array().unwrap().len(), 1);
    let r = app
        .get("/mcp-api/trackings/recent?limit=101")
        .token(&t.staff)
        .send()
        .await;
    assert_eq!(
        r.json["message"],
        json!(["limit must not be greater than 100"])
    );
    let r = app
        .get(&format!(
            "/mcp-api/trackings/recent?userUuid={}",
            c.staff.uuid
        ))
        .token(&t.staff2)
        .send()
        .await;
    assert_eq!(r.status.as_u16(), 403);
    let r = app
        .get(&format!(
            "/mcp-api/trackings/recent?userUuid={}",
            o.admin.uuid
        ))
        .token(&t.admin)
        .send()
        .await;
    assert_eq!(msg(&r), (404, json!("User not found")));
}

#[tokio::test]
async fn reports_endpoints() {
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let c = app.company().await;
    let o = app.other_company_admin().await;
    let t = tokens(&app, &c, &o).await;
    let s = seed(&app, &c, &o, 2003).await;
    let q = "dateFrom=2003-06-01&dateTo=2003-06-30";

    // project report
    let url = format!("/mcp-api/reports/project/{}?{q}", s.p1.1);
    let r = app.get(&url).token(&t.staff).send().await;
    r.assert_status(200);
    r.assert_no_sql_error();
    let d = &r.json["data"];
    assert_eq!(d["project"]["uuid"], json!(s.p1.1));
    assert_eq!(
        d["period"],
        json!({ "from": "2003-06-01", "to": "2003-06-30" })
    );
    assert_eq!(
        d["summary"],
        json!({ "totalHours": 1.5, "totalEntries": 2, "uniqueContributors": 1, "billableHours": 1 })
    );
    assert_eq!(
        d["budget"],
        json!({ "totalHour": 10, "usedHour": 2, "remainingHour": 8, "usagePercent": 20 })
    );
    assert_eq!(d["byUser"][0]["hours"], json!(1.5));
    assert_eq!(d["byUser"][0]["entries"], json!(2));
    assert_eq!(d["byUser"][0]["user"]["uuid"], json!(c.staff.uuid));
    assert_eq!(d["byTask"][0]["task"]["uuid"], json!(s.t1.1));
    assert_eq!(d["byTask"][0]["hours"], json!(1));
    assert_eq!(d["byTask"][1], json!({ "task": null, "hours": 0.5 }));
    assert_eq!(d["byDay"], json!([{ "date": "2003-06-15", "hours": 1.5 }]));
    // cache Redis (kunci Node yang sama), hasil identik
    let key = format!(
        "mcp:report:project:{}:{}:s:{}:2003-06-01:2003-06-30",
        c.staff.id, c.id, s.p1.1
    );
    use fred::prelude::KeysInterface;
    let cached: Option<String> = app.state().redis.next().get(&key).await.unwrap();
    assert!(cached.is_some(), "cache laporan tidak tertulis: {key}");
    let ttl: i64 = app.state().redis.next().pttl(&key).await.unwrap();
    assert!(ttl > 0 && ttl <= 300_000, "ttl {ttl}");
    let r2 = app.get(&url).token(&t.staff).send().await;
    assert_eq!(r2.json, r.json);
    // otorisasi project
    let r = app
        .get(&format!("/mcp-api/reports/project/{}?{q}", s.p2.1))
        .token(&t.staff)
        .send()
        .await;
    assert_eq!(msg(&r), (404, json!("Project not found")));
    let r = app
        .get(&format!("/mcp-api/reports/project/{}?{q}", s.p2.1))
        .token(&t.manager)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json["data"]["budget"], Value::Null);
    assert_eq!(r.json["data"]["summary"]["totalHours"], json!(0.75));
    let r = app
        .get(&format!("/mcp-api/reports/project/{}?{q}", s.p1.1))
        .token(&t.other_admin)
        .send()
        .await;
    assert_eq!(r.status.as_u16(), 404);
    app.get("/mcp-api/reports/project/zzz")
        .token(&t.manager)
        .send()
        .await
        .assert_status(400);
    // default periode (30 hari terakhir) tanpa data
    let r = app
        .get(&format!("/mcp-api/reports/project/{}", s.p1.1))
        .token(&t.manager)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json["data"]["summary"]["totalEntries"], json!(0));
    assert_eq!(r.json["data"]["byTask"], json!([]));
    let r = app
        .get(&format!("/mcp-api/reports/project/{}?dateFrom=bad", s.p1.1))
        .token(&t.manager)
        .send()
        .await;
    assert_eq!(
        r.json["message"],
        json!(["dateFrom must be a valid ISO 8601 date string"])
    );

    // team report: MANAGER+
    let r = app
        .get(&format!("/mcp-api/reports/team?{q}"))
        .token(&t.staff)
        .send()
        .await;
    assert_eq!(msg(&r), (403, json!("MANAGER+ role required")));
    let r = app
        .get(&format!("/mcp-api/reports/team?{q}"))
        .token(&t.manager)
        .send()
        .await;
    r.assert_status(200);
    r.assert_no_sql_error();
    let users = r.json["data"]["users"].as_array().unwrap();
    assert_eq!(users.len(), 1);
    assert_eq!(users[0]["user"]["uuid"], json!(c.staff.uuid));
    assert_eq!(users[0]["totalHours"], json!(2.25));
    assert_eq!(users[0]["projectsCount"], json!(2));
    assert_eq!(users[0]["topProjects"][0]["hours"], json!(1.5));
    assert_eq!(users[0]["topProjects"][1]["hours"], json!(0.75));
    assert_eq!(
        r.json["data"]["period"],
        json!({ "from": "2003-06-01", "to": "2003-06-30" })
    );
    // userUuids: user company lain diabaikan; hanya yang valid & anggota
    let r = app
        .get(&format!(
            "/mcp-api/reports/team?{q}&userUuids={}&userUuids={}",
            o.admin.uuid, c.staff.uuid
        ))
        .token(&t.admin)
        .send()
        .await;
    assert_eq!(r.json["data"]["users"].as_array().unwrap().len(), 1);
    let r = app
        .get(&format!(
            "/mcp-api/reports/team?{q}&userUuids[]={}",
            o.admin.uuid
        ))
        .token(&t.admin)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json["data"]["users"], json!([]));
    let r = app
        .get(&format!("/mcp-api/reports/team?{q}"))
        .token(&t.other_admin)
        .send()
        .await;
    assert_eq!(r.json["data"]["users"], json!([]));
    // validasi
    let r = app
        .get("/mcp-api/reports/team?userUuids[]=nope")
        .token(&t.manager)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        json!([
            "dateFrom must be a valid ISO 8601 date string",
            "dateTo must be a valid ISO 8601 date string",
            "each value in userUuids must be a UUID"
        ])
    );
    let r = app
        .get(&format!("/mcp-api/reports/team?{q}&userUuids=abc"))
        .token(&t.manager)
        .send()
        .await;
    assert_eq!(r.status.as_u16(), 500); // perilaku Node (Prisma menolak `in: "abc"`), lihat deviasi
}

#[tokio::test]
async fn calendar_events() {
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let c = app.company().await;
    let o = app.other_company_admin().await;
    let t = tokens(&app, &c, &o).await;
    let tag = uniq();
    let ins = |user: &str, title: &str, status: &str, day: u32, meta: &str| {
        format!(
            r#"INSERT INTO "googleCalendarEvent" ("companyId", "userId", "calendarId", title, description, location, "startAt", "endAt", "isAllDay", status, "htmlLink", "rawMeta")
               VALUES ({}, {user}, 'cal', '{title} {tag}', 'd', 'Room', '2004-03-{day:02} 09:00:00', '2004-03-{day:02} 10:00:00', false, '{status}', 'http://x', {meta});"#,
            c.id
        )
    };
    app.exec(&format!(
        "{}{}{}{}{}",
        ins("NULL", "Company", "confirmed", 10, r#"'{"attendees":[{"email":"a@b.c","displayName":"A","responseStatus":"accepted"},{"email":"z@b.c"}]}'::jsonb"#),
        ins(&c.staff.id.to_string(), "Mine", "confirmed", 11, "NULL"),
        ins(&c.staff2.id.to_string(), "Theirs", "confirmed", 12, "NULL"),
        ins("NULL", "Cancelled", "cancelled", 13, "NULL"),
        ins(&c.staff.id.to_string(), "Mine2", "tentative", 14, "'{}'::jsonb"),
    ))
    .await;
    let q = "startDate=2004-03-01&endDate=2004-03-31";

    let r = app
        .get(&format!("/mcp-api/calendar/events?{q}"))
        .token(&t.staff)
        .send()
        .await;
    r.assert_status(200);
    r.assert_no_sql_error();
    let titles: Vec<&str> = r.json["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["title"].as_str().unwrap())
        .collect();
    assert_eq!(
        titles,
        vec![
            format!("Company {tag}"),
            format!("Mine {tag}"),
            format!("Mine2 {tag}")
        ]
    );
    let e = &r.json["data"][0];
    assert_eq!(e["source"], json!("company"));
    assert_eq!(e["description"], json!("d"));
    assert_eq!(e["location"], json!("Room"));
    assert_eq!(e["startAt"], json!("2004-03-10T09:00:00.000Z"));
    assert_eq!(e["isAllDay"], json!(false));
    assert_eq!(e["status"], json!("confirmed"));
    assert_eq!(e["htmlLink"], json!("http://x"));
    assert!(e["syncedAt"].as_str().unwrap().ends_with('Z'));
    assert_eq!(
        e["attendees"],
        json!([
            { "email": "a@b.c", "name": "A", "responseStatus": "accepted" },
            { "email": "z@b.c", "name": null, "responseStatus": null }
        ])
    );
    assert_eq!(r.json["data"][1]["source"], json!("personal"));
    assert_eq!(r.json["data"][1]["attendees"], json!([]));
    assert_eq!(r.json["meta"], json!({ "total": 3, "skip": 0, "take": 20 }));
    // source + paginasi
    let r = app
        .get(&format!("/mcp-api/calendar/events?{q}&source=personal"))
        .token(&t.staff)
        .send()
        .await;
    assert_eq!(r.json["meta"]["total"], json!(2));
    let r = app
        .get(&format!(
            "/mcp-api/calendar/events?{q}&source=company&take=1&skip=0"
        ))
        .token(&t.staff2)
        .send()
        .await;
    assert_eq!(r.json["data"].as_array().unwrap().len(), 1);
    assert_eq!(r.json["meta"], json!({ "total": 1, "skip": 0, "take": 1 }));
    let r = app
        .get(&format!("/mcp-api/calendar/events?{q}&skip=1&take=1"))
        .token(&t.staff)
        .send()
        .await;
    assert_eq!(r.json["data"][0]["title"], json!(format!("Mine {tag}")));
    // company lain tidak melihat apa pun
    let r = app
        .get(&format!("/mcp-api/calendar/events?{q}"))
        .token(&t.other_admin)
        .send()
        .await;
    assert_eq!(r.json["meta"]["total"], json!(0));
    // validasi & rentang
    let r = app
        .get("/mcp-api/calendar/events?source=x&take=0&skip=-1")
        .token(&t.staff)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        json!([
            "startDate must be a valid ISO 8601 date string",
            "startDate should not be empty",
            "endDate must be a valid ISO 8601 date string",
            "endDate should not be empty",
            "source must be one of the following values: company, personal, all",
            "skip must not be less than 0",
            "take must not be less than 1"
        ])
    );
    let r = app
        .get("/mcp-api/calendar/events?startDate=2004-03-31&endDate=2004-03-01")
        .token(&t.staff)
        .send()
        .await;
    assert_eq!(msg(&r), (400, json!("startDate must be before endDate")));
    let r = app
        .get("/mcp-api/calendar/events?startDate=2003-01-01&endDate=2004-03-01")
        .token(&t.staff)
        .send()
        .await;
    assert_eq!(msg(&r), (400, json!("Date range max 366 days")));
}
