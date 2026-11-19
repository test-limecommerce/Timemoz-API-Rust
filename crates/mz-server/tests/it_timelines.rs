#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
//! Uji integrasi `mz-timelines` (13 route) terhadap Postgres nyata.
//!
//! Setiap test membuat company BARU (+ user, project, tracking) lewat SQL supaya
//! pengaturan company (mode manual time, budget, batas jam kerja) dan timeline
//! user tidak saling ganggu saat test berjalan paralel pada satu database.
//!
//! Jalankan: `MZ_TEST_DATABASE_URL=postgres://… cargo test -p mz-server --test it_timelines`

use std::sync::atomic::{AtomicU32, Ordering};

use mz_testkit::{TestApp, TestResponse};
use serde_json::{Value, json};

static SEQ: AtomicU32 = AtomicU32::new(1);

/// Hari tetap (Senin) untuk timeline "live": jauh di masa lalu, tidak ada cek umur/masa depan.
const D: &str = "2026-10-05";

fn t(h: u32, m: u32) -> String {
    format!("{D}T{h:02}:{m:02}:00.000Z")
}

fn sql_t(h: u32, m: u32) -> String {
    format!("{D} {h:02}:{m:02}:00")
}

// ---------------------------------------------------------------------------
// Fixture

#[derive(Clone)]
struct U {
    id: i32,
    uuid: String,
    token: String,
}

struct W {
    cid: i32,
    staff: U,
    staff2: U,
    manager: U,
    admin: U,
    project: i32,
    project_uuid: String,
}

fn next() -> u32 {
    SEQ.fetch_add(1, Ordering::Relaxed)
}

async fn user(app: &TestApp, cid: i32, role: &str, manual: bool, edit: bool) -> U {
    let n = next();
    let id: i32 = app
        .scalar(&format!(
            r#"INSERT INTO users (username, password, name, status)
               VALUES ('itl{n}@test.local', 'x', 'ITL User {n}', 1) RETURNING id"#
        ))
        .await;
    app.exec(&format!(
        r#"INSERT INTO "userCompany" ("userId", "companyId", status, "isAllowManualTime", "isAllowEditTime",
             "idleTimeInSecond", "isDefault") VALUES ({id}, {cid}, 1, {manual}, {edit}, 300, true);
           INSERT INTO "userAccessLevel" ("userId", "accessLevelId", "companyId", status)
             SELECT {id}, id, {cid}, 1 FROM "accessLevel" WHERE code = '{role}' AND "isSystem";"#
    ))
    .await;
    let uuid: String = app
        .scalar(&format!("SELECT uuid::text FROM users WHERE id = {id}"))
        .await;
    U {
        id,
        uuid,
        token: app.token(id),
    }
}

async fn project(app: &TestApp, cid: i32, billable: bool) -> (i32, String) {
    let n = next();
    let id: i32 = app
        .scalar(&format!(
            r#"INSERT INTO project (code, name, "screenshotPerHour", "projectTypeId", "startDate", status, billable)
               SELECT 'ITL{n}', 'ITL Project {n}', 3, (SELECT id FROM "projectType" ORDER BY id LIMIT 1),
                      '2026-01-01', 1, {billable} RETURNING id"#
        ))
        .await;
    app.exec(&format!(
        r#"INSERT INTO "projectCompany" ("projectId", "companyId", status) VALUES ({id}, {cid}, 1)"#
    ))
    .await;
    let uuid: String = app
        .scalar(&format!("SELECT uuid::text FROM project WHERE id = {id}"))
        .await;
    (id, uuid)
}

/// Company baru dengan mode manual time `mode` (NONE | ALL | LIMIT).
async fn world(app: &TestApp, mode: &str) -> W {
    let n = next();
    let cid: i32 = app
        .scalar(&format!(
            r#"INSERT INTO company (name, status, "manualTimeApprovalMode")
               VALUES ('ITL Co {n}', 1, '{mode}') RETURNING id"#
        ))
        .await;
    let staff = user(app, cid, "STAFF", false, false).await;
    let staff2 = user(app, cid, "STAFF", false, false).await;
    let manager = user(app, cid, "MANAGER", false, false).await;
    let admin = user(app, cid, "ADMIN", false, false).await;
    let (project, project_uuid) = project(app, cid, true).await;
    W {
        cid,
        staff,
        staff2,
        manager,
        admin,
        project,
        project_uuid,
    }
}

async fn tracking(app: &TestApp, user_id: i32, date: &str) -> (i32, String) {
    let id: i32 = app
        .scalar(&format!(
            r#"INSERT INTO tracking ("takenDate", "userId", status, "totalTakenInSecond", "isAbsent")
               VALUES ('{date}', {user_id}, 1, 0, false) RETURNING id"#
        ))
        .await;
    let uuid: String = app
        .scalar(&format!("SELECT uuid::text FROM tracking WHERE id = {id}"))
        .await;
    (id, uuid)
}

/// Timeline seed (UTC). `manual`: `None` = live, selain itu status manual time.
async fn tl(
    app: &TestApp,
    tr: i32,
    project: i32,
    from: (u32, u32),
    to: (u32, u32),
    manual: Option<&str>,
) -> (i32, String) {
    let (status, manual_sql, approval) = match manual {
        None => (1, "NULL".to_owned(), "NULL, NULL, NULL"),
        Some("PENDING") => (
            0,
            "'PENDING'::\"ManualTimeStatus\"".to_owned(),
            "NULL, NULL, NULL",
        ),
        Some(m) => (
            1,
            format!("'{m}'::\"ManualTimeStatus\""),
            "'AUTO'::\"ManualTimeApprovalSource\", now(), NULL",
        ),
    };
    let (a, b) = (sql_t(from.0, from.1), sql_t(to.0, to.1));
    let id: i32 = app
        .scalar(&format!(
            r#"INSERT INTO "trackingTimeline" (note, "isOffline", "startAt", "endAt", "startAtUtc", "endAtUtc",
                 timezone, "offsetMinutes", "trackingId", "projectId", status, billable, "manualTimeStatus",
                 "manualTimeApprovalSource", "approvedAt", "approvedById")
               VALUES ('seed', false, '{a}', '{b}', '{a}', '{b}', 'UTC', 0, {tr}, {project}, {status}, true,
                 {manual_sql}, {approval}) RETURNING id"#
        ))
        .await;
    let uuid: String = app
        .scalar(&format!(
            r#"SELECT uuid::text FROM "trackingTimeline" WHERE id = {id}"#
        ))
        .await;
    (id, uuid)
}

/// Live timeline UTC pada tanggal bebas (`a`/`b` = `YYYY-MM-DD HH:MM:SS`).
async fn tl_at(app: &TestApp, tr: i32, project: i32, a: &str, b: &str) -> i32 {
    app.scalar(&format!(
        r#"INSERT INTO "trackingTimeline" (note, "isOffline", "startAt", "endAt", "startAtUtc", "endAtUtc", timezone,
             "offsetMinutes", "trackingId", "projectId", status, billable)
           VALUES ('live', false, '{a}', '{b}', '{a}', '{b}', 'UTC', 0, {tr}, {project}, 1, true) RETURNING id"#
    ))
    .await
}

async fn set_total(app: &TestApp, tr: i32, secs: i32) {
    app.exec(&format!(
        r#"UPDATE tracking SET "totalTakenInSecond" = {secs} WHERE id = {tr}"#
    ))
    .await;
}

async fn total(app: &TestApp, tr: i32) -> i32 {
    app.scalar(&format!(
        r#"SELECT COALESCE("totalTakenInSecond", -1) FROM tracking WHERE id = {tr}"#
    ))
    .await
}

async fn count(app: &TestApp, sql: &str) -> i64 {
    app.scalar(&format!("SELECT count(*) FROM ({sql}) q")).await
}

