//! Uji integrasi `mz-timesheets` (30 route) terhadap Postgres NYATA.
//! Jalankan: `MZ_TEST_DATABASE_URL=postgres://… cargo test -p mz-server --test it_timesheets`
//!
//! Catatan: email (approval / status) dikirim fire-and-forget oleh `mz_core::mail::spawn_send` ke
//! Mailtrap dengan token kosong (ditolak 401, hasil ditelan). Itu tidak bisa dimatikan dari sini.
//! `PATCH /timesheet-invoices/:uuid/status` ke PAID meng-`await` panggilan Mailtrap; supaya uji
//! tidak menyentuh jaringan, pemilik invoice diberi `username` kosong (Node/Rust melewati email
//! bila kosong), jadi pengiriman email PAID itu sendiri tidak diuji.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::atomic::{AtomicU32, Ordering};

use mz_testkit::{Company, TestApp};
use serde_json::{Value, json};

static SEQ: AtomicU32 = AtomicU32::new(1);

fn next() -> u32 {
    SEQ.fetch_add(1, Ordering::Relaxed)
}

/// Kunci objek menurut urutan di respons (untuk uji urutan kunci = Node).
fn key_list(v: &Value) -> Vec<&str> {
    v.as_object()
        .unwrap_or_else(|| panic!("bukan objek: {v}"))
        .keys()
        .map(String::as_str)
        .collect()
}

fn uniq(prefix: &str) -> String {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("{prefix}{}x{t}x{}", std::process::id(), next())
}

#[derive(Clone)]
struct U {
    id: i32,
    uuid: String,
    token: String,
}

async fn prep(app: &TestApp) -> Company {
    let fx = app.company().await;
    app.exec(&format!(
        r#"UPDATE company SET "useTimesheetApproval" = true WHERE id = {}"#,
        fx.id
    ))
    .await;
    // Template anonim belum memberi STAFF izin fitur `timesheets` (produksi: role-templates / override-backfill
    // Node memberi READ+WRITE+DELETE).
    app.exec(
        r#"INSERT INTO "rolePermission" ("accessLevelId", "featureKey", action)
           SELECT al.id, 'timesheets', a.action FROM "accessLevel" al,
                  (VALUES ('READ'::"PermissionAction"), ('WRITE'), ('DELETE')) AS a(action)
           WHERE al.code = 'STAFF' AND al."isSystem"
           ON CONFLICT DO NOTHING"#,
    )
    .await;
    fx
}

/// User baru di company fixture dengan role sistem `role`.
async fn user(app: &TestApp, cid: i32, role: &str, username: Option<&str>) -> U {
    let n = uniq("its");
    let uname = username.map_or_else(|| format!("{n}@it-ts.test"), str::to_owned);
    let id: i32 = app
        .scalar(&format!(
            r#"INSERT INTO users (username, password, name, status)
               VALUES ('{uname}', 'x', 'ITS User {n}', 1) RETURNING id"#
        ))
        .await;
    app.exec(&format!(
        r#"INSERT INTO "userCompany" ("userId", "companyId", status, "isAllowManualTime", "isAllowEditTime",
             "idleTimeInSecond", "isDefault") VALUES ({id}, {cid}, 1, false, false, 300, true);
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

async fn pay_rate(
    app: &TestApp,
    uid: i32,
    cid: i32,
    period: &str,
    approval: bool,
    rate: f64,
    rt: &str,
) {
    app.exec(&format!(
        r#"INSERT INTO "userPayRate" ("userId", "companyId", "payPeriodType", "timesheetApproval", "payRate",
             "rateType", "effectiveDate", status)
           VALUES ({uid}, {cid}, '{period}'::"PayPeriodType", {approval}, {rate}, '{rt}'::"RateType",
             '2020-01-01', 1)"#
    ))
    .await;
}

async fn project(app: &TestApp, cid: i32, name: &str, billable: bool) -> i32 {
    let n = uniq("P");
    let id: i32 = app
        .scalar(&format!(
            r#"INSERT INTO project (code, name, "screenshotPerHour", "projectTypeId", "startDate", status, billable)
               SELECT '{n}', '{name}', 3, (SELECT id FROM "projectType" ORDER BY id LIMIT 1),
                      '2026-01-01', 1, {billable} RETURNING id"#
        ))
        .await;
    app.exec(&format!(
        r#"INSERT INTO "projectCompany" ("projectId", "companyId", status) VALUES ({id}, {cid}, 1)"#
    ))
    .await;
    id
}

/// Satu timeline `from`..`to` (jam) pada `date` (SQL date expr, mis. `'2026-03-02'`).
async fn seed(
    app: &TestApp,
    uid: i32,
    project: i32,
    date: &str,
    from: u32,
    to: u32,
    billable: bool,
) {
    let tr: i32 = app
        .scalar(&format!(
            r#"INSERT INTO tracking ("takenDate", "userId", status, "totalTakenInSecond", "isAbsent")
               VALUES ({date}, {uid}, 1, {}, false)
               ON CONFLICT ("userId", "takenDate") DO UPDATE SET status = 1 RETURNING id"#,
            (to - from) * 3600
        ))
        .await;
    app.exec(&format!(
        r#"INSERT INTO "trackingTimeline" (note, "isOffline", "startAt", "endAt", "startAtUtc", "endAtUtc",
             timezone, "offsetMinutes", "trackingId", "projectId", status, billable)
           VALUES ('seed', false, ({date}::date + time '{from:02}:00'), ({date}::date + time '{to:02}:00'),
             ({date}::date + time '{from:02}:00'), ({date}::date + time '{to:02}:00'),
             'UTC', 0, {tr}, {project}, 1, {billable})"#
    ))
    .await;
}

const CUR_MONTH: &str = "date_trunc('month', now() at time zone 'utc')::date";

async fn ts_id(app: &TestApp, uuid: &str) -> i32 {
    app.scalar(&format!(
        "SELECT id FROM timesheet WHERE uuid = '{uuid}'::uuid"
    ))
    .await
}

