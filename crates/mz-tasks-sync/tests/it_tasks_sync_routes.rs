//! Uji integrasi `POST /admin/sync/trigger`, `POST /admin/manager-summary/test`, dan
//! `worklimit::check_work_limits` terhadap Postgres nyata. Mailtrap dimatikan (`MAILTRAP_TOKEN` kosong
//! -> tidak ada jaringan). Jalankan: `MZ_TEST_DATABASE_URL=... MZ_TEST_REQUIRE_DB=1 cargo test -p mz-tasks-sync --test it_tasks_sync_routes`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use axum::Router;
use chrono::NaiveDate;
use mz_core::AppState;
use mz_testkit::TestApp;
use serde_json::json;
use tokio::sync::Mutex;

static SERIAL: Mutex<()> = Mutex::const_new(());

fn build(state: &AppState) -> Router {
    mz_tasks_sync::router(state).with_state(state.clone())
}

async fn app() -> Option<TestApp> {
    TestApp::with_vars(build, &[("MAILTRAP_TOKEN", "")]).await
}

fn uniq() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

async fn operator(app: &TestApp, status: i32) -> (i32, String) {
    let u = uniq();
    let id: i32 = app
        .scalar(&format!(
            r#"INSERT INTO operators (username, password, name, status)
               VALUES ('op{u}', 'x', 'IT Operator', {status}) RETURNING id"#
        ))
        .await;
    (
        id,
        app.token_with(json!({ "username": format!("op{u}"), "sub": id })),
    )
}

async fn audit_count(app: &TestApp, op: i32, action: &str) -> i64 {
    sqlx::query_scalar(
        r#"SELECT COUNT(*) FROM "operatorAuditLog" WHERE "operatorId" = $1 AND action = $2"#,
    )
    .bind(op)
    .bind(action)
    .fetch_one(app.db())
    .await
    .unwrap()
}

#[tokio::test]
async fn gates_and_validation() {
    let _g = SERIAL.lock().await;
    let Some(app) = app().await else { return };
    let c = app.company().await;
    let (op, token) = operator(&app, 1).await;
    let (_, inactive) = operator(&app, 0).await;

    for path in ["/admin/sync/trigger", "/admin/manager-summary/test"] {
        app.post(path).send().await.assert_status(401);
        app.post(path)
            .user(c.admin.id)
            .send()
            .await
            .assert_status(401);
        app.post(path)
            .token(&inactive)
            .send()
            .await
            .assert_status(401);
    }

    let r = app
        .post("/admin/sync/trigger")
        .token(&token)
        .json(&json!({}))
        .send()
        .await;
    r.assert_status(400).assert_no_sql_error();
    assert_eq!(
        r.json["message"],
        json!([
            "date must be a valid ISO 8601 date string",
            "date must be YYYY-MM-DD"
        ])
    );
    let r = app
        .post("/admin/sync/trigger")
        .token(&token)
        .json(&json!({"date":"2026-11-10","extra":1}))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        json!(["property extra should not exist"])
    );

    let r = app
        .post("/admin/manager-summary/test")
        .token(&token)
        .json(&json!({"companyId":0,"type":"yearly"}))
        .send()
        .await;
    r.assert_status(400).assert_no_sql_error();
    assert_eq!(
        r.json["message"],
        json!([
            "companyId must not be less than 1",
            "type must be one of the following values: daily, weekly"
        ])
    );
    assert_eq!(audit_count(&app, op, "sync.trigger").await, 0);
}

#[tokio::test]
async fn sync_trigger_aggregates_and_audits() {
    let _g = SERIAL.lock().await;
    let Some(app) = app().await else { return };
    let c = app.company().await;
    let (op, token) = operator(&app, 1).await;
    let u = uniq();
    let day = "2001-04-10";
    let project: i32 = app
        .scalar(&format!(
            r#"INSERT INTO project (code, name, "screenshotPerHour", "projectTypeId", "startDate", status)
               VALUES ('ts{u}', 'ts{u}', 3, (SELECT id FROM "projectType" ORDER BY id LIMIT 1), CURRENT_DATE, 1)
               RETURNING id"#
        ))
        .await;
    app.exec(&format!(
        r#"INSERT INTO "projectCompany" ("projectId", "companyId", status) VALUES ({project}, {}, 1)"#,
        c.id
    ))
    .await;
    let t: i32 = app
        .scalar(&format!(
            r#"INSERT INTO tracking ("takenDate", "userId", status) VALUES ('{day}', {}, 1)
               ON CONFLICT ("userId", "takenDate") DO UPDATE SET status = 1 RETURNING id"#,
            c.staff.id
        ))
        .await;
    app.exec(&format!(
        r#"INSERT INTO "trackingTimeline" (note, "startAt", "endAt", "trackingId", "projectId", status)
           VALUES ('it', '{day} 01:00:00', '{day} 02:00:00', {t}, {project}, 1)"#
    ))
    .await;

    let r = app
        .post("/admin/sync/trigger")
        .token(&token)
        .json(&json!({"date": day}))
        .send()
        .await;
    r.assert_status(201).assert_no_sql_error();
    assert_eq!(r.json, json!({"message":"Success","date":day}));

    let secs: i32 = app
        .scalar(&format!(
            r#"SELECT "totalSeconds" FROM "dailyProjectSummary"
               WHERE "projectId" = {project} AND "companyId" = {} AND "reportDate" = '{day}'"#,
            c.id
        ))
        .await;
    assert_eq!(secs, 3600);
    let company_secs: i32 = app
        .scalar(&format!(
            r#"SELECT "totalSeconds" FROM "dailyCompanySummary" WHERE "companyId" = {} AND "reportDate" = '{day}'"#,
            c.id
        ))
        .await;
    assert!(company_secs >= 3600);

    // Idempoten: panggilan kedua tetap sukses dan nilainya sama.
    app.post("/admin/sync/trigger")
        .token(&token)
        .json(&json!({"date": day}))
        .send()
        .await
        .assert_status(201);
    let again: i32 = app
        .scalar(&format!(
            r#"SELECT "totalSeconds" FROM "dailyProjectSummary"
               WHERE "projectId" = {project} AND "companyId" = {} AND "reportDate" = '{day}'"#,
            c.id
        ))
        .await;
    assert_eq!(again, 3600);
    assert_eq!(audit_count(&app, op, "sync.trigger").await, 2);
}