async fn active_rows(app: &TestApp, tr: i32) -> i64 {
    count(
        app,
        &format!(r#"SELECT 1 FROM "trackingTimeline" WHERE "trackingId" = {tr} AND status = 1"#),
    )
    .await
}

async fn shot(app: &TestApp, tl_id: i32, project: i32, at: (u32, u32)) -> i32 {
    app.scalar(&format!(
        r#"INSERT INTO screenshot ("fileFullpath", "takenAt", "trackingTimelineId", "projectId", status)
           VALUES ('itl/x.png', '{}', {tl_id}, {project}, 1) RETURNING id"#,
        sql_t(at.0, at.1)
    ))
    .await
}

async fn app_row(app: &TestApp, tl_id: i32, at: (u32, u32)) -> i32 {
    app.scalar(&format!(
        r#"INSERT INTO app ("trackingTimelineId", name, title, "startAt", "endAt", timezone)
           VALUES ({tl_id}, 'code', 'editor', '{0}', '{0}', 'UTC') RETURNING id"#,
        sql_t(at.0, at.1)
    ))
    .await
}

async fn url_row(app: &TestApp, tl_id: i32, at: (u32, u32)) -> i32 {
    app.scalar(&format!(
        r#"INSERT INTO url ("trackingTimelineId", host, url, title, "startAt", "endAt", timezone)
           VALUES ({tl_id}, 'example.com', 'https://example.com', 'Example', '{0}', '{0}', 'UTC') RETURNING id"#,
        sql_t(at.0, at.1)
    ))
    .await
}

async fn tl_of(app: &TestApp, kind: &str, id: i32) -> i32 {
    app.scalar(&format!(
        r#"SELECT "trackingTimelineId" FROM {kind} WHERE id = {id}"#
    ))
    .await
}

async fn tl_field(app: &TestApp, id: i32, expr: &str) -> String {
    app.scalar(&format!(
        r#"SELECT COALESCE(({expr})::text, 'NULL') FROM "trackingTimeline" WHERE id = {id}"#
    ))
    .await
}

async fn tl_id(app: &TestApp, uuid: &str) -> i32 {
    app.scalar(&format!(
        r#"SELECT id FROM "trackingTimeline" WHERE uuid = '{uuid}'"#
    ))
    .await
}

// ---------------------------------------------------------------------------
// HTTP

async fn post(app: &TestApp, uri: &str, token: &str, body: &Value) -> TestResponse {
    let r = app.post(uri).token(token).json(body).send().await;
    r.assert_no_sql_error();
    r
}

async fn patch(app: &TestApp, uri: &str, token: &str, body: &Value) -> TestResponse {
    let r = app.patch(uri).token(token).json(body).send().await;
    r.assert_no_sql_error();
    r
}

async fn get(app: &TestApp, uri: &str, token: &str) -> TestResponse {
    let r = app.get(uri).token(token).send().await;
    r.assert_no_sql_error();
    r
}

async fn del(app: &TestApp, uri: &str, token: &str) -> TestResponse {
    let r = app.delete(uri).token(token).send().await;
    r.assert_no_sql_error();
    r
}

fn msg(r: &TestResponse) -> String {
    r.json["message"].as_str().unwrap_or_default().to_owned()
}

/// HTTP 200 dan `success: true`.
fn ok(r: &TestResponse) -> &Value {
    r.assert_status(200);
    assert_eq!(r.json["success"], json!(true), "{}", r.json);
    &r.json
}

/// HTTP 200 dan `success: false` dengan pesan memuat `needle`.
fn fails(r: &TestResponse, needle: &str) {
    r.assert_status(200);
    assert_eq!(r.json["success"], json!(false), "{}", r.json);
    assert!(
        msg(r).contains(needle),
        "pesan `{}` tidak memuat `{needle}`",
        msg(r)
    );
}

/// Body create/update timeline UTC (`extra` menimpa field dasar).
fn body(w: &W, tr_uuid: &str, from: (u32, u32), to: (u32, u32), extra: &Value) -> Value {
    let mut b = json!({
        "startAt": t(from.0, from.1),
        "endAt": t(to.0, to.1),
        "timezone": "UTC",
        "offsetMinutes": 0,
        "isOffline": false,
        "trackingUuid": tr_uuid,
        "projectUuid": w.project_uuid,
    });
    if let (Some(base), Some(add)) = (b.as_object_mut(), extra.as_object()) {
        for (k, v) in add {
            base.insert(k.clone(), v.clone());
        }
    }
    b
}

async fn random_uuid(app: &TestApp) -> String {
    app.scalar("SELECT gen_random_uuid()::text").await
}

const CREATE: &str = "/tracking-timelines";

// ===========================================================================
// POST /tracking-timelines (create) + create-offline

#[tokio::test]
async fn create_existing_tracking_and_dedupe() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let w = world(&app, "ALL").await;
    let (tr, tr_uuid) = tracking(&app, w.staff.id, D).await;
    let tok = &w.staff.token;

    // Baru: row + total tracking naik, billable mengikuti project, note default "-".
    let r = post(
        &app,
        CREATE,
        tok,
        &body(&w, &tr_uuid, (1, 0), (2, 0), &json!({})),
    )
    .await;
    let j = ok(&r);
    assert_eq!(j["message"], "Create tracking timeline success");
    assert_eq!(j["data"]["userUuid"], json!(w.staff.uuid));
    assert_eq!(j["data"]["takenDate"], "2026-10-05T00:00:00.000Z");
    let uuid = j["data"]["uuid"].as_str().unwrap().to_owned();
    let id = tl_id(&app, &uuid).await;
    assert_eq!(total(&app, tr).await, 3600);
    assert_eq!(tl_field(&app, id, "note").await, "-");
    assert_eq!(tl_field(&app, id, "billable").await, "true");
    assert_eq!(tl_field(&app, id, r#""isOffline""#).await, "false");
    assert_eq!(tl_field(&app, id, r#""startAtUtc""#).await, sql_t(1, 0));
    assert_eq!(tl_field(&app, id, r#""offsetMinutes""#).await, "0");
    assert_eq!(tl_field(&app, id, r#""taskId""#).await, "NULL");

    // POST ganda yang identik: idempotent (row sama, total tidak berubah).
    let r = post(
        &app,
        CREATE,
        tok,
        &body(&w, &tr_uuid, (1, 0), (2, 0), &json!({})),
    )
    .await;
    assert_eq!(ok(&r)["data"]["uuid"], json!(uuid));
    assert_eq!(active_rows(&app, tr).await, 1);
    assert_eq!(total(&app, tr).await, 3600);

    // Beririsan dengan live lain → "already exists", tidak ada row baru.
    let r = post(
        &app,
        CREATE,
        tok,
        &body(&w, &tr_uuid, (1, 30), (2, 30), &json!({})),
    )
    .await;
    assert_eq!(ok(&r)["message"], "Tracking timeline already exists");
    assert_eq!(active_rows(&app, tr).await, 1);

    // Dashboard + overlap → "Overlap detected".
    let r = post(
        &app,
        CREATE,
        tok,
        &body(
            &w,
            &tr_uuid,
            (1, 15),
            (1, 45),
            &json!({"from": "dashboard"}),
        ),
    )
    .await;
    fails(&r, "Overlap detected");

    // Bersebelahan: note + billable eksplisit.
    let r = post(
        &app,
        CREATE,
        tok,
        &body(
            &w,
            &tr_uuid,
            (2, 0),
            (2, 30),
            &json!({"note": "halo", "billable": false}),
        ),
    )
    .await;
    let id2 = tl_id(&app, ok(&r)["data"]["uuid"].as_str().unwrap()).await;
    assert_eq!(tl_field(&app, id2, "note").await, "halo");
    assert_eq!(tl_field(&app, id2, "billable").await, "false");
    assert_eq!(total(&app, tr).await, 5400);

    // Manager (seluruh company) boleh membuat untuk staff; staff lain tidak.
    let r = post(
        &app,
        CREATE,
        &w.manager.token,
        &body(&w, &tr_uuid, (3, 0), (3, 10), &json!({})),
    )
    .await;
    ok(&r);
    assert_eq!(total(&app, tr).await, 6000);
    let r = post(
        &app,
        CREATE,
        &w.staff2.token,
        &body(&w, &tr_uuid, (4, 0), (4, 10), &json!({})),
    )
    .await;
    fails(&r, "not allowed to manage tracking timeline");
    assert_eq!(total(&app, tr).await, 6000);
}

#[tokio::test]
async fn create_validation_branches() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let w = world(&app, "ALL").await;
    let (tr, tr_uuid) = tracking(&app, w.staff.id, D).await;
    let tok = &w.staff.token;

    let r = post(
        &app,
        CREATE,
        tok,
        &body(&w, &tr_uuid, (2, 0), (1, 0), &json!({})),
    )
    .await;
    fails(&r, "Invalid endtime");
    let r = post(
        &app,
        CREATE,
        tok,
        &body(&w, &tr_uuid, (0, 0), (15, 0), &json!({})),
    )
    .await;
    fails(&r, "Duration exceeds maximum limit of 14 hours");
    // Lintas tengah malam.
    let r = post(
        &app,
        CREATE,
        tok,
        &body(
            &w,
            &tr_uuid,
            (22, 0),
            (23, 0),
            &json!({"endAt": "2026-10-06T02:00:00.000Z"}),
        ),
    )
    .await;
    fails(&r, "cannot cross midnight");
    // Project tak dikenal / project company lain.
    let unknown = random_uuid(&app).await;
    let r = post(
        &app,
        CREATE,
        tok,
        &body(
            &w,
            &tr_uuid,
            (1, 0),
            (2, 0),
            &json!({"projectUuid": unknown}),
        ),
    )
    .await;
    fails(&r, "Project UUID not found");
    let other = world(&app, "ALL").await;
    let r = post(
        &app,
        CREATE,
        tok,
        &body(
            &w,
            &tr_uuid,
            (1, 0),
            (2, 0),
            &json!({"projectUuid": other.project_uuid}),
        ),
    )
    .await;
    fails(&r, "Project UUID not found");
    // Timezone kosong → TypeError bergaya Node.
    let r = post(
        &app,
        CREATE,
        tok,
        &body(&w, &tr_uuid, (1, 0), (2, 0), &json!({"timezone": ""})),
    )
    .await;
    fails(&r, "reading 'isSame'");
    // Body tidak sah → 400 validasi.
    let r = app
        .post(CREATE)
        .token(tok)
        .json(&json!({"startAt": "bukan-tanggal"}))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(active_rows(&app, tr).await, 0);
    assert_eq!(total(&app, tr).await, 0);
}

#[tokio::test]
async fn create_creates_tracking_when_missing() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let w = world(&app, "ALL").await;
    let tok = &w.staff.token;
    let today: String = app
        .scalar("SELECT (now() AT TIME ZONE 'UTC')::date::text")
        .await;
    let tomorrow: String = app
        .scalar("SELECT ((now() AT TIME ZONE 'UTC')::date + 1)::text")
        .await;
    let yesterday: String = app
        .scalar("SELECT ((now() AT TIME ZONE 'UTC')::date - 1)::text")
        .await;

    // Start di masa lalu → takenDate = hari ini; tracking baru dibuat untuk pemanggil.
    let fresh = random_uuid(&app).await;
    let r = post(
        &app,
        CREATE,
        tok,
        &body(&w, &fresh, (1, 0), (2, 0), &json!({})),
    )
    .await;
    let j = ok(&r);
    assert_eq!(j["data"]["userUuid"], json!(w.staff.uuid));
    let tr_id: i32 = app
        .scalar(&format!(
            r#"SELECT id FROM tracking WHERE "userId" = {} AND "takenDate" = '{today}'"#,
            w.staff.id
        ))
        .await;
    assert_eq!(
        total(&app, tr_id).await,
        -1,
        "totalTakenInSecond NULL tetap NULL (parity Prisma increment)"
    );
    assert_eq!(active_rows(&app, tr_id).await, 1);

    // Tracking hari ini sudah ada + trackingUuid acak → pelanggaran unique (userId, takenDate).
    let again = random_uuid(&app).await;
    let r = post(
        &app,
        CREATE,
        tok,
        &body(&w, &again, (3, 0), (4, 0), &json!({})),
    )
    .await;
    fails(
        &r,
        "Unique constraint failed on the fields: (`userId`,`takenDate`)",
    );

    // Start besok → takenDate = tanggal start (bukan hari ini).
    let fut = random_uuid(&app).await;
    let r = post(
        &app,
        CREATE,
        tok,
        &body(
            &w,
            &fut,
            (0, 0),
            (0, 0),
            &json!({"startAt": format!("{tomorrow}T10:00:00.000Z"), "endAt": format!("{tomorrow}T11:00:00.000Z")}),
        ),
    )
    .await;
    ok(&r);
    assert_eq!(
        count(
            &app,
            &format!(
                r#"SELECT 1 FROM tracking WHERE "userId" = {} AND "takenDate" = '{tomorrow}'"#,
                w.staff.id
            )
        )
        .await,
        1
    );

    // Manager membuat atas nama staff2 lewat userUuid; userUuid tak dikenal → gagal.
    let for_staff2 = random_uuid(&app).await;
    let r = post(
        &app,
        CREATE,
        &w.manager.token,
        &body(
            &w,
            &for_staff2,
            (0, 0),
            (0, 0),
            &json!({"userUuid": w.staff2.uuid, "startAt": format!("{yesterday}T01:00:00.000Z"), "endAt": format!("{yesterday}T02:00:00.000Z")}),
        ),
    )
    .await;
    let j = ok(&r);
    assert_eq!(j["data"]["userUuid"], json!(w.staff2.uuid));
    let tr2: i32 = app
        .scalar(&format!(
            r#"SELECT id FROM tracking WHERE "userId" = {}"#,
            w.staff2.id
        ))
        .await;
    assert_eq!(active_rows(&app, tr2).await, 1);
    let ghost = random_uuid(&app).await;
    let r = post(
        &app,
        CREATE,
        &w.manager.token,
        &body(
            &w,
            &random_uuid(&app).await,
            (1, 0),
            (2, 0),
            &json!({"userUuid": ghost}),
        ),
    )
    .await;
    fails(&r, "User UUID not found");
    // Staff tidak boleh membuat atas nama staff2.
    let r = post(
        &app,
        CREATE,
        tok,
        &body(
            &w,
            &random_uuid(&app).await,
            (1, 0),
            (2, 0),
            &json!({"userUuid": w.staff2.uuid}),
        ),
    )
    .await;
    fails(&r, "not allowed to manage tracking timeline");
}

#[tokio::test]
async fn create_work_time_limits() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let w = world(&app, "ALL").await;
    // Batas harian 1 jam untuk staff, batas pekanan 2 jam untuk staff2.
    app.exec(&format!(
        r#"INSERT INTO "userWorkTimeLimit" ("userId", "companyId", "dailyLimitSec", status) VALUES ({}, {}, 3600, 1);
           INSERT INTO "userWorkTimeLimit" ("userId", "companyId", "weeklyLimitSec", status) VALUES ({}, {}, 7200, 1);"#,
        w.staff.id, w.cid, w.staff2.id, w.cid
    ))
    .await;

    let (tr, tr_uuid) = tracking(&app, w.staff.id, D).await;
    let r = post(
        &app,
        CREATE,
        &w.staff.token,
        &body(&w, &tr_uuid, (1, 0), (2, 0), &json!({})),
    )
    .await;
    ok(&r);
    let r = post(
        &app,
        CREATE,
        &w.staff.token,
        &body(&w, &tr_uuid, (2, 0), (2, 30), &json!({})),
    )
    .await;
    fails(&r, "Daily work time limit reached");
    assert_eq!(total(&app, tr).await, 3600);

    // Pekanan: dua hari dalam pekan yang sama (Senin-Selasa) memakai kuota yang sama.
    let (tr_a, _) = tracking(&app, w.staff2.id, D).await;
    let (tr_b, tr_b_uuid) = tracking(&app, w.staff2.id, "2026-10-06").await;
    tl(&app, tr_a, w.project, (1, 0), (2, 0), None).await;
    tl(&app, tr_b, w.project, (1, 0), (2, 0), None).await;
    let r = post(
        &app,
        CREATE,
        &w.staff2.token,
        &body(&w, &tr_b_uuid, (3, 0), (3, 30), &json!({})),
    )
    .await;
    fails(&r, "Weekly work time limit reached");
    // Pekan berikutnya masih lapang.
    let (_, tr_c_uuid) = tracking(&app, w.staff2.id, "2026-10-12").await;
    let r = post(
        &app,
        CREATE,
        &w.staff2.token,
        &body(
            &w,
            &tr_c_uuid,
            (0, 0),
            (0, 0),
            &json!({"startAt": "2026-10-12T01:00:00.000Z", "endAt": "2026-10-12T02:00:00.000Z"}),
        ),
    )
    .await;
    ok(&r);
}

#[tokio::test]
async fn create_budget_blocking() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let w = world(&app, "ALL").await;
    let (_, staff_tr) = tracking(&app, w.staff.id, D).await;
    let (mgr_tr_id, mgr_tr) = tracking(&app, w.manager.id, D).await;
    app.exec(&format!(
        r#"INSERT INTO "projectBudget" ("projectId", "isEnabled", "budgetType", "budgetHours", "currentBurnedSeconds",
             enforcement) VALUES ({}, true, 'HOURS', 1, 3600, 'STOP_TRACKING')"#,
        w.project
    ))
    .await;
    let blocked = "reached its budget limit";
    let r = post(
        &app,
        CREATE,
        &w.staff.token,
        &body(&w, &staff_tr, (1, 0), (2, 0), &json!({})),
    )
    .await;
    fails(&r, blocked);
    // Mode COST.
    app.exec(&format!(
        r#"UPDATE "projectBudget" SET "budgetType" = 'COST', "budgetAmount" = 10, "currentBurnedCost" = 10 WHERE "projectId" = {}"#,
        w.project
    ))
    .await;
    let r = post(
        &app,
        CREATE,
        &w.staff.token,
        &body(&w, &staff_tr, (1, 0), (2, 0), &json!({})),
    )
    .await;
    fails(&r, blocked);
    // ALERT_ONLY tidak memblokir.
    app.exec(&format!(
        r#"UPDATE "projectBudget" SET enforcement = 'ALERT_ONLY' WHERE "projectId" = {}"#,
        w.project
    ))
    .await;
    let r = post(
        &app,
        CREATE,
        &w.staff.token,
        &body(&w, &staff_tr, (1, 0), (2, 0), &json!({})),
    )
    .await;
    ok(&r);
    // STOP_TRACKING hanya memblokir STAFF: manager lolos.
    app.exec(&format!(
        r#"UPDATE "projectBudget" SET enforcement = 'STOP_TRACKING' WHERE "projectId" = {}"#,
        w.project
    ))
    .await;
    let r = post(
        &app,
        CREATE,
        &w.manager.token,
        &body(&w, &mgr_tr, (1, 0), (2, 0), &json!({})),
    )
    .await;
    ok(&r);
    assert_eq!(total(&app, mgr_tr_id).await, 3600);
}

#[tokio::test]
async fn create_offline_route_gates_and_tasks() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let w = world(&app, "ALL").await;
    let allowed = user(&app, w.cid, "STAFF", true, false).await;
    let (_, tr_uuid) = tracking(&app, w.staff.id, D).await;
    let (tr_a, tr_a_uuid) = tracking(&app, allowed.id, D).await;
    let (tr_m, tr_m_uuid) = tracking(&app, w.manager.id, D).await;
    let uri = "/tracking-timelines/create-offline";
    let offline = json!({"isOffline": true});

    // Staff tanpa isAllowManualTime: ditolak hanya bila isOffline.
    let r = post(
        &app,
        uri,
        &w.staff.token,
        &body(&w, &tr_uuid, (1, 0), (2, 0), &offline),
    )
    .await;
    fails(
        &r,
        "You are not allowed to create offline tracking timeline",
    );
    let r = post(
        &app,
        uri,
        &w.staff.token,
        &body(&w, &tr_uuid, (1, 0), (2, 0), &json!({})),
    )
    .await;
    ok(&r);

    // Dengan flag, dan sebagai manager (role seluruh company): lolos, isOffline tersimpan.
    // Task ClickUp dikaitkan lewat uuid → taskId terisi (tanpa token company → tanpa jaringan).
    let task_id: i32 = app
        .scalar(&format!(
            r#"INSERT INTO task ("projectId", "companyId", source, "externalId", name)
               VALUES ({}, {}, 'clickup', 'abc123', 'Task A') RETURNING id"#,
            w.project, w.cid
        ))
        .await;
    let task_uuid: String = app
        .scalar(&format!("SELECT uuid::text FROM task WHERE id = {task_id}"))
        .await;
    let r = post(
        &app,
        uri,
        &allowed.token,
        &body(
            &w,
            &tr_a_uuid,
            (1, 0),
            (2, 0),
            &json!({"isOffline": true, "clickupTaskUuid": task_uuid}),
        ),
    )
    .await;
    let id = tl_id(&app, ok(&r)["data"]["uuid"].as_str().unwrap()).await;
    assert_eq!(tl_field(&app, id, r#""isOffline""#).await, "true");
    assert_eq!(tl_field(&app, id, r#""taskId""#).await, task_id.to_string());
    assert_eq!(total(&app, tr_a).await, 3600);
    let r = post(
        &app,
        uri,
        &w.manager.token,
        &body(&w, &tr_m_uuid, (1, 0), (1, 30), &offline),
    )
    .await;
    ok(&r);
    assert_eq!(total(&app, tr_m).await, 1800);

    // Admin + task ClickUp: jalur latar (admin_company_id + token company) berjalan tanpa jaringan
    // karena company uji tidak punya clickupApiToken.
    let (_, tr_ad_uuid) = tracking(&app, w.admin.id, D).await;
    let r = post(
        &app,
        uri,
        &w.admin.token,
        &body(
            &w,
            &tr_ad_uuid,
            (1, 0),
            (1, 30),
            &json!({"clickupTaskUuid": task_uuid}),
        ),
    )
    .await;
    ok(&r);
}

// ===========================================================================
// PATCH /tracking-timelines/:uuid, update-offline/:uuid, update-offline/dashboard/:uuid

fn upd(uuid: &str) -> String {
    format!("/tracking-timelines/{uuid}")
}

#[tokio::test]
async fn update_basic_extend_shrink_project_and_billable() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let w = world(&app, "ALL").await;
    let (tr, tr_uuid) = tracking(&app, w.staff.id, D).await;
    let (id, uuid) = tl(&app, tr, w.project, (1, 0), (2, 0), None).await;
    set_total(&app, tr, 3600).await;
    let tok = &w.staff.token;

    // Perpanjang + note + billable false.
    let r = patch(
        &app,
        &upd(&uuid),
        tok,
        &body(
            &w,
            &tr_uuid,
            (1, 0),
            (2, 30),
            &json!({"note": "upd", "billable": false}),
        ),
    )
    .await;
    let j = ok(&r);
    assert_eq!(j["message"], "update tracking timeline success");
    assert_eq!(j["data"]["userUuid"], json!(w.staff.uuid));
    assert_eq!(tl_field(&app, id, r#""endAt""#).await, sql_t(2, 30));
    assert_eq!(tl_field(&app, id, r#""endAtUtc""#).await, sql_t(2, 30));
    assert_eq!(tl_field(&app, id, "note").await, "upd");
    assert_eq!(tl_field(&app, id, "billable").await, "false");
    assert_eq!(total(&app, tr).await, 5400);

    // Lewat route update-offline: persingkat; billable tak dikirim → tetap.
    let r = patch(
        &app,
        &format!("/tracking-timelines/update-offline/{uuid}"),
        tok,
        &body(&w, &tr_uuid, (1, 0), (1, 30), &json!({})),
    )
    .await;
    ok(&r);
    assert_eq!(tl_field(&app, id, r#""endAt""#).await, sql_t(1, 30));
    assert_eq!(tl_field(&app, id, "billable").await, "false");
    assert_eq!(tl_field(&app, id, "note").await, "upd");
    assert_eq!(total(&app, tr).await, 1800);

    // Pindah project (company sama) + taskId dari task Jira.
    let (p2, p2_uuid) = project(&app, w.cid, false).await;
    let task_id: i32 = app
        .scalar(&format!(
            r#"INSERT INTO task ("projectId", "companyId", source, "externalId", "externalMeta", name)
               VALUES ({p2}, {}, 'jira', '10001', '{{"issueKey":"ITL-1"}}', 'Jira task') RETURNING id"#,
            w.cid
        ))
        .await;
    let task_uuid: String = app
        .scalar(&format!("SELECT uuid::text FROM task WHERE id = {task_id}"))
        .await;
    let r = patch(
        &app,
        &upd(&uuid),
        &w.admin.token,
        &body(
            &w,
            &tr_uuid,
            (1, 0),
            (1, 30),
            &json!({"projectUuid": p2_uuid, "jiraTaskUuid": task_uuid, "billable": true}),
        ),
    )
    .await;
    ok(&r);
    assert_eq!(tl_field(&app, id, r#""projectId""#).await, p2.to_string());
    assert_eq!(tl_field(&app, id, r#""taskId""#).await, task_id.to_string());
    assert_eq!(tl_field(&app, id, "billable").await, "true");
    assert_eq!(total(&app, tr).await, 1800);

    // PATCH parsial (tanpa startAt/endAt/projectUuid): rentang, total, dan project
    // TETAP. Node menghitung durasi dari "sekarang" (total berkurang) dan memindah
    // ke project pertama milik owner — bug Node yang tidak ditiru (deviasi #15).
    let r = patch(
        &app,
        &upd(&uuid),
        tok,
        &json!({"note": "note saja", "timezone": "UTC", "trackingUuid": tr_uuid}),
    )
    .await;
    ok(&r);
    assert_eq!(tl_field(&app, id, "note").await, "note saja");
    assert_eq!(tl_field(&app, id, r#""startAt""#).await, sql_t(1, 0));
    assert_eq!(tl_field(&app, id, r#""endAt""#).await, sql_t(1, 30));
    assert_eq!(tl_field(&app, id, r#""projectId""#).await, p2.to_string());
    assert_eq!(total(&app, tr).await, 1800);

    // Hanya endAt: startAt diambil dari baris tersimpan.
    let r = patch(
        &app,
        &upd(&uuid),
        tok,
        &json!({"endAt": t(2, 0), "timezone": "UTC", "trackingUuid": tr_uuid}),
    )
    .await;
    ok(&r);
    assert_eq!(tl_field(&app, id, r#""startAt""#).await, sql_t(1, 0));
    assert_eq!(tl_field(&app, id, r#""endAt""#).await, sql_t(2, 0));
    assert_eq!(tl_field(&app, id, r#""projectId""#).await, p2.to_string());
    assert_eq!(total(&app, tr).await, 3600);
}

#[tokio::test]
async fn update_error_branches() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let w = world(&app, "ALL").await;
    let (tr, tr_uuid) = tracking(&app, w.staff.id, D).await;
    let (id, uuid) = tl(&app, tr, w.project, (1, 0), (2, 0), None).await;
    let (_, inactive) = {
        let (i, u) = tl(&app, tr, w.project, (5, 0), (6, 0), None).await;
        app.exec(&format!(
            r#"UPDATE "trackingTimeline" SET status = 0 WHERE id = {i}"#
        ))
        .await;
        (i, u)
    };
    set_total(&app, tr, 3600).await;
    let tok = &w.staff.token;
    let b = |extra: Value| body(&w, &tr_uuid, (1, 0), (2, 30), &extra);

    let ghost = random_uuid(&app).await;
    fails(
        &patch(&app, &upd(&ghost), tok, &b(json!({}))).await,
        "tracking timeline uuid not found",
    );
    fails(
        &patch(&app, &upd(&inactive), tok, &b(json!({}))).await,
        "tracking timeline uuid not found",
    );
    fails(
        &patch(&app, &upd(&uuid), &w.staff2.token, &b(json!({}))).await,
        "You are not allowed to update this tracking timeline",
    );
    fails(
        &patch(
            &app,
            &upd(&uuid),
            tok,
            &body(&w, &tr_uuid, (2, 0), (1, 0), &json!({})),
        )
        .await,
        "Invalid endtime",
    );
    fails(
        &patch(
            &app,
            &upd(&uuid),
            tok,
            &body(
                &w,
                &tr_uuid,
                (22, 0),
                (23, 0),
                &json!({"endAt": "2026-10-06T02:00:00.000Z"}),
            ),
        )
        .await,
        "cannot cross midnight",
    );
    fails(
        &patch(&app, &upd(&uuid), tok, &b(json!({"trackingUuid": ghost}))).await,
        "Tracking UUID not found",
    );
    fails(
        &patch(&app, &upd(&uuid), tok, &b(json!({"projectUuid": ghost}))).await,
        "Project UUID not found",
    );
    fails(
        &patch(&app, &upd(&uuid), tok, &b(json!({"timezone": ""}))).await,
        "reading 'isSame'",
    );
    // uuid path tak sah → 400 pipe.
    patch(&app, "/tracking-timelines/bukan-uuid", tok, &b(json!({})))
        .await
        .assert_status(400);
    assert_eq!(tl_field(&app, id, r#""endAt""#).await, sql_t(2, 0));
    assert_eq!(total(&app, tr).await, 3600);
}

#[tokio::test]
async fn update_overlap_twin_and_manual_trim() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let w = world(&app, "ALL").await;
    let (tr, tr_uuid) = tracking(&app, w.staff.id, D).await;
    let tok = &w.staff.token;

    // 1) Diabaikan karena bertabrakan dengan live lain pada bagian yang ditambah.
    let (id1, u1) = tl(&app, tr, w.project, (1, 0), (2, 0), None).await;
    tl(&app, tr, w.project, (3, 0), (4, 0), None).await;
    set_total(&app, tr, 7200).await;
    let r = patch(
        &app,
        &upd(&u1),
        tok,
        &body(&w, &tr_uuid, (1, 0), (3, 30), &json!({})),
    )
    .await;
    let j = ok(&r);
    assert_eq!(j["message"], "Update ignored due to overlap");
    assert_eq!(j["ignored"], json!(true));
    assert_eq!(j["data"]["uuid"], json!(u1));
    assert_eq!(j["data"]["tracking"]["user"]["uuid"], json!(w.staff.uuid));
    assert_eq!(tl_field(&app, id1, r#""endAt""#).await, sql_t(2, 0));
    assert_eq!(total(&app, tr).await, 7200);
    // Dashboard: overlap = galat.
    let r = patch(
        &app,
        &upd(&u1),
        tok,
        &body(&w, &tr_uuid, (1, 0), (3, 30), &json!({"from": "dashboard"})),
    )
    .await;
    fails(&r, "Overlap detected");

    // 2) Kembaran (twin) hasil POST ganda: dinonaktifkan, screenshot dipindah.
    let (tr2, tr2_uuid) = tracking(&app, w.staff.id, "2026-10-06").await;
    let (x, xu) = tl(&app, tr2, w.project, (5, 0), (5, 20), None).await;
    let (twin, _) = tl(&app, tr2, w.project, (5, 0), (5, 10), None).await;
    let s_twin = shot(&app, twin, w.project, (5, 5)).await;
    set_total(&app, tr2, 1800).await;
    let r = patch(
        &app,
        &upd(&xu),
        tok,
        &body(&w, &tr2_uuid, (5, 0), (5, 30), &json!({})),
    )
    .await;
    ok(&r);
    assert_eq!(tl_field(&app, twin, "status").await, "0");
    assert_eq!(tl_of(&app, "screenshot", s_twin).await, x);
    assert_eq!(
        total(&app, tr2).await,
        1800,
        "durasi twin tidak dihitung dua kali"
    );
    assert_eq!(tl_field(&app, x, r#""endAt""#).await, sql_t(5, 30));

    // 3) Manual time APPROVED yang tertimpa live yang diperpanjang: dipangkas, anak dipindah.
    let (tr3, tr3_uuid) = tracking(&app, w.staff.id, "2026-10-07").await;
    let (live, live_u) = tl(&app, tr3, w.project, (8, 0), (8, 30), None).await;
    let (man, _) = tl(&app, tr3, w.project, (8, 45), (9, 30), Some("APPROVED")).await;
    let s_in = shot(&app, man, w.project, (8, 50)).await;
    let s_out = shot(&app, man, w.project, (9, 10)).await;
    let a_in = app_row(&app, man, (8, 50)).await;
    let u_in = url_row(&app, man, (8, 55)).await;
    set_total(&app, tr3, 1800 + 2700).await;
    let r = patch(
        &app,
        &upd(&live_u),
        tok,
        &body(&w, &tr3_uuid, (8, 0), (9, 0), &json!({})),
    )
    .await;
    ok(&r);
    assert_eq!(tl_of(&app, "screenshot", s_in).await, live);
    assert_eq!(tl_of(&app, "screenshot", s_out).await, man);
    assert_eq!(tl_of(&app, "app", a_in).await, live);
    assert_eq!(tl_of(&app, "url", u_in).await, live);
    assert_eq!(tl_field(&app, man, r#""startAt""#).await, sql_t(9, 0));
    assert_eq!(total(&app, tr3).await, 3600 + 1800);
}

#[tokio::test]
async fn update_dashboard_route_gate() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let w = world(&app, "ALL").await;
    let editor = user(&app, w.cid, "STAFF", false, true).await;
    let (tr_s, tr_s_uuid) = tracking(&app, w.staff.id, D).await;
    let (tr_e, tr_e_uuid) = tracking(&app, editor.id, D).await;
    let (tr_m, tr_m_uuid) = tracking(&app, w.manager.id, D).await;
    let (_, us) = tl(&app, tr_s, w.project, (1, 0), (2, 0), None).await;
    let (ide, ue) = tl(&app, tr_e, w.project, (1, 0), (2, 0), None).await;
    let (idm, um) = tl(&app, tr_m, w.project, (1, 0), (2, 0), None).await;
    for tr in [tr_s, tr_e, tr_m] {
        set_total(&app, tr, 3600).await;
    }
    let dash = |u: &str| format!("/tracking-timelines/update-offline/dashboard/{u}");

    // Tanpa isAllowEditTime: ditolak.
    let r = patch(
        &app,
        &dash(&us),
        &w.staff.token,
        &body(&w, &tr_s_uuid, (1, 0), (2, 30), &json!({})),
    )
    .await;
    fails(
        &r,
        "You are not allowed to update offline tracking timeline",
    );
    // Dengan flag / sebagai manager: diizinkan.
    let r = patch(
        &app,
        &dash(&ue),
        &editor.token,
        &body(&w, &tr_e_uuid, (1, 0), (2, 30), &json!({})),
    )
    .await;
    ok(&r);
    assert_eq!(tl_field(&app, ide, r#""endAt""#).await, sql_t(2, 30));
    assert_eq!(total(&app, tr_e).await, 5400);
    let r = patch(
        &app,
        &dash(&um),
        &w.manager.token,
        &body(
            &w,
            &tr_m_uuid,
            (1, 0),
            (2, 15),
            &json!({"from": "dashboard"}),
        ),
    )
    .await;
    ok(&r);
    assert_eq!(tl_field(&app, idm, r#""endAt""#).await, sql_t(2, 15));
    assert_eq!(total(&app, tr_m).await, 4500);
}

// ===========================================================================
// DELETE /tracking-timelines/:uuid, /delete-offline/:uuid

#[tokio::test]
async fn remove_branches() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let w = world(&app, "ALL").await;
    let (tr, _) = tracking(&app, w.staff.id, D).await;
    let tok = &w.staff.token;
    let (id1, u1) = tl(&app, tr, w.project, (1, 0), (2, 0), None).await;
    let (id2, u2) = tl(&app, tr, w.project, (3, 0), (3, 30), None).await;
    let (appr, u_appr) = tl(&app, tr, w.project, (4, 0), (4, 20), Some("APPROVED")).await;
    let (pend, u_pend) = tl(&app, tr, w.project, (5, 0), (5, 10), Some("PENDING")).await;
    set_total(&app, tr, 3600 + 1800 + 1200).await;

    // Live biasa: soft delete + total berkurang; data tanggal & uuid user.
    let r = del(&app, &upd(&u1), tok).await;
    let j = ok(&r);
    assert_eq!(j["message"], "delete tracking timeline success");
    assert_eq!(j["data"]["isDeleted"], json!(true));
    assert_eq!(j["data"]["userUuid"], json!(w.staff.uuid));
    assert_eq!(j["data"]["takenDate"], D);
    assert_eq!(tl_field(&app, id1, "status").await, "0");
    assert_eq!(total(&app, tr).await, 1800 + 1200);
    // Hapus ulang: sudah nonaktif → tetap sukses, total tidak berubah.
    ok(&del(&app, &upd(&u1), tok).await);
    assert_eq!(total(&app, tr).await, 1800 + 1200);

    // Alias /delete-offline.
    ok(&del(
        &app,
        &format!("/tracking-timelines/delete-offline/{u2}"),
        tok,
    )
    .await);
    assert_eq!(tl_field(&app, id2, "status").await, "0");
    assert_eq!(total(&app, tr).await, 1200);

    // Manual APPROVED: baris dihapus fisik + total berkurang; PENDING: dihapus, total tetap.
    ok(&del(&app, &upd(&u_appr), tok).await);
    assert_eq!(
        count(
            &app,
            &format!(r#"SELECT 1 FROM "trackingTimeline" WHERE id = {appr}"#)
        )
        .await,
        0
    );
    assert_eq!(total(&app, tr).await, 0);
    set_total(&app, tr, 777).await;
    ok(&del(&app, &upd(&u_pend), tok).await);
    assert_eq!(
        count(
            &app,
            &format!(r#"SELECT 1 FROM "trackingTimeline" WHERE id = {pend}"#)
        )
        .await,
        0
    );
    assert_eq!(total(&app, tr).await, 777);

    // Tidak ada / bukan milik / role lintas company.
    let ghost = random_uuid(&app).await;
    fails(
        &del(&app, &upd(&ghost), tok).await,
        "Tracking timeline UUID not found",
    );
    let (keep, u_keep) = tl(&app, tr, w.project, (6, 0), (6, 10), None).await;
    fails(
        &del(&app, &upd(&u_keep), &w.staff2.token).await,
        "You are not allowed to delete this tracking timeline",
    );
    let other = world(&app, "ALL").await;
    fails(
        &del(&app, &upd(&u_keep), &other.manager.token).await,
        "You are not allowed to delete this tracking timeline",
    );
    assert_eq!(tl_field(&app, keep, "status").await, "1");
    // Manager di company yang sama boleh.
    ok(&del(&app, &upd(&u_keep), &w.manager.token).await);
    assert_eq!(tl_field(&app, keep, "status").await, "0");
}

// ===========================================================================
// GET /tracking-timelines/:uuid

#[tokio::test]
async fn find_one_branches() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let w = world(&app, "ALL").await;
    let (tr, _) = tracking(&app, w.staff.id, D).await;
    let (id, uuid) = tl(&app, tr, w.project, (1, 0), (2, 0), None).await;
    let s1 = shot(&app, id, w.project, (1, 10)).await;
    let s2 = shot(&app, id, w.project, (1, 20)).await;
    let s1_uuid: String = app
        .scalar(&format!(
            "SELECT uuid::text FROM screenshot WHERE id = {s1}"
        ))
        .await;
    let (_, inactive) = {
        let (i, u) = tl(&app, tr, w.project, (5, 0), (6, 0), None).await;
        app.exec(&format!(
            r#"UPDATE "trackingTimeline" SET status = 0 WHERE id = {i}"#
        ))
        .await;
        (i, u)
    };

    let r = get(&app, &upd(&uuid), &w.staff.token).await;
    let j = ok(&r);
    assert_eq!(j["data"]["uuid"], json!(uuid));
    assert_eq!(j["data"]["note"], "seed");
    assert_eq!(j["data"]["startAt"], t(1, 0));
    assert_eq!(j["data"]["project"]["uuid"], json!(w.project_uuid));
    assert_eq!(j["data"]["timezone"], "UTC");
    assert_eq!(j["data"]["billable"], json!(true));
    assert_eq!(j["data"]["manualTimeStatus"], Value::Null);
    let shots = j["data"]["screenshot"].as_array().unwrap();
    assert_eq!(shots.len(), 2);
    assert_eq!(shots[0]["uuid"], json!(s1_uuid));
    assert_eq!(
        shots[0]["screenshotUrl"],
        json!(format!("/screenshots/{s1_uuid}/image?variant=display"))
    );
    assert_eq!(shots[1]["id"], json!(s2));

    // Manager company sama boleh; staff lain dan company lain ditolak; nonaktif/tak ada = not found.
    ok(&get(&app, &upd(&uuid), &w.manager.token).await);
    fails(
        &get(&app, &upd(&uuid), &w.staff2.token).await,
        "not allowed to access this tracking timeline",
    );
    let other = world(&app, "ALL").await;
    fails(
        &get(&app, &upd(&uuid), &other.admin.token).await,
        "not allowed to access this tracking timeline",
    );
    fails(
        &get(&app, &upd(&inactive), &w.staff.token).await,
        "not found",
    );
    let ghost = random_uuid(&app).await;
    fails(&get(&app, &upd(&ghost), &w.staff.token).await, "not found");
    get(&app, "/tracking-timelines/bukan-uuid", &w.staff.token)
        .await
        .assert_status(400);
    // Method salah pada path statis ditelan :uuid → 400.
    get(&app, "/tracking-timelines/manual-time", &w.staff.token)
        .await
        .assert_status(400);
}

// ===========================================================================
// PATCH /:uuid/note, PATCH /bulk-update-billable

#[tokio::test]
async fn update_note_branches() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let w = world(&app, "ALL").await;
    let (tr, _) = tracking(&app, w.staff.id, D).await;
    let (id, uuid) = tl(&app, tr, w.project, (1, 0), (2, 0), None).await;
    let note_uri = |u: &str| format!("/tracking-timelines/{u}/note");

    let r = patch(
        &app,
        &note_uri(&uuid),
        &w.manager.token,
        &json!({"note": "catatan baru"}),
    )
    .await;
    let j = ok(&r);
    assert_eq!(j["message"], "Note berhasil diupdate");
    assert_eq!(tl_field(&app, id, "note").await, "catatan baru");
    assert_ne!(tl_field(&app, id, r#""updatedAt""#).await, "NULL");

    // Staff: 401 "insufficient permissions" (RolesGuard). Company lain & uuid asing: 404. Validasi: 400.
    patch(
        &app,
        &note_uri(&uuid),
        &w.staff.token,
        &json!({"note": "x"}),
    )
    .await
    .assert_status(401);
    let other = world(&app, "ALL").await;
    patch(
        &app,
        &note_uri(&uuid),
        &other.manager.token,
        &json!({"note": "x"}),
    )
    .await
    .assert_status(404);
    let ghost = random_uuid(&app).await;
    patch(
        &app,
        &note_uri(&ghost),
        &w.manager.token,
        &json!({"note": "x"}),
    )
    .await
    .assert_status(404);
    patch(
        &app,
        &note_uri(&uuid),
        &w.manager.token,
        &json!({"note": ""}),
    )
    .await
    .assert_status(400);
    patch(
        &app,
        &note_uri(&uuid),
        &w.manager.token,
        &json!({"note": "a".repeat(256)}),
    )
    .await
    .assert_status(400);
    assert_eq!(tl_field(&app, id, "note").await, "catatan baru");
}

#[tokio::test]
async fn bulk_update_billable_branches() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let w = world(&app, "ALL").await;
    let (tr, _) = tracking(&app, w.staff.id, D).await;
    let (a, ua) = tl(&app, tr, w.project, (1, 0), (2, 0), None).await;
    let (b, ub) = tl(&app, tr, w.project, (3, 0), (4, 0), None).await;
    let (c, uc) = tl(&app, tr, w.project, (5, 0), (6, 0), None).await;
    app.exec(&format!(
        r#"UPDATE "trackingTimeline" SET status = 0 WHERE id = {c}"#
    ))
    .await;
    let uri = "/tracking-timelines/bulk-update-billable";
    let tok = &w.manager.token;

    // Duplikat uuid dihitung sekali.
    let r = patch(
        &app,
        uri,
        tok,
        &json!({"uuids": [ua, ub, ua], "billable": false}),
    )
    .await;
    let j = ok(&r);
    assert_eq!(j["data"]["updatedCount"], json!(2));
    assert_eq!(
        j["message"],
        "Successfully updated billable status for 2 entries"
    );
    assert_eq!(tl_field(&app, a, "billable").await, "false");
    assert_eq!(tl_field(&app, b, "billable").await, "false");

    // Semua-atau-tidak: salah satu tak ketemu (nonaktif / company lain / acak) → 404 + rollback.
    let ghost = random_uuid(&app).await;
    for bad in [uc.clone(), ghost] {
        patch(
            &app,
            uri,
            tok,
            &json!({"uuids": [ua, bad], "billable": true}),
        )
        .await
        .assert_status(404);
        assert_eq!(tl_field(&app, a, "billable").await, "false");
    }
    let other = world(&app, "ALL").await;
    patch(
        &app,
        uri,
        &other.manager.token,
        &json!({"uuids": [ua], "billable": true}),
    )
    .await
    .assert_status(404);
    assert_eq!(tl_field(&app, a, "billable").await, "false");

    fails(
        &patch(&app, uri, tok, &json!({"uuids": [], "billable": true})).await,
        "No UUIDs provided",
    );
    patch(
        &app,
        uri,
        &w.staff.token,
        &json!({"uuids": [ua], "billable": true}),
    )
    .await
    .assert_status(401);
    patch(&app, uri, tok, &json!({"uuids": ["x"], "billable": true}))
        .await
        .assert_status(400);
}

// ===========================================================================
// Live tracking memangkas manual time APPROVED (create)

#[tokio::test]
async fn create_trims_approved_manual_same_tracking() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let w = world(&app, "ALL").await;
    let (tr, tr_uuid) = tracking(&app, w.staff.id, D).await;
    let tok = &w.staff.token;
    let at = |from: (u32, u32), to: (u32, u32)| body(&w, &tr_uuid, from, to, &json!({}));

    // Delete: manual tertutup penuh → baris hilang, anak + lampiran pindah ke live.
    let (m1, _) = tl(&app, tr, w.project, (10, 0), (10, 20), Some("APPROVED")).await;
    let s1 = shot(&app, m1, w.project, (10, 5)).await;
    let a1 = app_row(&app, m1, (10, 6)).await;
    let u1 = url_row(&app, m1, (10, 7)).await;
    let att1: i32 = app
        .scalar(&format!(
            r#"INSERT INTO "manualTimeAttachment" ("fileName", "fileFullpath", "fileSize", "mimeType", "trackingTimelineId")
               VALUES ('a.png', 'manual-time/x/a.png', 10, 'image/png', {m1}) RETURNING id"#
        ))
        .await;
    set_total(&app, tr, 1200).await;
    let r = post(&app, CREATE, tok, &at((9, 50), (10, 30))).await;
    let live1 = tl_id(&app, ok(&r)["data"]["uuid"].as_str().unwrap()).await;
    assert_eq!(
        count(
            &app,
            &format!(r#"SELECT 1 FROM "trackingTimeline" WHERE id = {m1}"#)
        )
        .await,
        0
    );
    assert_eq!(tl_of(&app, "screenshot", s1).await, live1);
    assert_eq!(tl_of(&app, "app", a1).await, live1);
    assert_eq!(tl_of(&app, "url", u1).await, live1);
    assert_eq!(tl_of(&app, r#""manualTimeAttachment""#, att1).await, live1);
    assert_eq!(
        total(&app, tr).await,
        2400,
        "total = durasi live (manual 1200 detik terpangkas)"
    );

    // Split: manual 12:00-14:00 dipotong live 12:30-13:00 → kiri (baris asli), kanan (klon).
    let (m2, _) = tl(&app, tr, w.project, (12, 0), (14, 0), Some("APPROVED")).await;
    let s_left = shot(&app, m2, w.project, (12, 10)).await;
    let s_mid = shot(&app, m2, w.project, (12, 40)).await;
    let s_right = shot(&app, m2, w.project, (13, 30)).await;
    set_total(&app, tr, 7200).await;
    let r = post(&app, CREATE, tok, &at((12, 30), (13, 0))).await;
    let live2 = tl_id(&app, ok(&r)["data"]["uuid"].as_str().unwrap()).await;
    assert_eq!(tl_field(&app, m2, r#""endAt""#).await, sql_t(12, 30));
    assert_eq!(tl_of(&app, "screenshot", s_left).await, m2);
    assert_eq!(tl_of(&app, "screenshot", s_mid).await, live2);
    let right = tl_of(&app, "screenshot", s_right).await;
    assert_ne!(right, m2);
    assert_eq!(tl_field(&app, right, r#""startAt""#).await, sql_t(13, 0));
    assert_eq!(tl_field(&app, right, r#""endAt""#).await, sql_t(14, 0));
    assert_eq!(
        tl_field(&app, right, r#""manualTimeStatus""#).await,
        "APPROVED"
    );
    assert_eq!(
        tl_field(&app, right, r#""manualTimeApprovalSource""#).await,
        "AUTO"
    );
    assert_eq!(
        tl_field(&app, right, r#""approvedAt" IS NOT NULL"#).await,
        "true"
    );
    assert_eq!(
        tl_field(&app, right, r#""clickupTimeEntryId""#).await,
        "NULL"
    );
    assert_eq!(total(&app, tr).await, 7200);

    // Shrink kanan & kiri.
    let (m3, _) = tl(&app, tr, w.project, (15, 0), (16, 0), Some("APPROVED")).await;
    let r = post(&app, CREATE, tok, &at((14, 30), (15, 30))).await;
    ok(&r);
    assert_eq!(tl_field(&app, m3, r#""startAt""#).await, sql_t(15, 30));
    let (m4, _) = tl(&app, tr, w.project, (17, 0), (18, 0), Some("APPROVED")).await;
    ok(&post(&app, CREATE, tok, &at((17, 30), (18, 30))).await);
    assert_eq!(tl_field(&app, m4, r#""endAt""#).await, sql_t(17, 30));

    // Manual di project yang tak bisa menerima anak (company project tujuan bukan subset):
    // baris manual hanya dinonaktifkan, anak tidak dipindah.
    let other = world(&app, "ALL").await;
    let (m5, _) = tl(&app, tr, other.project, (19, 0), (19, 20), Some("APPROVED")).await;
    let s5 = shot(&app, m5, other.project, (19, 5)).await;
    let r = post(&app, CREATE, tok, &at((19, 0), (19, 30))).await;
    ok(&r);
    assert_eq!(tl_field(&app, m5, "status").await, "0");
    assert_eq!(tl_of(&app, "screenshot", s5).await, m5);

    // Dashboard menolak menimpa manual time.
    let (_, _) = tl(&app, tr, w.project, (21, 0), (21, 20), Some("APPROVED")).await;
    let r = post(
        &app,
        CREATE,
        tok,
        &body(
            &w,
            &tr_uuid,
            (21, 0),
            (21, 30),
            &json!({"from": "dashboard"}),
        ),
    )
    .await;
    fails(&r, "Overlap detected");
}

#[tokio::test]
async fn create_trims_approved_manual_in_other_tracking() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let w = world(&app, "ALL").await;
    let (tr1, _) = tracking(&app, w.staff.id, "2026-10-04").await;
    let (tr2, tr2_uuid) = tracking(&app, w.staff.id, D).await;
    let (m, _) = tl(&app, tr1, w.project, (1, 0), (2, 0), Some("APPROVED")).await;
    let s_in = shot(&app, m, w.project, (1, 45)).await;
    set_total(&app, tr1, 3600).await;
    let tok = &w.staff.token;

    // Dashboard menolak.
    let r = post(
        &app,
        CREATE,
        tok,
        &body(
            &w,
            &tr2_uuid,
            (1, 30),
            (2, 30),
            &json!({"from": "dashboard"}),
        ),
    )
    .await;
    fails(&r, "Overlap detected");
    assert_eq!(tl_field(&app, m, r#""endAt""#).await, sql_t(2, 0));

    let r = post(
        &app,
        CREATE,
        tok,
        &body(&w, &tr2_uuid, (1, 30), (2, 30), &json!({})),
    )
    .await;
    let live = tl_id(&app, ok(&r)["data"]["uuid"].as_str().unwrap()).await;
    assert_eq!(tl_field(&app, m, r#""endAt""#).await, sql_t(1, 30));
    assert_eq!(tl_of(&app, "screenshot", s_in).await, live);
    assert_eq!(
        total(&app, tr1).await,
        1800,
        "total tracking manual berkurang 30 menit"
    );
    assert_eq!(total(&app, tr2).await, 3600);
}

// ===========================================================================
// POST /:uuid/split, POST /:uuid/split-time

fn split_uri(u: &str) -> String {
    format!("/tracking-timelines/{u}/split")
}

fn split_time_uri(u: &str) -> String {
    format!("/tracking-timelines/{u}/split-time")
}

#[tokio::test]
async fn split_entry_branches() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let w = world(&app, "ALL").await;
    let (tr, _) = tracking(&app, w.staff.id, D).await;
    let tok = &w.manager.token;

    // Carve di tengah → 3 segmen; screenshot dipindah per jendela [start, end).
    let (id, uuid) = tl(&app, tr, w.project, (1, 0), (3, 0), None).await;
    let s0 = shot(&app, id, w.project, (1, 10)).await;
    let s1 = shot(&app, id, w.project, (1, 45)).await;
    let s2 = shot(&app, id, w.project, (2, 30)).await;
    app.exec(&format!(
        r#"UPDATE screenshot SET status = 0 WHERE id = {s2}"#
    ))
    .await;
    let s_del_stays = s2;
    set_total(&app, tr, 7200).await;
    let r = post(
        &app,
        &split_uri(&uuid),
        tok,
        &json!({"carveStartAt": t(1, 30), "carveEndAt": t(2, 0), "carvedBillable": false}),
    )
    .await;
    let j = ok(&r);
    assert_eq!(j["message"], "Entry split into 3 segments");
    let data = j["data"].as_array().unwrap();
    assert_eq!(data.len(), 3);
    assert_eq!(data[0]["billable"], json!(true));
    assert_eq!(data[1]["billable"], json!(false));
    assert_eq!(data[2]["billable"], json!(true));
    let ids: Vec<i32> = data
        .iter()
        .map(|d| d["id"].as_i64().unwrap() as i32)
        .collect();
    assert_eq!(tl_field(&app, id, "status").await, "0");
    assert_eq!(tl_of(&app, "screenshot", s0).await, ids[0]);
    assert_eq!(tl_of(&app, "screenshot", s1).await, ids[1]);
    assert_eq!(
        tl_of(&app, "screenshot", s_del_stays).await,
        id,
        "screenshot DELETED tidak ikut dipindah"
    );
    assert_eq!(tl_field(&app, ids[1], r#""startAt""#).await, sql_t(1, 30));
    assert_eq!(tl_field(&app, ids[2], r#""endAtUtc""#).await, sql_t(3, 0));
    assert_eq!(active_rows(&app, tr).await, 3);
    assert_eq!(total(&app, tr).await, 7200);

    // Carve dijepit ke batas entri → 2 segmen.
    let (id2, u2) = tl(&app, tr, w.project, (5, 0), (6, 0), None).await;
    let r = post(
        &app,
        &split_uri(&u2),
        tok,
        &json!({"carveStartAt": t(4, 0), "carveEndAt": t(5, 30)}),
    )
    .await;
    assert_eq!(ok(&r)["message"], "Entry split into 2 segments");
    assert_eq!(tl_field(&app, id2, "status").await, "0");

    // Gagal: tidak beririsan, PENDING, tidak ada, role, company lain.
    let (_, u3) = tl(&app, tr, w.project, (8, 0), (9, 0), None).await;
    fails(
        &post(
            &app,
            &split_uri(&u3),
            tok,
            &json!({"carveStartAt": t(10, 0), "carveEndAt": t(11, 0)}),
        )
        .await,
        "does not overlap",
    );
    let (_, u_pend) = tl(&app, tr, w.project, (12, 0), (13, 0), Some("PENDING")).await;
    // PENDING berstatus 0 → tidak ditemukan; ubah ke status 1 agar masuk cabang PENDING.
    app.exec(&format!(
        r#"UPDATE "trackingTimeline" SET status = 1 WHERE uuid = '{u_pend}'"#
    ))
    .await;
    fails(
        &post(
            &app,
            &split_uri(&u_pend),
            tok,
            &json!({"carveStartAt": t(12, 10), "carveEndAt": t(12, 20)}),
        )
        .await,
        "still pending approval",
    );
    let ghost = random_uuid(&app).await;
    fails(
        &post(
            &app,
            &split_uri(&ghost),
            tok,
            &json!({"carveStartAt": t(8, 10), "carveEndAt": t(8, 20)}),
        )
        .await,
        "Tracking timeline not found",
    );
    post(
        &app,
        &split_uri(&u3),
        &w.staff.token,
        &json!({"carveStartAt": t(8, 10), "carveEndAt": t(8, 20)}),
    )
    .await
    .assert_status(401);
    let other = world(&app, "ALL").await;
    fails(
        &post(
            &app,
            &split_uri(&u3),
            &other.manager.token,
            &json!({"carveStartAt": t(8, 10), "carveEndAt": t(8, 20)}),
        )
        .await,
        "Tracking timeline not found",
    );
    post(&app, &split_uri(&u3), tok, &json!({"carveStartAt": "x"}))
        .await
        .assert_status(400);
    assert_eq!(active_rows(&app, tr).await, 3 + 2 + 1 + 1);
}

#[tokio::test]
async fn split_time_branches() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let w = world(&app, "ALL").await;
    let (p2, p2_uuid) = project(&app, w.cid, false).await;
    let (tr, _) = tracking(&app, w.staff.id, D).await;
    let tok = &w.manager.token;
    let seg = |from: (u32, u32), to: (u32, u32), pu: &str, extra: Value| {
        let mut s =
            json!({"startAt": t(from.0, from.1), "endAt": t(to.0, to.1), "projectUuid": pu});
        if let (Some(b), Some(e)) = (s.as_object_mut(), extra.as_object()) {
            for (k, v) in e {
                b.insert(k.clone(), v.clone());
            }
        }
        s
    };

    // Dua segmen beda project; segmen dikirim tak berurutan; batas persis masuk segmen terakhir.
    let (id, uuid) = tl(&app, tr, w.project, (1, 0), (3, 0), None).await;
    let s_a = shot(&app, id, w.project, (1, 30)).await;
    let s_mid = shot(&app, id, w.project, (2, 0)).await;
    let s_end = shot(&app, id, w.project, (3, 0)).await;
    set_total(&app, tr, 7200).await;
    let r = post(
        &app,
        &split_time_uri(&uuid),
        tok,
        &json!({"segments": [
            seg((2, 0), (3, 0), &p2_uuid, json!({})),
            seg((1, 0), (2, 0), &w.project_uuid, json!({"billable": false, "note": "bagian A"})),
        ]}),
    )
    .await;
    let j = ok(&r);
    assert_eq!(j["message"], "Entry successfully split into 2 segments");
    let data = j["data"].as_array().unwrap();
    assert_eq!(data[0]["projectUuid"], json!(w.project_uuid));
    assert_eq!(data[0]["billable"], json!(false));
    assert_eq!(
        data[1]["billable"],
        json!(true),
        "billable mewarisi entri asli"
    );
    let new_a = tl_id(&app, data[0]["uuid"].as_str().unwrap()).await;
    let new_b = tl_id(&app, data[1]["uuid"].as_str().unwrap()).await;
    assert_eq!(tl_field(&app, new_a, "note").await, "bagian A");
    assert_eq!(tl_field(&app, new_b, "note").await, "seed");
    assert_eq!(
        tl_field(&app, new_b, r#""projectId""#).await,
        p2.to_string()
    );
    assert_eq!(tl_of(&app, "screenshot", s_a).await, new_a);
    assert_eq!(tl_of(&app, "screenshot", s_mid).await, new_b);
    assert_eq!(tl_of(&app, "screenshot", s_end).await, new_b);
    assert_eq!(tl_field(&app, id, "status").await, "0");
    assert_eq!(total(&app, tr).await, 7200);

    // Manual APPROVED: segmen mewarisi status/approver.
    let (m, mu) = tl(&app, tr, w.project, (6, 0), (8, 0), Some("APPROVED")).await;
    app.exec(&format!(
        r#"UPDATE "trackingTimeline" SET "approvedById" = {} WHERE id = {m}"#,
        w.manager.id
    ))
    .await;
    let r = post(
        &app,
        &split_time_uri(&mu),
        tok,
        &json!({"segments": [
            seg((6, 0), (7, 0), &w.project_uuid, json!({})),
            seg((7, 0), (8, 0), &w.project_uuid, json!({})),
        ]}),
    )
    .await;
    let j = ok(&r);
    let first = tl_id(&app, j["data"][0]["uuid"].as_str().unwrap()).await;
    assert_eq!(
        tl_field(&app, first, r#""manualTimeStatus""#).await,
        "APPROVED"
    );
    assert_eq!(
        tl_field(&app, first, r#""manualTimeApprovalSource""#).await,
        "AUTO"
    );
    assert_eq!(
        tl_field(&app, first, r#""approvedById""#).await,
        w.manager.id.to_string()
    );
    assert_eq!(tl_field(&app, m, "status").await, "0");

    // Gagal: gap, project tak ada / company lain, batas awal jauh, validasi, role.
    let (_, u4) = tl(&app, tr, w.project, (10, 0), (12, 0), None).await;
    fails(
        &post(
            &app,
            &split_time_uri(&u4),
            tok,
            &json!({"segments": [
                seg((10, 0), (10, 30), &w.project_uuid, json!({})),
                seg((11, 0), (12, 0), &w.project_uuid, json!({})),
            ]}),
        )
        .await,
        "Gap detected",
    );
    let other = world(&app, "ALL").await;
    fails(
        &post(
            &app,
            &split_time_uri(&u4),
            tok,
            &json!({"segments": [
                seg((10, 0), (11, 0), &w.project_uuid, json!({})),
                seg((11, 0), (12, 0), &other.project_uuid, json!({})),
            ]}),
        )
        .await,
        "not found or inactive",
    );
    fails(
        &post(
            &app,
            &split_time_uri(&u4),
            tok,
            &json!({"segments": [
                seg((9, 0), (11, 0), &w.project_uuid, json!({})),
                seg((11, 0), (12, 0), &w.project_uuid, json!({})),
            ]}),
        )
        .await,
        "First segment must start at",
    );
    post(
        &app,
        &split_time_uri(&u4),
        tok,
        &json!({"segments": [seg((10, 0), (12, 0), &w.project_uuid, json!({}))]}),
    )
    .await
    .assert_status(400);
    let two = json!({"segments": [
        seg((10, 0), (11, 0), &w.project_uuid, json!({})),
        seg((11, 0), (12, 0), &w.project_uuid, json!({})),
    ]});
    post(&app, &split_time_uri(&u4), &w.staff.token, &two)
        .await
        .assert_status(401);
    fails(
        &post(&app, &split_time_uri(&u4), &other.manager.token, &two).await,
        "Tracking timeline not found",
    );
    let ghost = random_uuid(&app).await;
    fails(
        &post(&app, &split_time_uri(&ghost), tok, &two).await,
        "Tracking timeline not found",
    );
    assert_eq!(tl_field(&app, tl_id(&app, &u4).await, "status").await, "1");
}

// ===========================================================================
// POST /tracking-timelines/manual-time

const MANUAL: &str = "/tracking-timelines/manual-time";

/// Tanggal UTC `n` hari lalu.
async fn days_ago(app: &TestApp, n: i32) -> String {
    app.scalar(&format!(
        "SELECT ((now() AT TIME ZONE 'UTC')::date - {n})::text"
    ))
    .await
}

fn mbody(
    w: &W,
    tr_uuid: &str,
    day: &str,
    from: (u32, u32),
    to: (u32, u32),
    extra: &Value,
) -> Value {
    let mut b = json!({
        "startAt": format!("{day}T{:02}:{:02}:00.000Z", from.0, from.1),
        "endAt": format!("{day}T{:02}:{:02}:00.000Z", to.0, to.1),
        "timezone": "UTC",
        "offsetMinutes": 0,
        "projectUuid": w.project_uuid,
        "trackingUuid": tr_uuid,
        "reason": "lupa menyalakan tracker",
    });
    if let (Some(base), Some(add)) = (b.as_object_mut(), extra.as_object()) {
        for (k, v) in add {
            if v.is_null() {
                base.remove(k);
            } else {
                base.insert(k.clone(), v.clone());
            }
        }
    }
    b
}

/// `(id, manualTimeStatus, status)` dari respons manual time.
async fn manual_row(app: &TestApp, r: &TestResponse) -> (i32, String, String) {
    let uuid = r.json["data"]["uuid"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(!uuid.is_empty(), "tidak ada data.uuid: {}", r.json);
    let id = tl_id(app, &uuid).await;
    (
        id,
        tl_field(app, id, r#""manualTimeStatus""#).await,
        tl_field(app, id, "status").await,
    )
}

#[tokio::test]
async fn manual_pending_creates_tokens_and_attachments() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let w = world(&app, "ALL").await;
    let day = days_ago(&app, 1).await;
    let (tr, tr_uuid) = tracking(&app, w.staff.id, &day).await;
    app.exec(&format!(
        r#"INSERT INTO "projectManualTimeApprover" ("projectId", "userId") VALUES ({}, {}), ({}, {})"#,
        w.project, w.manager.id, w.project, w.admin.id
    ))
    .await;

    let r = post(
        &app,
        MANUAL,
        &w.staff.token,
        &mbody(&w, &tr_uuid, &day, (10, 0), (11, 0), &json!({"note": "n1"})),
    )
    .await;
    let j = ok(&r);
    assert_eq!(j["message"], "Manual time submitted, waiting for approval");
    assert_eq!(j["data"]["manualTimeStatus"], "PENDING");
    assert_eq!(j["data"]["approvedAt"], Value::Null);
    assert_eq!(j["data"]["reason"], "lupa menyalakan tracker");
    let (id, status, st) = manual_row(&app, &r).await;
    assert_eq!((status.as_str(), st.as_str()), ("PENDING", "0"));
    assert_eq!(tl_field(&app, id, r#""isOffline""#).await, "true");
    assert_eq!(tl_field(&app, id, "note").await, "n1");
    assert_eq!(tl_field(&app, id, "billable").await, "true");
    assert_eq!(
        tl_field(&app, id, r#""requestedAt" IS NOT NULL"#).await,
        "true"
    );
    assert_eq!(total(&app, tr).await, 0);

    // Token approver dibuat di latar (spawn): satu per approver.
    let mut tokens = 0;
    for _ in 0..50 {
        tokens = count(
            &app,
            &format!(
                r#"SELECT 1 FROM "manualTimeApprovalToken" WHERE "trackingTimelineId" = {id}"#
            ),
        )
        .await;
        if tokens == 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(tokens, 2, "satu manualTimeApprovalToken per approver");
    assert_eq!(
        count(
            &app,
            &format!(
                r#"SELECT 1 FROM "manualTimeApprovalToken" WHERE "trackingTimelineId" = {id} AND "approverId" = {}
                   AND "expiredAt" > now() AND "usedAt" IS NULL"#,
                w.manager.id
            )
        )
        .await,
        1
    );

    // Duplikat (project + tracking + start + end sama).
    let r = post(
        &app,
        MANUAL,
        &w.staff.token,
        &mbody(&w, &tr_uuid, &day, (10, 0), (11, 0), &json!({})),
    )
    .await;
    fails(&r, "same start and end time already exists");
    assert_eq!(
        count(
            &app,
            &format!(r#"SELECT 1 FROM "trackingTimeline" WHERE "trackingId" = {tr}"#)
        )
        .await,
        1
    );

    // Lampiran: baris ikut transaksi entri (UNNEST). Presigned URL butuh kredensial GCS;
    // di lingkungan uji tanpa kredensial respons bisa gagal SETELAH commit.
    let atts = json!([
        {"fileName": "bukti.png", "mimeType": "image/png", "fileSize": 1234},
        {"fileName": "surat.pdf", "mimeType": "application/pdf", "fileSize": 4096},
    ]);
    let r = post(
        &app,
        MANUAL,
        &w.staff.token,
        &mbody(
            &w,
            &tr_uuid,
            &day,
            (12, 0),
            (13, 0),
            &json!({"attachments": atts, "billable": false}),
        ),
    )
    .await;
    let new_id: i32 = app
        .scalar(&format!(
            r#"SELECT id FROM "trackingTimeline" WHERE "trackingId" = {tr} AND "startAt"::time = '12:00:00'"#
        ))
        .await;
    assert_eq!(tl_field(&app, new_id, "billable").await, "false");
    assert_eq!(
        count(&app, &format!(r#"SELECT 1 FROM "manualTimeAttachment" WHERE "trackingTimelineId" = {new_id} AND status = 1"#)).await,
        2
    );
    assert_eq!(
        count(
            &app,
            &format!(
                r#"SELECT 1 FROM "manualTimeAttachment" a JOIN "trackingTimeline" t ON t.id = a."trackingTimelineId"
                   WHERE t.id = {new_id} AND a."fileFullpath" LIKE 'manual-time/' || t.uuid::text || '/%_bukti.png'
                     AND a."fileSize" = 1234 AND a."mimeType" = 'image/png'"#
            )
        )
        .await,
        1
    );
    if r.json["success"] == json!(true) {
        assert_eq!(
            r.json["data"]["attachments"].as_array().map(Vec::len),
            Some(2)
        );
    } else {
        // Sama dengan Node: presign dilakukan SESUDAH commit, jadi galat GCS tidak membatalkan baris.
        assert!(msg(&r).contains("GCP_BUCKET_NAME"), "{}", r.json);
    }
}

#[tokio::test]
async fn manual_validation_branches() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let w = world(&app, "NONE").await;
    let day = days_ago(&app, 1).await;
    let (tr, tr_uuid) = tracking(&app, w.staff.id, &day).await;
    let tok = &w.staff.token;
    let m = |from, to, extra: Value| mbody(&w, &tr_uuid, &day, from, to, &extra);

    fails(
        &post(&app, MANUAL, tok, &m((11, 0), (11, 0), json!({}))).await,
        "Invalid endtime",
    );
    fails(
        &post(&app, MANUAL, tok, &m((11, 0), (10, 0), json!({}))).await,
        "Invalid endtime",
    );
    fails(
        &post(&app, MANUAL, tok, &m((0, 0), (15, 0), json!({}))).await,
        "Duration exceeds maximum limit of 14 hours",
    );
    let six: Vec<Value> = (0..6)
        .map(|i| json!({"fileName": format!("f{i}.png"), "mimeType": "image/png", "fileSize": 1}))
        .collect();
    fails(
        &post(
            &app,
            MANUAL,
            tok,
            &m((10, 0), (11, 0), json!({"attachments": six})),
        )
        .await,
        "Maximum 5 attachments allowed",
    );
    fails(
        &post(
            &app,
            MANUAL,
            tok,
            &m((10, 0), (11, 0), json!({"attachments": [{"fileName": "a.exe", "mimeType": "application/x-msdownload", "fileSize": 1}]})),
        )
        .await,
        "File type not allowed",
    );
    fails(
        &post(
            &app,
            MANUAL,
            tok,
            &m((10, 0), (11, 0), json!({"attachments": [{"fileName": "big.pdf", "mimeType": "application/pdf", "fileSize": 11 * 1024 * 1024}]})),
        )
        .await,
        "exceeds 10MB limit",
    );
    // Berakhir di masa depan.
    let tomorrow = days_ago(&app, -1).await;
    fails(
        &post(
            &app,
            MANUAL,
            tok,
            &mbody(&w, &tr_uuid, &tomorrow, (10, 0), (11, 0), &json!({})),
        )
        .await,
        "cannot end in the future",
    );
    let ghost = random_uuid(&app).await;
    fails(
        &post(
            &app,
            MANUAL,
            tok,
            &m((10, 0), (11, 0), json!({"projectUuid": ghost})),
        )
        .await,
        "Project UUID not found",
    );
    fails(
        &post(
            &app,
            MANUAL,
            tok,
            &m((10, 0), (11, 0), json!({"trackingUuid": ghost})),
        )
        .await,
        "Tracking UUID not found",
    );
    fails(
        &post(
            &app,
            MANUAL,
            &w.staff2.token,
            &m((10, 0), (11, 0), json!({})),
        )
        .await,
        "not allowed to manage tracking timeline",
    );
    let other = world(&app, "NONE").await;
    fails(
        &post(
            &app,
            MANUAL,
            tok,
            &m((10, 0), (11, 0), json!({"projectUuid": other.project_uuid})),
        )
        .await,
        "Project UUID not found",
    );
    // Validasi DTO (reason wajib).
    post(
        &app,
        MANUAL,
        tok,
        &m((10, 0), (11, 0), json!({"reason": ""})),
    )
    .await
    .assert_status(400);
    assert_eq!(
        count(
            &app,
            &format!(r#"SELECT 1 FROM "trackingTimeline" WHERE "trackingId" = {tr}"#)
        )
        .await,
        0
    );
}

#[tokio::test]
async fn manual_auto_approve_and_trim() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let w = world(&app, "NONE").await;
    let u = user(&app, w.cid, "STAFF", true, false).await;
    let d1 = days_ago(&app, 1).await;
    let d2 = days_ago(&app, 2).await;
    let d3 = days_ago(&app, 3).await;
    let tok = &u.token;
    let (tr1, tr1_uuid) = tracking(&app, u.id, &d1).await;
    let (tr2, tr2_uuid) = tracking(&app, u.id, &d2).await;
    let (tr3, tr3_uuid) = tracking(&app, u.id, &d3).await;

    // Disetujui otomatis: APPROVED/AUTO, status 1, total naik.
    let r = post(
        &app,
        MANUAL,
        tok,
        &mbody(&w, &tr1_uuid, &d1, (10, 0), (11, 0), &json!({})),
    )
    .await;
    let j = ok(&r);
    assert_eq!(
        j["message"],
        "Manual time submitted and approved automatically"
    );
    assert_eq!(j["data"]["manualTimeStatus"], "APPROVED");
    assert_eq!(j["data"]["manualTimeApprovalSource"], "AUTO");
    assert!(j["data"]["approvedAt"].is_string());
    let (id, status, st) = manual_row(&app, &r).await;
    assert_eq!((status.as_str(), st.as_str()), ("APPROVED", "1"));
    assert_eq!(tl_field(&app, id, r#""approvedById""#).await, "NULL");
    assert_eq!(total(&app, tr1).await, 3600);
    // Duplikat.
    fails(
        &post(
            &app,
            MANUAL,
            tok,
            &mbody(&w, &tr1_uuid, &d1, (10, 0), (11, 0), &json!({})),
        )
        .await,
        "already exists",
    );
    assert_eq!(total(&app, tr1).await, 3600);

    // Sebagian sudah terekam (live 10:00-10:30) → manual 09:30-10:30 dipangkas jadi 09:30-10:00.
    let day_sql = |d: &str, h: u32, m: u32| format!("{d} {h:02}:{m:02}:00");
    tl_at(
        &app,
        tr2,
        w.project,
        &day_sql(&d2, 10, 0),
        &day_sql(&d2, 10, 30),
    )
    .await;
    set_total(&app, tr2, 1800).await;
    let r = post(
        &app,
        MANUAL,
        tok,
        &mbody(&w, &tr2_uuid, &d2, (9, 30), (10, 30), &json!({})),
    )
    .await;
    let j = ok(&r);
    assert_eq!(
        j["message"],
        "Manual time submitted and approved automatically; the part already recorded was removed"
    );
    let (id, status, _) = manual_row(&app, &r).await;
    assert_eq!(status, "APPROVED");
    assert_eq!(
        tl_field(&app, id, r#""startAt""#).await,
        day_sql(&d2, 9, 30)
    );
    assert_eq!(tl_field(&app, id, r#""endAt""#).await, day_sql(&d2, 10, 0));
    assert_eq!(total(&app, tr2).await, 1800 + 1800);
    // Sepenuhnya tertutup → PENDING ("covered").
    let r = post(
        &app,
        MANUAL,
        tok,
        &mbody(&w, &tr2_uuid, &d2, (10, 5), (10, 25), &json!({})),
    )
    .await;
    ok(&r);
    let (_, status, st) = manual_row(&app, &r).await;
    assert_eq!((status.as_str(), st.as_str()), ("PENDING", "0"));
    // Live di tengah → dua potongan (baris asli + klon).
    tl_at(
        &app,
        tr2,
        w.project,
        &day_sql(&d2, 13, 0),
        &day_sql(&d2, 13, 30),
    )
    .await;
    let before = total(&app, tr2).await;
    let r = post(
        &app,
        MANUAL,
        tok,
        &mbody(&w, &tr2_uuid, &d2, (12, 0), (14, 0), &json!({})),
    )
    .await;
    ok(&r);
    assert_eq!(
        count(
            &app,
            &format!(
                r#"SELECT 1 FROM "trackingTimeline" WHERE "trackingId" = {tr2} AND "manualTimeStatus" = 'APPROVED'
                   AND status = 1 AND "startAt"::time IN ('12:00:00', '13:30:00')"#
            )
        )
        .await,
        2
    );
    assert_eq!(total(&app, tr2).await, before + 3600 + 1800);

    // Zona waktu hilang → PENDING (missing-timezone); entri lama > 7 hari → PENDING (too-old).
    let r = post(
        &app,
        MANUAL,
        tok,
        &mbody(
            &w,
            &tr3_uuid,
            &d3,
            (10, 0),
            (11, 0),
            &json!({"timezone": Value::Null, "offsetMinutes": Value::Null}),
        ),
    )
    .await;
    ok(&r);
    assert_eq!(manual_row(&app, &r).await.1, "PENDING");
    let old = days_ago(&app, 10).await;
    let r = post(
        &app,
        MANUAL,
        tok,
        &mbody(&w, &tr3_uuid, &old, (10, 0), (11, 0), &json!({})),
    )
    .await;
    ok(&r);
    assert_eq!(manual_row(&app, &r).await.1, "PENDING");
    assert_eq!(total(&app, tr3).await, 0);
}

#[tokio::test]
async fn manual_pending_reasons_budget_exception_and_work_limit() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let w = world(&app, "NONE").await;
    let day = days_ago(&app, 1).await;

    // Tidak diizinkan (isAllowManualTime false) → PENDING.
    let (_, t_staff) = tracking(&app, w.staff.id, &day).await;
    let r = post(
        &app,
        MANUAL,
        &w.staff.token,
        &mbody(&w, &t_staff, &day, (10, 0), (11, 0), &json!({})),
    )
    .await;
    ok(&r);
    assert_eq!(manual_row(&app, &r).await.1, "PENDING");

    // Daftar pengecualian tanpa batas → PENDING.
    let exc = user(&app, w.cid, "STAFF", true, false).await;
    app.exec(&format!(
        r#"INSERT INTO "manualTimeApprovalException" ("companyId", "userId") VALUES ({}, {})"#,
        w.cid, exc.id
    ))
    .await;
    let (_, t_exc) = tracking(&app, exc.id, &day).await;
    let r = post(
        &app,
        MANUAL,
        &exc.token,
        &mbody(&w, &t_exc, &day, (10, 0), (11, 0), &json!({})),
    )
    .await;
    ok(&r);
    assert_eq!(manual_row(&app, &r).await.1, "PENDING");

    // Budget STOP_TRACKING tercapai → PENDING (hanya STAFF).
    let wb = world(&app, "NONE").await;
    let budget_user = user(&app, wb.cid, "STAFF", true, false).await;
    let (_, t_bu) = tracking(&app, budget_user.id, &day).await;
    app.exec(&format!(
        r#"INSERT INTO "projectBudget" ("projectId", "isEnabled", "budgetType", "budgetHours", "currentBurnedSeconds",
             enforcement) VALUES ({}, true, 'HOURS', 1, 3600, 'STOP_TRACKING')"#,
        wb.project
    ))
    .await;
    let r = post(
        &app,
        MANUAL,
        &budget_user.token,
        &mbody(&wb, &t_bu, &day, (10, 0), (11, 0), &json!({})),
    )
    .await;
    ok(&r);
    assert_eq!(manual_row(&app, &r).await.1, "PENDING");

    // Batas jam kerja harian 30 menit: manual 1 jam → PENDING (work-limit); batas 2 jam → APPROVED.
    let limited = user(&app, w.cid, "STAFF", true, false).await;
    let roomy = user(&app, w.cid, "STAFF", true, false).await;
    app.exec(&format!(
        r#"INSERT INTO "userWorkTimeLimit" ("userId", "companyId", "dailyLimitSec", "weeklyLimitSec", status)
           VALUES ({}, {cid}, 1800, 100000, 1), ({}, {cid}, 7200, 100000, 1)"#,
        limited.id,
        roomy.id,
        cid = w.cid
    ))
    .await;
    let (_, t_lim) = tracking(&app, limited.id, &day).await;
    let (tr_roomy, t_roomy) = tracking(&app, roomy.id, &day).await;
    let r = post(
        &app,
        MANUAL,
        &limited.token,
        &mbody(&w, &t_lim, &day, (10, 0), (11, 0), &json!({})),
    )
    .await;
    ok(&r);
    assert_eq!(manual_row(&app, &r).await.1, "PENDING");
    let r = post(
        &app,
        MANUAL,
        &roomy.token,
        &mbody(&w, &t_roomy, &day, (10, 0), (11, 0), &json!({})),
    )
    .await;
    ok(&r);
    assert_eq!(manual_row(&app, &r).await.1, "APPROVED");
    assert_eq!(total(&app, tr_roomy).await, 3600);
}

#[tokio::test]
async fn manual_limit_mode_quota() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let w = world(&app, "LIMIT").await;
    app.exec(&format!(
        r#"UPDATE company SET "manualTimeAutoApproveLimitMinutes" = 60, "manualTimeAutoApprovePeriod" = 'DAY' WHERE id = {}"#,
        w.cid
    ))
    .await;
    let day = days_ago(&app, 1).await;
    let u = user(&app, w.cid, "STAFF", true, false).await;
    let (tr, tr_uuid) = tracking(&app, u.id, &day).await;
    let m = |from, to| mbody(&w, &tr_uuid, &day, from, to, &json!({}));

    // 30 menit: muat. 45 menit: sisa 30 → PENDING. 20 menit: muat.
    let r = post(&app, MANUAL, &u.token, &m((10, 0), (10, 30))).await;
    ok(&r);
    assert_eq!(manual_row(&app, &r).await.1, "APPROVED");
    let r = post(&app, MANUAL, &u.token, &m((11, 0), (11, 45))).await;
    ok(&r);
    assert_eq!(manual_row(&app, &r).await.1, "PENDING");
    let r = post(&app, MANUAL, &u.token, &m((12, 0), (12, 20))).await;
    ok(&r);
    assert_eq!(manual_row(&app, &r).await.1, "APPROVED");
    assert_eq!(total(&app, tr).await, 1800 + 1200);

    // Pengecualian dengan batas sendiri (15 menit).
    let ex = user(&app, w.cid, "STAFF", true, false).await;
    app.exec(&format!(
        r#"INSERT INTO "manualTimeApprovalException" ("companyId", "userId", "limitMinutes") VALUES ({}, {}, 15)"#,
        w.cid, ex.id
    ))
    .await;
    let (_, ex_uuid) = tracking(&app, ex.id, &day).await;
    let me = |from, to| mbody(&w, &ex_uuid, &day, from, to, &json!({}));
    let r = post(&app, MANUAL, &ex.token, &me((10, 0), (10, 20))).await;
    ok(&r);
    assert_eq!(manual_row(&app, &r).await.1, "PENDING");
    let r = post(&app, MANUAL, &ex.token, &me((10, 30), (10, 40))).await;
    ok(&r);
    assert_eq!(manual_row(&app, &r).await.1, "APPROVED");

    // Periode pekanan.
    app.exec(&format!(
        r#"UPDATE company SET "manualTimeAutoApprovePeriod" = 'WEEK' WHERE id = {}"#,
        w.cid
    ))
    .await;
    let wk = user(&app, w.cid, "STAFF", true, false).await;
    let (_, wk_uuid) = tracking(&app, wk.id, &day).await;
    let mw = |from, to| mbody(&w, &wk_uuid, &day, from, to, &json!({}));
    let r = post(&app, MANUAL, &wk.token, &mw((10, 0), (10, 40))).await;
    ok(&r);
    assert_eq!(manual_row(&app, &r).await.1, "APPROVED");
    let r = post(&app, MANUAL, &wk.token, &mw((11, 0), (11, 30))).await;
    ok(&r);
    assert_eq!(manual_row(&app, &r).await.1, "PENDING");

    // Mode LIMIT tanpa batas menit (quota-unavailable) → PENDING.
    let w2 = world(&app, "LIMIT").await;
    let u2 = user(&app, w2.cid, "STAFF", true, false).await;
    let (_, u2_uuid) = tracking(&app, u2.id, &day).await;
    let r = post(
        &app,
        MANUAL,
        &u2.token,
        &mbody(&w2, &u2_uuid, &day, (10, 0), (10, 30), &json!({})),
    )
    .await;
    ok(&r);
    assert_eq!(manual_row(&app, &r).await.1, "PENDING");
}