/// Timesheet APPROVED untuk `u` lewat admin/create + approve (periode eksplisit).
async fn approved_ts(app: &TestApp, fx: &Company, u: &U, start: &str, end: &str) -> String {
    let r = app
        .post("/timesheets/admin/create")
        .token(&fx.manager.token)
        .json(&json!({"userUuid": u.uuid, "periodStart": start, "periodEnd": end, "payPeriodType": "MONTHLY"}))
        .send()
        .await;
    r.assert_status(201).assert_ok();
    let uuid = r.data()["uuid"].as_str().unwrap().to_owned();
    app.patch(&format!("/timesheets/{uuid}/approve"))
        .token(&fx.admin.token)
        .send()
        .await
        .assert_ok();
    uuid
}

// ───────────────────────── employee flow ─────────────────────────

#[tokio::test]
async fn submit_current_my_withdraw_flow() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = prep(&app).await;
    let s = user(&app, fx.id, "STAFF", None).await;

    // tanpa pay rate
    let r = app
        .get("/timesheets/my/current")
        .token(&s.token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json["success"], false);
    assert_eq!(r.json["message"], "Pay rate not configured");
    let r = app
        .post("/timesheets/submit")
        .token(&s.token)
        .json(&json!({}))
        .send()
        .await;
    assert_eq!(r.json["message"], "Pay rate not configured");

    // approval nonaktif di akun
    pay_rate(&app, s.id, fx.id, "MONTHLY", false, 10.0, "HOURLY").await;
    let r = app
        .post("/timesheets/submit")
        .token(&s.token)
        .json(&json!({}))
        .send()
        .await;
    assert_eq!(
        r.json["message"],
        "Timesheet approval is not enabled for your account"
    );
    app.exec(&format!(
        r#"UPDATE "userPayRate" SET "timesheetApproval" = true WHERE "userId" = {}"#,
        s.id
    ))
    .await;

    let p = project(&app, fx.id, &uniq("Proj"), true).await;
    seed(&app, s.id, p, CUR_MONTH, 9, 11, true).await;

    let r = app
        .get("/timesheets/my/current")
        .token(&s.token)
        .send()
        .await;
    r.assert_ok();
    assert_eq!(r.data()["status"], "NOT_SUBMITTED");
    assert_eq!(r.data()["totalSeconds"], 7200);
    assert_eq!(r.data()["estimatedAmount"], 20);
    assert!(r.data()["timesheet"].is_null());

    let r = app
        .post("/timesheets/submit")
        .token(&s.token)
        .json(&json!({"notes": "tolong cek"}))
        .send()
        .await;
    r.assert_status(201).assert_ok();
    let d = r.data();
    assert_eq!(d["status"], "PENDING");
    assert_eq!(d["totalSeconds"], 7200);
    assert_eq!(d["totalAmount"], 20);
    assert_eq!(d["notes"], "tolong cek");
    let uuid = d["uuid"].as_str().unwrap().to_owned();
    let id = ts_id(&app, &uuid).await;
    let tokens: i64 = app
        .scalar(&format!(
            r#"SELECT count(*) FROM "timesheetApprovalToken" WHERE "timesheetId" = {id}"#
        ))
        .await;
    assert!(tokens >= 3, "token approver per approver: {tokens}");

    // submit ulang ditolak
    let r = app
        .post("/timesheets/submit")
        .token(&s.token)
        .json(&json!({}))
        .send()
        .await;
    r.assert_status(201);
    assert_eq!(
        r.json["message"],
        "You already have a pending or approved timesheet for this period"
    );

    let r = app
        .get("/timesheets/my/current")
        .token(&s.token)
        .send()
        .await;
    assert_eq!(r.data()["status"], "PENDING");
    assert_eq!(r.data()["timesheet"]["uuid"], uuid.as_str());

    // /my + filter
    let r = app.get("/timesheets/my").token(&s.token).send().await;
    r.assert_ok();
    assert_eq!(r.data().as_array().unwrap().len(), 1);
    assert_eq!(r.data()[0]["uuid"], uuid.as_str());
    assert!(r.data()[0]["approvedBy"].is_null());
    let r = app
        .get("/timesheets/my?status=APPROVED&year=2026")
        .token(&s.token)
        .send()
        .await;
    r.assert_ok();
    assert_eq!(r.data().as_array().unwrap().len(), 0);
    let r = app
        .get("/timesheets/my?status=PENDING&year=2020")
        .token(&s.token)
        .send()
        .await;
    assert_eq!(r.data().as_array().unwrap().len(), 0);
    app.get("/timesheets/my?status=BOGUS")
        .token(&s.token)
        .send()
        .await
        .assert_status(400);
    app.get("/timesheets/my").send().await.assert_status(401);

    // withdraw: orang lain tidak menemukan
    let s2 = user(&app, fx.id, "STAFF", None).await;
    let r = app
        .post(&format!("/timesheets/my/withdraw/{uuid}"))
        .token(&s2.token)
        .send()
        .await;
    assert_eq!(r.json["message"], "Timesheet not found");
    // withdraw ditolak bila sudah diapprove
    app.exec(&format!(
        "UPDATE timesheet SET status = 'APPROVED' WHERE id = {id}"
    ))
    .await;
    let r = app
        .post(&format!("/timesheets/my/withdraw/{uuid}"))
        .token(&s.token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(
        r.json["message"],
        "Only PENDING timesheets can be withdrawn"
    );
    app.exec(&format!(
        "UPDATE timesheet SET status = 'PENDING' WHERE id = {id}"
    ))
    .await;
    let r = app
        .post(&format!("/timesheets/my/withdraw/{uuid}"))
        .token(&s.token)
        .send()
        .await;
    r.assert_ok();
    assert_eq!(
        app.scalar::<i64>(&format!("SELECT count(*) FROM timesheet WHERE id = {id}"))
            .await,
        0
    );
    assert_eq!(
        app.scalar::<i64>(&format!(
            r#"SELECT count(*) FROM "timesheetApprovalToken" WHERE "timesheetId" = {id}"#
        ))
        .await,
        0
    );
}