#[tokio::test]
async fn manager_summary_test_paths() {
    let _g = SERIAL.lock().await;
    let Some(app) = app().await else { return };
    let c = app.company().await;
    let (op, token) = operator(&app, 1).await;

    // Company tanpa config -> 201 {success:false}, tanpa audit.
    app.exec(&format!(
        r#"DELETE FROM "managerSummaryConfig" WHERE "companyId" = {}"#,
        c.id
    ))
    .await;
    let r = app
        .post("/admin/manager-summary/test")
        .token(&token)
        .json(&json!({"companyId": c.id, "type": "daily"}))
        .send()
        .await;
    r.assert_status(201).assert_no_sql_error();
    assert_eq!(
        &r.json,
        &json!({"success": false, "message": "Manager summary config not found for this company"})
    );
    assert_eq!(audit_count(&app, op, "manager_summary.test").await, 0);

    // Config tanpa penerima.
    app.exec(&format!(
        r#"INSERT INTO "managerSummaryConfig" ("companyId", emails, "createdById") VALUES ({}, '[]'::jsonb, {})"#,
        c.id, c.admin.id
    ))
    .await;
    let r = app
        .post("/admin/manager-summary/test")
        .token(&token)
        .json(&json!({"companyId": c.id, "type": "weekly"}))
        .send()
        .await;
    r.assert_status(201);
    assert_eq!(
        &r.json,
        &json!({"success": false, "message": "No email recipients configured"})
    );

    // Override penerima: sukses (email tidak keluar jaringan), audit ber-companyId.
    for body in [
        json!({"companyId": c.id, "type": "daily", "date": "2001-04-10", "emails": ["qa@example.com"]}),
        json!({"companyId": c.id, "type": "weekly", "startDate": "2001-03-01", "endDate": "2001-03-07", "emails": ["qa@example.com"]}),
    ] {
        let r = app
            .post("/admin/manager-summary/test")
            .token(&token)
            .json(&body)
            .send()
            .await;
        r.assert_status(201).assert_no_sql_error();
        assert_eq!(r.json["success"], json!(true), "{:?}", r.json);
    }
    assert_eq!(audit_count(&app, op, "manager_summary.test").await, 2);
    let cid: Option<i32> = sqlx::query_scalar(
        r#"SELECT "companyId" FROM "operatorAuditLog" WHERE "operatorId" = $1 AND action = 'manager_summary.test' LIMIT 1"#,
    )
    .bind(op)
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!(cid, Some(c.id));
}

#[tokio::test]
async fn work_limit_breach_recorded() {
    let _g = SERIAL.lock().await;
    let Some(app) = app().await else { return };
    let c = app.company().await;
    let proj: i32 = app
        .scalar(r#"SELECT id FROM project ORDER BY id LIMIT 1"#)
        .await;
    // 2001-03-05 = Senin: daily + weekly (Mon 2001-02-26 .. Sun 2001-03-04) diperiksa.
    let (monday, prev_monday) = ("2001-03-05", "2001-02-26");
    app.exec(&format!(
        r#"DELETE FROM "userWorkTimeLimit" WHERE "userId" = {} AND "companyId" = {}"#,
        c.staff.id, c.id
    ))
    .await;
    app.exec(&format!(
        r#"INSERT INTO "userWorkTimeLimit" ("userId", "companyId", "dailyLimitSec", "weeklyLimitSec", status)
           VALUES ({}, {}, 3600, 7200, 1)"#,
        c.staff.id, c.id
    ))
    .await;
    for (d, s) in [(monday, 5400), (prev_monday, 8000)] {
        app.exec(&format!(
            r#"INSERT INTO "dailyUserProjectSummary" ("reportDate", "userId", "projectId", "companyId", "totalSeconds")
               VALUES ('{d}', {}, {proj}, {}, {s})
               ON CONFLICT ("userId", "projectId", "reportDate") DO UPDATE SET "totalSeconds" = {s}, "companyId" = {}"#,
            c.staff.id, c.id, c.id
        ))
        .await;
    }
    let date = NaiveDate::from_ymd_opt(2001, 3, 5).unwrap();
    // Dua kali: upsert idempoten.
    for _ in 0..2 {
        mz_tasks_sync::worklimit::check_work_limits(app.state(), date, &[c.id])
            .await
            .unwrap();
    }
    let rows: Vec<(String, String, i32, i32, i32)> = sqlx::query_as(
        r#"SELECT "limitType"::text, "periodDate"::text, "workedSec", "limitSec", "overtimeSec"
           FROM "workLimitBreach" WHERE "userId" = $1 AND "companyId" = $2 ORDER BY "limitType"::text"#,
    )
    .bind(c.staff.id)
    .bind(c.id)
    .fetch_all(app.db())
    .await
    .unwrap();
    assert_eq!(
        rows,
        vec![
            ("DAILY".into(), "2001-03-05".into(), 5400, 3600, 1800),
            ("WEEKLY".into(), "2001-02-26".into(), 8000, 7200, 800),
        ]
    );
}