#[tokio::test]
async fn approve_reject_reopen_and_roles() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = prep(&app).await;
    let s = user(&app, fx.id, "STAFF", None).await;
    pay_rate(&app, s.id, fx.id, "MONTHLY", true, 5.0, "HOURLY").await;
    let p = project(&app, fx.id, &uniq("Proj"), true).await;
    seed(&app, s.id, p, "'2026-05-04'", 8, 9, true).await;

    let r = app
        .post("/timesheets/admin/create")
        .token(&fx.admin.token)
        .json(
            &json!({"userUuid": s.uuid, "periodStart": "2026-05-01", "periodEnd": "2026-05-31",
                      "payPeriodType": "MONTHLY"}),
        )
        .send()
        .await;
    r.assert_status(201).assert_ok();
    let uuid = r.data()["uuid"].as_str().unwrap().to_owned();

    // staff tidak boleh
    for (m, path) in [("approve", "approve"), ("reject", "reject")] {
        let _ = m;
        let r = app
            .patch(&format!("/timesheets/{uuid}/{path}"))
            .token(&s.token)
            .json(&json!({"rejectedReason": "x"}))
            .send()
            .await;
        r.assert_status(401);
    }
    // validasi reject
    app.patch(&format!("/timesheets/{uuid}/reject"))
        .token(&fx.manager.token)
        .json(&json!({}))
        .send()
        .await
        .assert_status(400);
    app.patch("/timesheets/not-a-uuid/approve")
        .token(&fx.manager.token)
        .send()
        .await
        .assert_status(400);

    let r = app
        .patch(&format!("/timesheets/{uuid}/approve"))
        .token(&fx.manager.token)
        .send()
        .await;
    r.assert_ok();
    assert_eq!(r.data()["status"], "APPROVED");
    assert_eq!(r.data()["approvedBy"]["uuid"], fx.manager.uuid.as_str());
    // dua kali
    let r = app
        .patch(&format!("/timesheets/{uuid}/approve"))
        .token(&fx.admin.token)
        .send()
        .await;
    assert_eq!(r.json["message"], "Only PENDING timesheets can be approved");
    let r = app
        .patch(&format!("/timesheets/{uuid}/reject"))
        .token(&fx.admin.token)
        .json(&json!({"rejectedReason": "x"}))
        .send()
        .await;
    assert_eq!(r.json["message"], "Only PENDING timesheets can be rejected");

    // reopen: reason + tanpa reason
    let r = app
        .patch(&format!("/timesheets/{uuid}/reopen"))
        .token(&fx.admin.token)
        .json(&json!({"reason": "salah jam"}))
        .send()
        .await;
    r.assert_ok();
    assert_eq!(r.data()["notes"], "[REOPENED] salah jam");
    let r = app
        .patch(&format!("/timesheets/{uuid}/reopen"))
        .token(&fx.admin.token)
        .json(&json!({}))
        .send()
        .await;
    assert_eq!(
        r.json["message"],
        "Only APPROVED or REJECTED timesheets can be reopened"
    );

    let r = app
        .patch(&format!("/timesheets/{uuid}/reject"))
        .token(&fx.admin.token)
        .json(&json!({"rejectedReason": "kurang bukti"}))
        .send()
        .await;
    r.assert_ok();
    assert_eq!(r.data()["status"], "REJECTED");
    assert_eq!(r.data()["rejectedReason"], "kurang bukti");
    // edit/recalculate ditolak untuk REJECTED
    let r = app
        .patch(&format!("/timesheets/{uuid}/edit"))
        .token(&fx.admin.token)
        .json(&json!({"notes": "n"}))
        .send()
        .await;
    assert_eq!(r.json["message"], "Rejected timesheets cannot be edited");
    let r = app
        .patch(&format!("/timesheets/{uuid}/recalculate"))
        .token(&fx.admin.token)
        .send()
        .await;
    assert_eq!(
        r.json["message"],
        "Rejected timesheets cannot be recalculated"
    );

    let r = app
        .patch(&format!("/timesheets/{uuid}/reopen"))
        .token(&fx.manager.token)
        .json(&json!({}))
        .send()
        .await;
    r.assert_ok();
    assert_eq!(r.data()["notes"], "[REOPENED]");
    let row: (Option<i32>, Option<String>) = sqlx::query_as(
        r#"SELECT "approvedById", "rejectedReason" FROM timesheet WHERE uuid = $1::uuid"#,
    )
    .bind(&uuid)
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!(row, (None, None));

    // company lain tidak bisa reopen / edit / delete (uuid acak)
    let ghost = "00000000-0000-4000-8000-000000000000";
    for r in [
        app.patch(&format!("/timesheets/{ghost}/reopen"))
            .token(&fx.admin.token)
            .json(&json!({}))
            .send()
            .await,
        app.patch(&format!("/timesheets/{ghost}/edit"))
            .token(&fx.admin.token)
            .json(&json!({}))
            .send()
            .await,
        app.patch(&format!("/timesheets/{ghost}/recalculate"))
            .token(&fx.admin.token)
            .send()
            .await,
        app.patch(&format!("/timesheets/{ghost}/approve"))
            .token(&fx.admin.token)
            .send()
            .await,
        app.delete(&format!("/timesheets/{ghost}"))
            .token(&fx.admin.token)
            .send()
            .await,
        app.get(&format!("/timesheets/{ghost}"))
            .token(&fx.admin.token)
            .send()
            .await,
    ] {
        assert_eq!(r.json["message"], "Timesheet not found", "{}", r.uri);
    }
}

#[tokio::test]
async fn email_action_token_route() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = prep(&app).await;
    let mut uuids = vec![];
    for _ in 0..2 {
        let s = user(&app, fx.id, "STAFF", None).await;
        pay_rate(&app, s.id, fx.id, "MONTHLY", true, 5.0, "HOURLY").await;
        let r = app
            .post("/timesheets/submit")
            .token(&s.token)
            .json(&json!({}))
            .send()
            .await;
        r.assert_status(201).assert_ok();
        uuids.push(r.data()["uuid"].as_str().unwrap().to_owned());
    }
    let token_of = |uuid: String| {
        let app = &app;
        async move {
            let id = ts_id(app, &uuid).await;
            app.scalar::<String>(&format!(
                r#"SELECT uuid::text FROM "timesheetApprovalToken" WHERE "timesheetId" = {id} ORDER BY id LIMIT 1"#
            ))
            .await
        }
    };
    let status_of = |uuid: String| {
        let app = &app;
        async move {
            app.scalar::<String>(&format!(
                "SELECT status::text FROM timesheet WHERE uuid = '{uuid}'::uuid"
            ))
            .await
        }
    };
    let loc = |r: &mz_testkit::TestResponse| {
        r.headers
            .get("location")
            .map(|v| v.to_str().unwrap().to_owned())
            .unwrap_or_default()
    };

    // approve (publik, tanpa JWT)
    let t0 = token_of(uuids[0].clone()).await;
    let r = app
        .get(&format!(
            "/timesheets/email-action?token={t0}&action=approve"
        ))
        .send()
        .await;
    r.assert_status(302);
    assert!(
        loc(&r).ends_with(&format!("/timesheet/{}", uuids[0])),
        "{}",
        loc(&r)
    );
    assert_eq!(status_of(uuids[0].clone()).await, "APPROVED");
    assert!(
        app.scalar::<bool>(&format!(
            r#"SELECT "usedAt" IS NOT NULL FROM "timesheetApprovalToken" WHERE uuid = '{t0}'::uuid"#
        ))
        .await
    );
    // token terpakai: tak mengubah apa pun
    let r = app
        .get(&format!(
            "/timesheets/email-action?token={t0}&action=reject"
        ))
        .send()
        .await;
    r.assert_status(302);
    assert_eq!(status_of(uuids[0].clone()).await, "APPROVED");

    // reject
    let t1 = token_of(uuids[1].clone()).await;
    app.get(&format!(
        "/timesheets/email-action?token={t1}&action=reject"
    ))
    .send()
    .await
    .assert_status(302);
    assert_eq!(status_of(uuids[1].clone()).await, "REJECTED");
    let reason: String = app
        .scalar(&format!(
            "SELECT \"rejectedReason\" FROM timesheet WHERE uuid = '{}'::uuid",
            uuids[1]
        ))
        .await;
    assert_eq!(reason, "Rejected via email");

    // aksi tak valid / token tak dikenal -> /timesheet
    let r = app
        .get("/timesheets/email-action?token=x&action=nope")
        .send()
        .await;
    r.assert_status(302);
    assert!(loc(&r).ends_with("/timesheet"));
    let r = app
        .get("/timesheets/email-action?token=00000000-0000-4000-8000-000000000000&action=approve")
        .send()
        .await;
    r.assert_status(302);
    assert!(loc(&r).ends_with("/timesheet"));
    // token kedaluwarsa
    let s = user(&app, fx.id, "STAFF", None).await;
    pay_rate(&app, s.id, fx.id, "MONTHLY", true, 5.0, "HOURLY").await;
    let r = app
        .post("/timesheets/submit")
        .token(&s.token)
        .json(&json!({}))
        .send()
        .await;
    let u2 = r.data()["uuid"].as_str().unwrap().to_owned();
    let t2 = token_of(u2.clone()).await;
    app.exec(&format!(r#"UPDATE "timesheetApprovalToken" SET "expiredAt" = now() - interval '1 day' WHERE uuid = '{t2}'::uuid"#)).await;
    let r = app
        .get(&format!(
            "/timesheets/email-action?token={t2}&action=approve"
        ))
        .send()
        .await;
    r.assert_status(302);
    assert_eq!(status_of(u2).await, "PENDING");
    // token bukan uuid -> 500 (deviasi #6, perilaku Prisma)
    app.get("/timesheets/email-action?token=bukan-uuid&action=approve")
        .send()
        .await
        .assert_status(500);
}

// ───────────────────────── admin: create / list / detail / edit / export ─────────────────────────

#[tokio::test]
async fn admin_create_list_detail_edit_recalc_export_delete() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = prep(&app).await;
    let s = user(&app, fx.id, "STAFF", None).await;
    let pname = uniq("Alpha");
    let pa = project(&app, fx.id, &pname, true).await;
    let pb = project(&app, fx.id, &uniq("Beta"), false).await;
    seed(&app, s.id, pa, "'2026-03-02'", 9, 11, true).await;
    seed(&app, s.id, pb, "'2026-03-02'", 13, 14, false).await;
    seed(&app, s.id, pa, "'2026-03-03'", 9, 10, true).await;

    let body = |extra: Value| {
        let mut b = json!({"userUuid": s.uuid, "periodStart": "2026-03-01", "periodEnd": "2026-03-31",
                           "payPeriodType": "MONTHLY"});
        for (k, v) in extra.as_object().unwrap() {
            b[k] = v.clone();
        }
        b
    };
    // gagal: tanpa pay rate & tanpa override
    let r = app
        .post("/timesheets/admin/create")
        .token(&fx.manager.token)
        .json(&body(json!({})))
        .send()
        .await;
    assert_eq!(
        r.json["message"],
        "No active pay rate configured for this employee and no override provided"
    );
    let r = app
        .post("/timesheets/admin/create")
        .token(&fx.manager.token)
        .json(&body(json!({"periodStart": "2026-04-01"})))
        .send()
        .await;
    assert_eq!(
        r.json["message"],
        "Period start must be before or equal to period end"
    );
    let r = app
        .post("/timesheets/admin/create")
        .token(&fx.manager.token)
        .json(&body(
            json!({"userUuid": "00000000-0000-4000-8000-000000000000"}),
        ))
        .send()
        .await;
    assert_eq!(r.json["message"], "Employee not found");
    app.post("/timesheets/admin/create")
        .token(&fx.staff.token)
        .json(&body(json!({})))
        .send()
        .await
        .assert_status(401);
    app.post("/timesheets/admin/create")
        .token(&fx.manager.token)
        .json(&json!({}))
        .send()
        .await
        .assert_status(400);

    // sukses dengan override FIXED
    let r = app
        .post("/timesheets/admin/create")
        .token(&fx.manager.token)
        .json(&body(
            json!({"payRate": 30, "rateType": "HOURLY", "notes": "n1"}),
        ))
        .send()
        .await;
    r.assert_status(201).assert_ok();
    let d = r.data();
    assert_eq!(d["totalSeconds"], 14400);
    assert_eq!(d["totalHours"], 4);
    assert_eq!(d["totalAmount"], 120);
    assert_eq!(d["notes"], "n1");
    let uuid = d["uuid"].as_str().unwrap().to_owned();
    // duplikat
    let r = app
        .post("/timesheets/admin/create")
        .token(&fx.manager.token)
        .json(&body(json!({"payRate": 1})))
        .send()
        .await;
    assert_eq!(
        r.json["message"],
        "Employee already has a pending or approved timesheet for this period"
    );

    // list admin + filter
    let r = app.get("/timesheets").token(&fx.admin.token).send().await;
    r.assert_ok();
    assert!(r.json["total"].as_u64().unwrap() >= 1);
    let r = app
        .get(&format!(
            "/timesheets?status=PENDING&userUuid={}&periodStart=2026-03-01&periodEnd=2026-03-31",
            s.uuid
        ))
        .token(&fx.super_admin.token)
        .send()
        .await;
    r.assert_ok();
    assert_eq!(r.json["total"], 1);
    assert_eq!(r.data()[0]["uuid"], uuid.as_str());
    assert_eq!(r.data()[0]["user"]["uuid"], s.uuid.as_str());
    let r = app
        .get(&format!("/timesheets?userUuid={}&status=APPROVED", s.uuid))
        .token(&fx.admin.token)
        .send()
        .await;
    assert_eq!(r.json["total"], 0);
    app.get("/timesheets")
        .token(&fx.staff.token)
        .send()
        .await
        .assert_status(401);
    app.get("/timesheets?status=X")
        .token(&fx.admin.token)
        .send()
        .await
        .assert_status(400);

    // detail + breakdown
    let r = app
        .get(&format!("/timesheets/{uuid}"))
        .token(&fx.manager.token)
        .send()
        .await;
    r.assert_ok();
    let d = r.data();
    assert_eq!(d["billableSeconds"], 10800);
    assert_eq!(d["nonBillableSeconds"], 3600);
    assert_eq!(d["billablePercentage"], 75);
    assert_eq!(d["dailyBreakdown"].as_array().unwrap().len(), 2);
    assert_eq!(d["user"]["uuid"], s.uuid.as_str());
    app.get(&format!("/timesheets/{uuid}"))
        .token(&fx.staff.token)
        .send()
        .await
        .assert_status(401);

    // ekspor
    let r = app
        .get(&format!("/timesheets/{uuid}/export/pdf"))
        .token(&fx.manager.token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.headers.get("content-type").unwrap(), "application/pdf");
    assert!(r.bytes.starts_with(b"%PDF"), "bukan PDF");
    assert!(r.bytes.len() > 500);
    assert!(
        r.headers
            .get("content-disposition")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("Timesheet-")
    );
    let r = app
        .get(&format!("/timesheets/{uuid}/export/csv"))
        .token(&fx.manager.token)
        .send()
        .await;
    r.assert_status(200);
    assert!(
        r.headers
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("text/csv")
    );
    let csv = r.text();
    assert!(csv.contains(&pname), "csv memuat nama project: {csv}");
    assert!(csv.contains("2026-03-02"));
    let r = app
        .get("/timesheets/00000000-0000-4000-8000-000000000000/export/pdf")
        .token(&fx.manager.token)
        .send()
        .await;
    r.assert_status(404);
    app.get(&format!("/timesheets/{uuid}/export/csv"))
        .token(&fx.staff.token)
        .send()
        .await
        .assert_status(401);

    // edit
    let r = app
        .patch(&format!("/timesheets/{uuid}/edit"))
        .token(&fx.manager.token)
        .json(&json!({"notes": "diubah", "payRate": 50, "rateType": "FIXED"}))
        .send()
        .await;
    r.assert_ok();
    assert_eq!(r.data()["totalAmount"], 50);
    assert_eq!(r.data()["rateType"], "FIXED");
    let r = app
        .patch(&format!("/timesheets/{uuid}/edit"))
        .token(&fx.manager.token)
        .json(&json!({"payRate": 10, "rateType": "HOURLY", "notes": null}))
        .send()
        .await;
    r.assert_ok();
    assert_eq!(r.data()["totalAmount"], 40);
    assert!(r.data()["notes"].is_null());
    app.patch(&format!("/timesheets/{uuid}/edit"))
        .token(&fx.manager.token)
        .json(&json!({"payRate": -1}))
        .send()
        .await
        .assert_status(400);
    app.patch(&format!("/timesheets/{uuid}/edit"))
        .token(&fx.staff.token)
        .json(&json!({"notes": "x"}))
        .send()
        .await
        .assert_status(401);

    // recalculate setelah tambah timeline
    seed(&app, s.id, pa, "'2026-03-04'", 9, 11, true).await;
    let r = app
        .patch(&format!("/timesheets/{uuid}/recalculate"))
        .token(&fx.admin.token)
        .send()
        .await;
    r.assert_ok();
    assert_eq!(r.data()["totalSeconds"], 21600);
    assert_eq!(r.data()["totalAmount"], 60);

    // delete
    let r = app
        .delete(&format!("/timesheets/{uuid}"))
        .token(&fx.admin.token)
        .send()
        .await;
    r.assert_ok();
    assert_eq!(
        app.scalar::<i64>(&format!(
            "SELECT count(*) FROM timesheet WHERE uuid = '{uuid}'::uuid"
        ))
        .await,
        0
    );
    app.delete(&format!("/timesheets/{uuid}"))
        .token(&fx.staff.token)
        .send()
        .await
        .assert_status(401);
}

#[tokio::test]
async fn delete_blocked_when_invoiced() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = prep(&app).await;
    let s = user(&app, fx.id, "STAFF", None).await;
    pay_rate(&app, s.id, fx.id, "MONTHLY", true, 5.0, "HOURLY").await;
    let uuid = approved_ts(&app, &fx, &s, "2026-04-01", "2026-04-30").await;
    app.post(&format!("/timesheet-invoices/{uuid}/generate"))
        .token(&fx.admin.token)
        .send()
        .await
        .assert_status(201);
    let r = app
        .delete(&format!("/timesheets/{uuid}"))
        .token(&fx.admin.token)
        .send()
        .await;
    assert_eq!(
        r.json["message"],
        "Cannot delete timesheet that has been invoiced. Remove the invoice first."
    );
    // withdraw hanya PENDING -> reopen lalu withdraw terblokir karena invoice
    app.patch(&format!("/timesheets/{uuid}/reopen"))
        .token(&fx.admin.token)
        .json(&json!({}))
        .send()
        .await
        .assert_ok();
    let r = app
        .post(&format!("/timesheets/my/withdraw/{uuid}"))
        .token(&s.token)
        .send()
        .await;
    assert!(
        r.json["message"]
            .as_str()
            .unwrap()
            .starts_with("Cannot withdraw timesheet that has been invoiced")
    );
}

// ───────────────────────── invoices ─────────────────────────

#[tokio::test]
async fn invoices_generate_list_get_pdf_status() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = prep(&app).await;
    let s = user(&app, fx.id, "STAFF", Some("")).await; // username kosong: lihat catatan atas
    let s2 = user(&app, fx.id, "STAFF", None).await;
    let s3 = user(&app, fx.id, "STAFF", None).await;
    for u in [&s, &s2, &s3] {
        pay_rate(&app, u.id, fx.id, "MONTHLY", true, 12.5, "HOURLY").await;
        let p = project(&app, fx.id, &uniq("Inv"), true).await;
        seed(&app, u.id, p, "'2026-02-03'", 9, 12, true).await;
    }
    let t1 = approved_ts(&app, &fx, &s, "2026-02-01", "2026-02-28").await;
    let t2 = approved_ts(&app, &fx, &s2, "2026-02-01", "2026-02-28").await;
    let t3 = approved_ts(&app, &fx, &s3, "2026-02-01", "2026-02-28").await;

    // belum approved -> 400
    let pending = app
        .post("/timesheets/admin/create")
        .token(&fx.admin.token)
        .json(
            &json!({"userUuid": s.uuid, "periodStart": "2026-03-01", "periodEnd": "2026-03-31",
                      "payPeriodType": "MONTHLY", "payRate": 1}),
        )
        .send()
        .await;
    let pu = pending.data()["uuid"].as_str().unwrap().to_owned();
    app.post(&format!("/timesheet-invoices/{pu}/generate"))
        .token(&fx.admin.token)
        .send()
        .await
        .assert_status(400);
    app.post("/timesheet-invoices/00000000-0000-4000-8000-000000000000/generate")
        .token(&fx.admin.token)
        .send()
        .await
        .assert_status(404);
    app.post(&format!("/timesheet-invoices/{t1}/generate"))
        .token(&fx.staff.token)
        .send()
        .await
        .assert_status(401);

    // dua generate bersamaan (advisory lock) -> nomor berbeda
    let (a, b) = tokio::join!(
        app.post(&format!("/timesheet-invoices/{t2}/generate"))
            .token(&fx.admin.token)
            .send(),
        app.post(&format!("/timesheet-invoices/{t3}/generate"))
            .token(&fx.manager.token)
            .send(),
    );
    a.assert_status(201);
    b.assert_status(201);
    let (na, nb) = (
        a.json["invoiceNumber"].as_str().unwrap(),
        b.json["invoiceNumber"].as_str().unwrap(),
    );
    assert_ne!(na, nb);
    let prefix: String = app
        .scalar("SELECT 'INV-TS-' || to_char(now() at time zone 'utc', 'YYYYMM') || '-'")
        .await;
    assert!(
        na.starts_with(&prefix) && nb.starts_with(&prefix),
        "{na} {nb}"
    );
    assert_eq!(a.json["status"], "ISSUED");

    let r = app
        .post(&format!("/timesheet-invoices/{t1}/generate"))
        .token(&fx.admin.token)
        .send()
        .await;
    r.assert_status(201);
    let inv1 = r.json["uuid"].as_str().unwrap().to_owned();
    let n1 = r.json["invoiceNumber"].as_str().unwrap().to_owned();
    assert!(n1.starts_with(&prefix));
    let seqs: Vec<i64> = [na, nb, n1.as_str()]
        .iter()
        .map(|n| n[prefix.len()..].parse().unwrap())
        .collect();
    let mut sorted = seqs.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), 3, "nomor unik: {seqs:?}");
    // idempoten
    let r = app
        .post(&format!("/timesheet-invoices/{t1}/generate"))
        .token(&fx.admin.token)
        .send()
        .await;
    assert_eq!(r.json["uuid"], inv1.as_str());
    assert_eq!(r.json["message"], "Invoice sudah pernah dibuat sebelumnya");

    // list
    let r = app
        .get("/timesheet-invoices")
        .token(&fx.admin.token)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert!(r.json["total"].as_i64().unwrap() >= 3);
    assert_eq!(r.json["page"], 1);
    let r = app
        .get(&format!(
            "/timesheet-invoices?status=ISSUED&userUuid={}&page=1&limit=5",
            s.uuid
        ))
        .token(&fx.manager.token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json["total"], 1);
    assert_eq!(r.json["data"][0]["uuid"], inv1.as_str());
    assert_eq!(r.json["data"][0]["totalAmount"], 37.5);
    let r = app
        .get("/timesheet-invoices?page=2&limit=1")
        .token(&fx.admin.token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json["data"].as_array().unwrap().len(), 1);
    app.get("/timesheet-invoices?userUuid=00000000-0000-4000-8000-000000000000")
        .token(&fx.admin.token)
        .send()
        .await
        .assert_status(404);
    app.get("/timesheet-invoices?page=0")
        .token(&fx.admin.token)
        .send()
        .await
        .assert_status(400);
    app.get("/timesheet-invoices")
        .token(&fx.staff.token)
        .send()
        .await
        .assert_status(401);

    // detail
    let r = app
        .get(&format!("/timesheet-invoices/{inv1}"))
        .token(&fx.admin.token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json["invoiceNumber"], n1.as_str());
    assert_eq!(r.json["totalSeconds"], 10800);
    assert_eq!(r.json["totalHours"], 3);
    assert_eq!(r.json["payRate"], 12.5);
    assert_eq!(r.json["periodStart"], "2026-02-01T00:00:00.000Z");
    assert!(r.json["paidAt"].is_null());
    app.get("/timesheet-invoices/00000000-0000-4000-8000-000000000000")
        .token(&fx.admin.token)
        .send()
        .await
        .assert_status(404);

    // pdf
    let r = app
        .get(&format!("/timesheet-invoices/{inv1}/download/pdf"))
        .token(&fx.admin.token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.headers.get("content-type").unwrap(), "application/pdf");
    assert!(r.bytes.starts_with(b"%PDF") && r.bytes.len() > 500);
    app.get(&format!("/timesheet-invoices/{inv1}/download/pdf"))
        .token(&fx.staff.token)
        .send()
        .await
        .assert_status(401);

    // status
    app.patch(&format!("/timesheet-invoices/{inv1}/status"))
        .token(&fx.admin.token)
        .json(&json!({"status": "DRAFT"}))
        .send()
        .await
        .assert_status(400);
    let r = app
        .patch(&format!("/timesheet-invoices/{inv1}/status"))
        .token(&fx.admin.token)
        .json(&json!({"status": "PAID"}))
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json["status"], "PAID");
    assert!(r.json["paidAt"].is_string());
    app.patch(&format!("/timesheet-invoices/{inv1}/status"))
        .token(&fx.admin.token)
        .json(&json!({"status": "CANCELLED"}))
        .send()
        .await
        .assert_status(400);
    // CANCELLED pada invoice lain (tanpa email)
    let inv2: String = app
        .scalar(&format!(r#"SELECT i.uuid::text FROM "timesheetInvoice" i JOIN timesheet t ON t.id = i."timesheetId" WHERE t.uuid = '{t2}'::uuid"#))
        .await;
    let r = app
        .patch(&format!("/timesheet-invoices/{inv2}/status"))
        .token(&fx.manager.token)
        .json(&json!({"status": "CANCELLED"}))
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json["status"], "CANCELLED");
    assert!(r.json["paidAt"].is_null());
    app.patch(&format!("/timesheet-invoices/{inv2}/status"))
        .token(&fx.manager.token)
        .json(&json!({"status": "PAID"}))
        .send()
        .await
        .assert_status(400);
    app.patch(&format!("/timesheet-invoices/{inv2}/status"))
        .token(&fx.staff.token)
        .json(&json!({"status": "PAID"}))
        .send()
        .await
        .assert_status(401);
    let r = app
        .get("/timesheet-invoices?status=PAID")
        .token(&fx.admin.token)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
}

// ───────────────────────── manager summary ─────────────────────────

#[tokio::test]
async fn manager_summary_config_crud() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = prep(&app).await;
    let uri = "/manager-summary/config";
    // bersihkan config bawaan template (satu config per company; hanya test ini yang memakainya)
    app.exec(&format!(
        r#"DELETE FROM "managerSummaryConfig" WHERE "companyId" = {}"#,
        fx.id
    ))
    .await;

    let r = app.get(uri).token(&fx.manager.token).send().await;
    r.assert_ok();
    assert!(r.json["data"].is_null());
    app.put(uri)
        .token(&fx.manager.token)
        .json(&json!({"isDailyEnabled": false}))
        .send()
        .await
        .assert_status(404);
    app.delete(uri)
        .token(&fx.admin.token)
        .send()
        .await
        .assert_status(404);
    for r in [
        app.get(uri).token(&fx.staff.token).send().await,
        app.post(uri)
            .token(&fx.staff.token)
            .json(&json!({"emails": ["a@b.co"]}))
            .send()
            .await,
        app.put(uri)
            .token(&fx.staff.token)
            .json(&json!({}))
            .send()
            .await,
        app.delete(uri).token(&fx.staff.token).send().await,
        app.delete(uri).token(&fx.manager.token).send().await, // hanya SUPER_ADMIN/ADMIN
    ] {
        r.assert_status(401);
    }
    app.post(uri)
        .token(&fx.manager.token)
        .json(&json!({}))
        .send()
        .await
        .assert_status(400);
    app.post(uri)
        .token(&fx.manager.token)
        .json(&json!({"emails": ["a@b.co"], "sendTime": "08:15"}))
        .send()
        .await
        .assert_status(400);

    let r = app.post(uri).token(&fx.manager.token)
        .json(&json!({"emails": ["a@b.co"], "userIds": [1, 2], "disabledFlags": ["x"], "includeWeekends": true}))
        .send().await;
    r.assert_status(201).assert_ok();
    let d = r.data();
    // Urutan kunci = urutan field model Prisma (bukan urutan jsonb: panjang lalu abjad).
    assert_eq!(
        key_list(d),
        [
            "id",
            "uuid",
            "companyId",
            "isDailyEnabled",
            "isWeeklyEnabled",
            "sendTime",
            "includeWeekends",
            "emails",
            "userIds",
            "lowHoursThreshold",
            "disabledFlags",
            "lastDailySentAt",
            "lastWeeklySentAt",
            "createdById",
            "createdAt",
            "updatedAt",
        ]
    );
    assert_eq!(d["emails"], json!(["a@b.co"]));
    assert_eq!(d["sendTime"], "08:00");
    assert_eq!(d["lowHoursThreshold"], 14400);
    assert_eq!(d["includeWeekends"], true);
    assert_eq!(d["userIds"], json!([1, 2]));
    assert_eq!(d["createdById"], fx.manager.id);
    assert!(d["createdAt"].as_str().unwrap().ends_with('Z'));
    assert!(d["lastDailySentAt"].is_null());

    let r = app.get(uri).token(&fx.admin.token).send().await;
    r.assert_ok();
    assert_eq!(r.data()["emails"], json!(["a@b.co"]));

    // POST saat sudah ada = update
    let r = app
        .post(uri)
        .token(&fx.admin.token)
        .json(&json!({"emails": ["c@d.co"], "sendTime": "09:30"}))
        .send()
        .await;
    r.assert_status(201).assert_ok();
    assert_eq!(r.data()["sendTime"], "09:30");
    assert_eq!(r.json["message"], "Config updated");

    let r = app
        .put(uri)
        .token(&fx.super_admin.token)
        .json(
            &json!({"isDailyEnabled": false, "isWeeklyEnabled": false, "lowHoursThreshold": 7200,
                      "userIds": null, "disabledFlags": ["a", "b"]}),
        )
        .send()
        .await;
    r.assert_ok();
    let d = r.data();
    assert_eq!(d["isDailyEnabled"], false);
    assert_eq!(d["lowHoursThreshold"], 7200);
    assert!(d["userIds"].is_null());
    assert_eq!(d["disabledFlags"], json!(["a", "b"]));
    app.put(uri)
        .token(&fx.admin.token)
        .json(&json!({"emails": []}))
        .send()
        .await
        .assert_status(400);

    app.delete(uri)
        .token(&fx.admin.token)
        .send()
        .await
        .assert_ok();
    assert_eq!(
        app.scalar::<i64>(&format!(
            r#"SELECT count(*) FROM "managerSummaryConfig" WHERE "companyId" = {}"#,
            fx.id
        ))
        .await,
        0
    );
}

// ───────────────────────── scheduled reports ─────────────────────────

#[tokio::test]
async fn scheduled_reports_crud() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = prep(&app).await;
    let name = uniq("Laporan");
    let uri = "/scheduled-reports";
    let body = json!({"name": name, "sendTime": "07:30", "frequency": "WEEKLY", "days": [1, 3],
                      "emails": ["x@y.com"], "subject": "Subjek", "message": "Pesan",
                      "userIds": [1], "projectIds": [2], "sections": ["summary"]});

    for r in [
        app.get(uri).token(&fx.staff.token).send().await,
        app.post(uri)
            .token(&fx.staff.token)
            .json(&body)
            .send()
            .await,
    ] {
        r.assert_status(401);
    }
    app.post(uri)
        .token(&fx.manager.token)
        .json(&json!({}))
        .send()
        .await
        .assert_status(400);
    app.post(uri)
        .token(&fx.manager.token)
        .json(
            &json!({"name": "n", "sendTime": "07:15", "frequency": "WEEKLY",
        "emails": ["x@y.com"], "subject": "s", "message": "m"}),
        )
        .send()
        .await
        .assert_status(400);

    let r = app
        .post(uri)
        .token(&fx.manager.token)
        .json(&body)
        .send()
        .await;
    r.assert_status(200).assert_ok();
    let d = r.data();
    assert_eq!(
        key_list(d),
        [
            "id",
            "uuid",
            "name",
            "sendTime",
            "frequency",
            "days",
            "emails",
            "subject",
            "message",
            "userIds",
            "projectIds",
            "sections",
            "isActive",
            "lastSentAt",
            "companyId",
            "createdById",
            "createdAt",
            "updatedAt",
        ]
    );
    assert_eq!(d["name"], name.as_str());
    assert_eq!(d["frequency"], "WEEKLY");
    assert_eq!(d["days"], json!([1, 3]));
    assert_eq!(d["isActive"], true);
    assert_eq!(d["companyId"], fx.id);
    assert!(d["createdAt"].as_str().unwrap().ends_with('Z'));
    let uuid = d["uuid"].as_str().unwrap().to_owned();

    // minimal: kolom Json nullable NULL
    let r = app
        .post(uri)
        .token(&fx.admin.token)
        .json(
            &json!({"name": uniq("Min"), "sendTime": "00:00", "frequency": "DAILY",
                      "emails": ["z@y.com"], "subject": "s", "message": "m"}),
        )
        .send()
        .await;
    r.assert_ok();
    assert!(r.data()["days"].is_null());

    let r = app.get(uri).token(&fx.admin.token).send().await;
    r.assert_ok();
    let list = r.data().as_array().unwrap();
    assert!(list.len() >= 2);
    let mine = list.iter().find(|x| x["uuid"] == uuid.as_str()).unwrap();
    // `include: { createdBy }`: kolom skalar menurut field model, `createdBy` terakhir.
    assert_eq!(key_list(mine).last(), Some(&"createdBy"));
    assert_eq!(key_list(mine).get(..3), Some(&["id", "uuid", "name"][..]));
    assert_eq!(key_list(&mine["createdBy"]), ["id", "name", "username"]);
    assert_eq!(mine["createdBy"]["id"], fx.manager.id);
    assert_eq!(mine["createdBy"]["username"], fx.manager.username.as_str());

    let r = app
        .get(&format!("{uri}/{uuid}"))
        .token(&fx.super_admin.token)
        .send()
        .await;
    r.assert_ok();
    assert!(r.data()["createdBy"]["name"].is_string());
    app.get(&format!("{uri}/00000000-0000-4000-8000-000000000000"))
        .token(&fx.admin.token)
        .send()
        .await
        .assert_status(404);
    app.get(&format!("{uri}/zzz"))
        .token(&fx.admin.token)
        .send()
        .await
        .assert_status(400);

    let r = app
        .patch(&format!("{uri}/{uuid}"))
        .token(&fx.admin.token)
        .json(
            &json!({"name": "Baru", "frequency": "MONTHLY", "isActive": false, "days": null,
                      "emails": ["q@w.com"], "sections": null, "projectIds": [9]}),
        )
        .send()
        .await;
    r.assert_ok();
    let d = r.data();
    assert_eq!(d["name"], "Baru");
    assert_eq!(d["frequency"], "MONTHLY");
    assert_eq!(d["isActive"], false);
    assert!(d["days"].is_null());
    assert!(d["sections"].is_null());
    assert_eq!(d["projectIds"], json!([9]));
    app.patch(&format!("{uri}/{uuid}"))
        .token(&fx.admin.token)
        .json(&json!({"isActive": "x"}))
        .send()
        .await
        .assert_status(400);
    app.patch(&format!("{uri}/{uuid}"))
        .token(&fx.staff.token)
        .json(&json!({}))
        .send()
        .await
        .assert_status(401);
    app.patch(&format!("{uri}/00000000-0000-4000-8000-000000000000"))
        .token(&fx.admin.token)
        .json(&json!({}))
        .send()
        .await
        .assert_status(404);

    app.delete(&format!("{uri}/{uuid}"))
        .token(&fx.staff.token)
        .send()
        .await
        .assert_status(401);
    app.delete(&format!("{uri}/{uuid}"))
        .token(&fx.manager.token)
        .send()
        .await
        .assert_ok();
    app.delete(&format!("{uri}/{uuid}"))
        .token(&fx.manager.token)
        .send()
        .await
        .assert_status(404);
    app.get(&format!("{uri}/{uuid}"))
        .token(&fx.manager.token)
        .send()
        .await
        .assert_status(404);
}
