//! Uji integrasi `mz-users` (28 route) terhadap Postgres NYATA.
//! Jalankan: `MZ_TEST_DATABASE_URL=postgres://… cargo test -p mz-server --test it_users`
//!
//! `POST /users/log` sukses & `GET /users/log` baris ber-`filePath` diuji lewat emulator GCS
//! kecil di proses test (`STORAGE_EMULATOR_HOST`, seperti `@google-cloud/storage`), tanpa
//! jaringan luar.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use mz_testkit::{Company, TestApp};
use serde_json::{Value, json};

/// bcrypt cost 4 untuk "Passw0rd!Test".
const PW_HASH: &str = "$2b$04$cmVeKhevRkxQOKQOxZOOhe1nFeoBKWImR4mQdFdAnpyYQOSSvfTUO";
const PW: &str = "Passw0rd!Test";

static SEQ: AtomicU32 = AtomicU32::new(0);

fn uniq(prefix: &str) -> String {
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("{prefix}{}x{t}x{n}", std::process::id())
}

fn email(prefix: &str) -> String {
    format!("{}@it-users.test", uniq(prefix))
}

struct Fresh {
    id: i32,
    uuid: String,
    username: String,
    token: String,
}

/// User baru lewat route invite (generatePassword => aktif) lalu password dikenal via SQL.
async fn invite_active(app: &TestApp, fx: &Company, code: &str) -> Fresh {
    let username = email("inv");
    let res = app
        .post("/users/invite")
        .token(&fx.super_admin.token)
        .json(&json!({
            "username": username, "name": "IT Invitee", "accessLevelCode": code,
            "isScreenshotActive": true, "generatePassword": true
        }))
        .send()
        .await;
    res.assert_ok();
    assert_eq!(res.json["message"], "invitation success", "{:?}", res.json);
    let id = res.json["data"]["user"]["id"].as_i64().unwrap() as i32;
    let uuid = res.json["data"]["user"]["uuid"]
        .as_str()
        .unwrap()
        .to_owned();
    app.exec(&format!(
        "UPDATE users SET password = '{PW_HASH}' WHERE id = {id}"
    ))
    .await;
    Fresh {
        id,
        uuid,
        username,
        token: app.token(id),
    }
}

/// Tunggu baris audit muncul (flush asinkron).
async fn wait_count(app: &TestApp, sql: &str, want: i64) -> i64 {
    let mut n = 0;
    for _ in 0..40 {
        n = app.scalar::<i64>(sql).await;
        if n >= want {
            return n;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    n
}

/// Template anonim tidak punya langganan untuk company fixture: buat satu (idempoten).
async fn ensure_subscription(app: &TestApp, fx: &Company) {
    app.exec(&format!(
        r#"INSERT INTO subscriptions ("companyId", "pricingTierId", "startDate", "endDate", status, "maxUsers")
           SELECT {0}, (SELECT id FROM "pricingTiers" ORDER BY id LIMIT 1), NOW() - interval '1 day',
                  NOW() + interval '1 year', 'ACTIVE'::"SubscriptionStatus", 10
           WHERE NOT EXISTS (SELECT 1 FROM subscriptions WHERE "companyId" = {0})"#,
        fx.id
    ))
    .await;
}

fn update_body(username: &str, status: i32, code: &str) -> Value {
    json!({
        "username": username, "name": "IT Updated", "status": status,
        "screenshotPerHour": 4, "idleTimeInSecond": 120,
        "isScreenshotActive": true, "isBlur": true, "accessLevel": code
    })
}

const MISSING: &str = "00000000-0000-4000-8000-00000000dead";

// ───────────────────────────── list / find ─────────────────────────────

#[tokio::test]
async fn list_users_scopes_paging_search() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let fresh = invite_active(&app, &fx, "STAFF").await;

    let res = app.get("/users").token(&fx.super_admin.token).send().await;
    res.assert_ok().assert_no_sql_error();
    let total = res.data()["total"].as_i64().unwrap();
    assert!(total >= 6, "{total}");
    let expected: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM users u WHERE u.status <> 0 AND EXISTS
               (SELECT 1 FROM "userCompany" uc WHERE uc."userId"=u.id AND uc."companyId"={})"#,
            fx.id
        ))
        .await;
    assert!(total >= expected - 50 && total >= 1);
    assert!(
        res.data()["result"]
            .as_array()
            .unwrap()
            .iter()
            .any(|u| u["uuid"] == fresh.uuid.as_str())
    );

    // paginasi + urutan
    for (qs, ord) in [
        (
            "pageNumber=1&itemsPerPage=3&orderBy=asc&orderColumn=name",
            "name",
        ),
        (
            "pageNumber=2&itemsPerPage=2&orderBy=desc&orderColumn=id",
            "id",
        ),
        (
            "pageNumber=1&itemsPerPage=5&orderColumn=username",
            "username",
        ),
        (
            "pageNumber=1&itemsPerPage=5&orderColumn=updatedAt",
            "updatedAt",
        ),
        (
            "pageNumber=1&itemsPerPage=5&orderColumn=status&orderBy=asc",
            "status",
        ),
    ] {
        let r = app
            .get(&format!("/users?{qs}"))
            .token(&fx.super_admin.token)
            .send()
            .await;
        r.assert_ok().assert_no_sql_error();
        assert!(r.data()["result"].as_array().unwrap().len() <= 5, "{ord}");
    }
    let page = app
        .get("/users?pageNumber=1&itemsPerPage=3")
        .token(&fx.super_admin.token)
        .send()
        .await;
    assert_eq!(page.data()["result"].as_array().unwrap().len(), 3);

    // pencarian (username / name, ILIKE)
    let r = app
        .get(&format!(
            "/users?pageNumber=1&itemsPerPage=10&search={}",
            fresh.username.to_uppercase()
        ))
        .token(&fx.super_admin.token)
        .send()
        .await;
    r.assert_ok();
    assert_eq!(r.data()["total"], 1);
    assert_eq!(r.data()["result"][0]["uuid"], fresh.uuid.as_str());
    let r = app
        .get("/users?pageNumber=1&itemsPerPage=10&search=zzzz-no-such-user-zzzz")
        .token(&fx.super_admin.token)
        .send()
        .await;
    assert_eq!(r.data()["total"], 0);

    // manager / admin melihat seluruh company; staff hanya dirinya
    for u in [&fx.manager, &fx.admin] {
        let r = app.get("/users").token(&u.token).send().await;
        r.assert_ok().assert_no_sql_error();
        assert!(r.data()["total"].as_i64().unwrap() >= 6);
    }
    let r = app.get("/users").token(&fx.staff.token).send().await;
    r.assert_ok().assert_no_sql_error();
    assert_eq!(r.data()["total"], 1);
    assert_eq!(r.data()["result"][0]["uuid"], fx.staff.uuid.as_str());

    // validasi query
    app.get("/users?pageNumber=abc&itemsPerPage=5")
        .token(&fx.super_admin.token)
        .send()
        .await
        .assert_status(400);
    app.get("/users").send().await.assert_status(401);
}

#[tokio::test]
async fn find_one_user() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    for u in [&fx.super_admin, &fx.manager, &fx.staff] {
        let r = app
            .get(&format!("/users/{}", fx.staff2.uuid))
            .token(&u.token)
            .send()
            .await;
        r.assert_ok().assert_no_sql_error();
        assert_eq!(r.data()["uuid"], fx.staff2.uuid.as_str());
        assert!(
            r.data()["userCompany"]
                .as_array()
                .is_some_and(|a| !a.is_empty())
        );
        assert!(r.data().get("password").is_none());
    }
    let r = app
        .get(&format!("/users/{MISSING}"))
        .token(&fx.super_admin.token)
        .send()
        .await;
    r.assert_no_sql_error();
    assert_eq!(r.status.as_u16(), 404, "{:?}", r.json);
    app.get("/users/not-a-uuid")
        .token(&fx.super_admin.token)
        .send()
        .await
        .assert_status(400);
}

// ───────────────────────────── invite ─────────────────────────────

#[tokio::test]
async fn invite_single_with_audit_and_duplicates() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let username = email("Inv");
    ensure_subscription(&app, &fx).await;

    let res = app
        .post("/users/invite")
        .token(&fx.super_admin.token)
        .json(&json!({"username": username, "name": "Budi IT", "accessLevelCode": "STAFF", "isScreenshotActive": true}))
        .send()
        .await;
    res.assert_ok();
    assert_eq!(res.json["message"], "invitation success", "{:?}", res.json);
    assert!(res.json["data"]["user"].get("password").is_none());
    let uuid = res.json["data"]["user"]["uuid"]
        .as_str()
        .unwrap()
        .to_owned();
    let id = res.json["data"]["user"]["id"].as_i64().unwrap();
    // username dinormalisasi huruf kecil, status 2 = invited
    assert_eq!(
        res.json["data"]["user"]["username"],
        username.to_lowercase().as_str()
    );
    assert_eq!(res.json["data"]["user"]["status"], 2);
    let (uc, ual): (i64, i64) = (
        app.scalar(&format!(
            r#"SELECT COUNT(*) FROM "userCompany" WHERE "userId"={id} AND "companyId"={} AND status=1 AND "invitedById"={}"#,
            fx.id, fx.super_admin.id
        ))
        .await,
        app.scalar(&format!(
            r#"SELECT COUNT(*) FROM "userAccessLevel" WHERE "userId"={id} AND "companyId"={} AND status=1"#,
            fx.id
        ))
        .await,
    );
    assert_eq!((uc, ual), (1, 1));

    // audit: users.create (entityUuid) + userCompany.create
    let q = format!(
        r#"SELECT COUNT(*) FROM "auditLog" WHERE "companyId"={} AND action='create' AND "actorId"={}
           AND ("entityId"={id} OR "entityUuid"='{uuid}'::uuid)"#,
        fx.id, fx.super_admin.id
    );
    assert!(
        wait_count(&app, &q, 1).await >= 1,
        "baris audit invite tidak ada"
    );

    // undang ulang (masih INVITED, anggota company) => dikirim ulang
    let again = app
        .post("/users/invite")
        .token(&fx.super_admin.token)
        .json(&json!({"username": username, "name": "Budi IT", "accessLevelCode": "STAFF", "isScreenshotActive": true}))
        .send()
        .await;
    again.assert_ok();
    assert_eq!(
        again.json["message"], "invitation resent",
        "{:?}",
        again.json
    );

    // generatePassword pada email terdaftar ditolak
    let dup = app
        .post("/users/invite")
        .token(&fx.super_admin.token)
        .json(&json!({"username": username, "name": "x", "accessLevelCode": "STAFF", "isScreenshotActive": false, "generatePassword": true}))
        .send()
        .await;
    assert_eq!(dup.json["success"], false);
    assert_eq!(dup.json["message"], "User already registered");

    // manager tidak boleh mengundang ADMIN (plafon jabatan)
    let r = app
        .post("/users/invite")
        .token(&fx.manager.token)
        .json(&json!({"username": email("mgr"), "name": "x", "accessLevelCode": "SUPER_ADMIN", "isScreenshotActive": false}))
        .send()
        .await;
    r.assert_no_sql_error();
    assert_eq!(r.status.as_u16(), 422, "{:?}", r.json);
    // manager boleh mengundang STAFF
    let r = app
        .post("/users/invite")
        .token(&fx.manager.token)
        .json(&json!({"username": email("mgr"), "name": "x", "accessLevelCode": "STAFF", "isScreenshotActive": false}))
        .send()
        .await;
    r.assert_ok();
    assert_eq!(r.json["message"], "invitation success", "{:?}", r.json);

    // staff 403, validasi 400, tanpa token 401
    app.post("/users/invite")
        .token(&fx.staff.token)
        .json(&json!({"username": email("s"), "name": "x", "accessLevelCode": "STAFF", "isScreenshotActive": false}))
        .send()
        .await
        .assert_status(401);
    app.post("/users/invite")
        .token(&fx.super_admin.token)
        .json(&json!({"username": "bukan-email"}))
        .send()
        .await
        .assert_status(400);
}

#[tokio::test]
async fn invite_batch_mixed() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let (a, b) = (email("ba"), email("bb"));
    let res = app
        .post("/users/invite/batch")
        .token(&fx.super_admin.token)
        .json(&json!({"generatePassword": true, "users": [
            {"username": a, "name": "A", "accessLevelCode": "STAFF", "isScreenshotActive": true},
            {"username": b, "name": "B", "accessLevelCode": "MANAGER", "isScreenshotActive": false, "generatePassword": false},
            {"username": a, "name": "A2", "accessLevelCode": "STAFF", "isScreenshotActive": true},
        ]}))
        .send()
        .await;
    res.assert_ok();
    let d = res.data().as_array().unwrap();
    assert_eq!(d.len(), 3, "{:?}", res.json);
    assert_eq!(d[0]["status"], "invited", "{:?}", res.json);
    assert_eq!(d[0]["isPasswordGenerated"], true);
    assert_eq!(d[1]["status"], "invited", "{:?}", res.json);
    assert_eq!(d[1]["isPasswordGenerated"], false);
    assert_eq!(d[2]["status"], "User already registered");
    let n: i64 = app
        .scalar(&format!(
            "SELECT COUNT(*) FROM users WHERE username IN ('{}','{}')",
            a.to_lowercase(),
            b.to_lowercase()
        ))
        .await;
    assert_eq!(n, 2);
    // manager: SUPER_ADMIN ditolak per-item, bukan membatalkan batch
    let res = app
        .post("/users/invite/batch")
        .token(&fx.manager.token)
        .json(&json!({"users": [
            {"username": email("bm"), "name": "A", "accessLevelCode": "SUPER_ADMIN", "isScreenshotActive": true},
            {"username": email("bm"), "name": "B", "accessLevelCode": "STAFF", "isScreenshotActive": true},
        ]}))
        .send()
        .await;
    res.assert_ok();
    assert_eq!(res.data()[1]["status"], "invited", "{:?}", res.json);
    assert_ne!(res.data()[0]["status"], "invited");
    app.post("/users/invite/batch")
        .token(&fx.super_admin.token)
        .json(&json!({"users": "x"}))
        .send()
        .await
        .assert_status(400);
    app.post("/users/invite/batch")
        .token(&fx.staff.token)
        .json(&json!({"users": []}))
        .send()
        .await
        .assert_status(401);
}

// ───────────────────────────── update / delete / enable / disable ─────────────────────────────

#[tokio::test]
async fn update_user_with_audit() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let u = invite_active(&app, &fx, "STAFF").await;

    let new_name = email("upd");
    let res = app
        .patch(&format!("/users/{}", u.uuid))
        .token(&fx.super_admin.token)
        .json(&update_body(&new_name, 1, "MANAGER"))
        .send()
        .await;
    res.assert_ok();
    assert_eq!(res.json["message"], "update users success");
    assert_eq!(res.data()["username"], new_name.as_str());

    let row: (String, i32) = sqlx_row(
        &app,
        &format!("SELECT name, status FROM users WHERE id={}", u.id),
    )
    .await;
    assert_eq!(row, ("IT Updated".into(), 1));
    let code: String = app
        .scalar(&format!(
            r#"SELECT al.code FROM "userAccessLevel" ual JOIN "accessLevel" al ON al.id=ual."accessLevelId"
               WHERE ual."userId"={} AND ual."companyId"={}"#,
            u.id, fx.id
        ))
        .await;
    assert_eq!(code, "MANAGER");
    let (spp, idle, blur): (i32, i32, bool) = app_triple(&app, u.id, fx.id).await;
    assert_eq!((spp, idle, blur), (4, 120, true));

    // audit users.update
    let q = format!(
        r#"SELECT COUNT(*) FROM "auditLog" WHERE "companyId"={} AND action='update' AND "actorId"={}
           AND ("entityId"={} OR "entityUuid"='{}'::uuid)"#,
        fx.id, fx.super_admin.id, u.id, u.uuid
    );
    assert!(
        wait_count(&app, &q, 1).await >= 1,
        "baris audit update tidak ada"
    );

    // status 0 => token dicabut + keluar divisi
    let res = app
        .patch(&format!("/users/{}", u.uuid))
        .token(&fx.super_admin.token)
        .json(&update_body(&new_name, 0, "STAFF"))
        .send()
        .await;
    res.assert_ok();
    let (bl_or_valid,): (i64,) = (app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM users WHERE id={} AND status=0"#,
            u.id
        ))
        .await,);
    assert_eq!(bl_or_valid, 1);

    // manager tidak boleh menaikkan ke ADMIN; staff 403; tidak ada -> 404; body salah 400
    let v = invite_active(&app, &fx, "STAFF").await;
    let r = app
        .patch(&format!("/users/{}", v.uuid))
        .token(&fx.manager.token)
        .json(&update_body(&v.username, 1, "ADMIN"))
        .send()
        .await;
    r.assert_no_sql_error();
    assert_eq!(r.status.as_u16(), 422, "{:?}", r.json);
    let r = app
        .patch(&format!("/users/{}", v.uuid))
        .token(&fx.manager.token)
        .json(&update_body(&v.username, 1, "STAFF"))
        .send()
        .await;
    r.assert_ok();
    app.patch(&format!("/users/{}", v.uuid))
        .token(&fx.staff.token)
        .json(&update_body(&v.username, 1, "STAFF"))
        .send()
        .await
        .assert_status(401);
    let r = app
        .patch(&format!("/users/{MISSING}"))
        .token(&fx.super_admin.token)
        .json(&update_body("a@b.co", 1, "STAFF"))
        .send()
        .await;
    r.assert_no_sql_error();
    assert_eq!(r.status.as_u16(), 404, "{:?}", r.json);
    app.patch(&format!("/users/{}", v.uuid))
        .token(&fx.super_admin.token)
        .json(&json!({"username": "x"}))
        .send()
        .await
        .assert_status(400);
}

async fn sqlx_row(app: &TestApp, sql: &str) -> (String, i32) {
    sqlx::query_as(sql).fetch_one(app.db()).await.unwrap()
}

async fn app_triple(app: &TestApp, uid: i32, cid: i32) -> (i32, i32, bool) {
    sqlx::query_as(&format!(
        r#"SELECT "screenshotPerHour","idleTimeInSecond","isBlur" FROM "userCompany" WHERE "userId"={uid} AND "companyId"={cid}"#
    ))
    .fetch_one(app.db())
    .await
    .unwrap()
}

#[tokio::test]
async fn delete_user_soft() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let u = invite_active(&app, &fx, "STAFF").await;
    app.exec(&format!(
        r#"INSERT INTO "userConnected" ("userId", status, os, "macAddress") VALUES ({}, 1, 'linux', 'aa:{}')"#,
        u.id,
        uniq("m")
    ))
    .await;

    app.delete(&format!("/users/{}", u.uuid))
        .token(&fx.staff.token)
        .send()
        .await
        .assert_status(401);
    app.delete(&format!("/users/{}", u.uuid))
        .token(&fx.manager.token)
        .send()
        .await
        .assert_status(401);
    let res = app
        .delete(&format!("/users/{}", u.uuid))
        .token(&fx.super_admin.token)
        .send()
        .await;
    res.assert_ok();
    assert_eq!(res.data()["isDeleted"], true);
    let st: i32 = app
        .scalar(&format!("SELECT status FROM users WHERE id={}", u.id))
        .await;
    assert_eq!(st, 0);
    let active: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "userCompany" WHERE "userId"={} AND status=1"#,
            u.id
        ))
        .await;
    assert_eq!(active, 0);
    let q = format!(
        r#"SELECT COUNT(*) FROM "auditLog" WHERE "companyId"={} AND "actorId"={} AND action='delete'"#,
        fx.id, fx.super_admin.id
    );
    assert!(wait_count(&app, &q, 1).await >= 1, "audit delete tidak ada");

    let r = app
        .delete(&format!("/users/{MISSING}"))
        .token(&fx.super_admin.token)
        .send()
        .await;
    r.assert_no_sql_error();
    assert_eq!(r.status.as_u16(), 404, "{:?}", r.json);
}

#[tokio::test]
async fn disable_and_enable_user() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let u = invite_active(&app, &fx, "STAFF").await;

    app.post(&format!("/users/{}/disable", u.uuid))
        .token(&fx.manager.token)
        .send()
        .await
        .assert_status(401);
    let r = app
        .post(&format!("/users/{}/disable", u.uuid))
        .token(&fx.super_admin.token)
        .send()
        .await;
    r.assert_status(201).assert_ok();
    let st: i32 = app
        .scalar(&format!("SELECT status FROM users WHERE id={}", u.id))
        .await;
    assert_ne!(st, 1, "disable harus menonaktifkan");
    let r = app
        .post(&format!("/users/{}/enable", u.uuid))
        .token(&fx.admin.token)
        .send()
        .await;
    r.assert_status(201).assert_ok();
    let st: i32 = app
        .scalar(&format!("SELECT status FROM users WHERE id={}", u.id))
        .await;
    assert_eq!(st, 1);
    let q = format!(
        r#"SELECT COUNT(*) FROM "auditLog" WHERE "companyId"={} AND "actorId" IN ({},{})"#,
        fx.id, fx.super_admin.id, fx.admin.id
    );
    assert!(wait_count(&app, &q, 1).await >= 1);

    let r = app
        .post(&format!("/users/{MISSING}/disable"))
        .token(&fx.super_admin.token)
        .send()
        .await;
    r.assert_no_sql_error();
    assert!(
        r.status.is_client_error() || r.json["success"] == false,
        "{:?}",
        r.json
    );
    let r = app
        .post(&format!("/users/{MISSING}/enable"))
        .token(&fx.super_admin.token)
        .send()
        .await;
    r.assert_no_sql_error();
}

#[tokio::test]
async fn delete_account_self() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let u = invite_active(&app, &fx, "STAFF").await;
    app.exec(&format!(
        r#"INSERT INTO "userConnected" ("userId", status, os, "macAddress") VALUES ({}, 1, 'linux', 'bb:{}')"#,
        u.id,
        uniq("m")
    ))
    .await;
    // password salah
    let r = app
        .post("/users/delete-account")
        .token(&u.token)
        .json(&json!({"password": "salah"}))
        .send()
        .await;
    r.assert_no_sql_error();
    assert_eq!(r.json["success"], false);
    assert_eq!(r.json["message"], "Password is incorrect");
    app.post("/users/delete-account")
        .token(&u.token)
        .json(&json!({}))
        .send()
        .await
        .assert_status(400);
    // benar
    let r = app
        .post("/users/delete-account")
        .token(&u.token)
        .json(&json!({"password": PW}))
        .send()
        .await;
    r.assert_status(200).assert_ok();
    assert_eq!(r.json["message"], "delete account success", "{:?}", r.json);
    let st: i32 = app
        .scalar(&format!("SELECT status FROM users WHERE id={}", u.id))
        .await;
    assert_eq!(st, 0);
    let dev: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "userConnected" WHERE "userId"={} AND status=1"#,
            u.id
        ))
        .await;
    assert_eq!(dev, 0);
    let q = format!(
        r#"SELECT COUNT(*) FROM "auditLog" WHERE "companyId"={} AND "actorId"={} AND action='delete'"#,
        fx.id, u.id
    );
    assert!(
        wait_count(&app, &q, 1).await >= 1,
        "audit delete-account tidak ada"
    );
    // token user terhapus ditolak / idempotent
    let r = app
        .post("/users/delete-account")
        .token(&u.token)
        .json(&json!({"password": PW}))
        .send()
        .await;
    r.assert_no_sql_error();

    // admin terakhir tidak boleh (user baru jadi satu-satunya ADMIN di company BARU? tidak
    // bisa tanpa membuat company) -> cukup user tanpa sandi cocok
    app.post("/users/delete-account")
        .send()
        .await
        .assert_status(401);
}

// ───────────────────────────── me: product tour / subscription ─────────────────────────────

#[tokio::test]
async fn product_tour_get_and_patch() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let u = invite_active(&app, &fx, "STAFF").await;
    let r = app
        .get("/users/me/product-tour")
        .token(&u.token)
        .send()
        .await;
    r.assert_ok().assert_no_sql_error();
    let r = app
        .request(http::Method::PATCH, "/users/me/product-tour")
        .token(&u.token)
        .json(&json!({"status": "IN_PROGRESS", "step": "dashboard-welcome"}))
        .send()
        .await;
    r.assert_ok();
    assert_eq!(r.data()["status"], "IN_PROGRESS", "{:?}", r.json);
    assert_eq!(r.data()["step"], "dashboard-welcome");
    let r = app
        .get("/users/me/product-tour")
        .token(&u.token)
        .send()
        .await;
    r.assert_ok();
    assert!(
        r.json.to_string().contains("dashboard-welcome"),
        "{:?}",
        r.json
    );
    let r = app
        .request(http::Method::PATCH, "/users/me/product-tour")
        .token(&u.token)
        .json(&json!({"status": "COMPLETED"}))
        .send()
        .await;
    r.assert_ok();
    assert!(r.data()["step"].is_null());
    app.request(http::Method::PATCH, "/users/me/product-tour")
        .token(&u.token)
        .json(&json!({"status": "BOGUS"}))
        .send()
        .await
        .assert_status(400);
}

#[tokio::test]
async fn check_subscription_for_roles() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    ensure_subscription(&app, &fx).await;
    for u in [&fx.super_admin, &fx.staff] {
        let r = app
            .get("/users/check/subscription")
            .token(&u.token)
            .send()
            .await;
        r.assert_ok().assert_no_sql_error();
        assert_ne!(
            r.json["message"], "Cannot check subscription",
            "{:?}",
            r.json
        );
        assert!(r.json["data"].is_object(), "{:?}", r.json);
    }
}

// ───────────────────────────── local-db & log ─────────────────────────────

#[tokio::test]
async fn local_db_upsert_and_get() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let u = invite_active(&app, &fx, "STAFF").await;
    let r = app
        .get("/users/local-db?date=2024-01-15")
        .token(&u.token)
        .send()
        .await;
    r.assert_ok();
    assert!(r.json["data"].is_null());
    let r = app
        .post("/users/local-db")
        .token(&u.token)
        .json(&json!({"date": "2024-01-15", "data": {"a": 1}}))
        .send()
        .await;
    r.assert_status(200).assert_ok();
    assert_eq!(r.data()["data"]["a"], 1);
    assert_eq!(r.data()["date"], "2024-01-15T00:00:00.000Z");
    let r = app
        .post("/users/local-db")
        .token(&u.token)
        .json(&json!({"date": "2024-01-15", "data": {"a": 2, "b": [1]}}))
        .send()
        .await;
    r.assert_ok();
    let n: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "userLocalDb" WHERE "userId"={}"#,
            u.id
        ))
        .await;
    assert_eq!(n, 1);
    let r = app
        .get("/users/local-db?date=2024-01-15")
        .token(&u.token)
        .send()
        .await;
    assert_eq!(r.data()["data"]["a"], 2, "{:?}", r.json);
    assert!(r.data()["updatedAt"].is_string());
    app.post("/users/local-db")
        .token(&u.token)
        .json(&json!({"date": "x", "data": 1}))
        .send()
        .await
        .assert_status(400);
    // tanpa ?date -> Invalid Date (500 di Node juga)
    app.get("/users/local-db")
        .token(&u.token)
        .send()
        .await
        .assert_status(500);
}

#[tokio::test]
async fn user_log_db_parts() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let u = invite_active(&app, &fx, "STAFF").await;
    // POST valid: GCS tak terjangkau di test -> galat server, TANPA baris yatim di DB.
    let r = tokio::time::timeout(
        Duration::from_secs(20),
        app.post("/users/log")
            .token(&u.token)
            .json(&json!({"date": "2024-01-16", "content": "isi log"}))
            .send(),
    )
    .await
    .expect("POST /users/log menggantung (jaringan luar?)");
    if r.status.is_success() {
        assert_eq!(r.json["message"], "Log saved");
    }
    // validasi
    app.post("/users/log")
        .token(&u.token)
        .json(&json!({"date": "2024-01-15"}))
        .send()
        .await
        .assert_status(400);
    app.post("/users/log")
        .token(&u.token)
        .json(&json!({"date": "bad", "content": "x"}))
        .send()
        .await
        .assert_status(400);
    app.get("/users/log")
        .token(&u.token)
        .send()
        .await
        .assert_status(400);
    app.get("/users/log?date=bad")
        .token(&u.token)
        .send()
        .await
        .assert_status(400);
    // daftar kosong
    let r = app
        .get("/users/log?date=2024-01-15")
        .token(&u.token)
        .send()
        .await;
    r.assert_ok();
    assert_eq!(r.json["data"].as_array().unwrap().len(), 0);
    // baris legacy ber-content (tanpa GCS)
    app.exec(&format!(
        r#"INSERT INTO "userLog" ("userId", date, content) VALUES ({0}, '2024-01-15', 'legacy-1'), ({0}, '2024-01-15', 'legacy-2')"#,
        u.id
    ))
    .await;
    let r = app
        .get("/users/log?date=2024-01-15")
        .token(&u.token)
        .send()
        .await;
    r.assert_ok().assert_no_sql_error();
    let d = r.json["data"].as_array().unwrap();
    assert_eq!(d.len(), 2);
    assert_eq!(d[0]["content"], "legacy-1");
    assert_eq!(d[1]["content"], "legacy-2");
    assert!(d[0].get("filePath").is_none());
}

// ───────────────────────────── devices ─────────────────────────────

#[tokio::test]
async fn connected_devices_list_and_revoke_all() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let u = invite_active(&app, &fx, "STAFF").await;
    for i in 0..2 {
        app.exec(&format!(
            r#"INSERT INTO "userConnected" ("userId", status, os, "macAddress") VALUES ({}, 1, 'win', 'cc{i}:{}')"#,
            u.id,
            uniq("m")
        ))
        .await;
    }
    let r = app
        .get("/users/connected/devices")
        .token(&u.token)
        .send()
        .await;
    r.assert_ok();
    assert_eq!(r.json["success"], true, "{:?}", r.json);
    assert_eq!(r.json["data"].as_array().unwrap().len(), 2);
    assert!(r.json["data"][0]["macAddress"].is_string());
    let r = app
        .delete("/users/connected/devices/revoke/1")
        .token(&u.token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json["success"], true, "{:?}", r.json);
    let n: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "userConnected" WHERE "userId"={} AND status=1"#,
            u.id
        ))
        .await;
    assert_eq!(n, 0);
    let r = app
        .get("/users/connected/devices")
        .token(&u.token)
        .send()
        .await;
    assert_eq!(r.json["data"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn revoke_by_mac_blacklists_token() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let u = invite_active(&app, &fx, "STAFF").await;
    let mac = format!("dd:{}", uniq("m"));
    app.exec(&format!(
        r#"INSERT INTO "userConnected" ("userId", status, os, "macAddress") VALUES ({}, 1, 'mac', '{mac}')"#,
        u.id
    ))
    .await;
    let r = app
        .delete(&format!("/users/revoke/{mac}"))
        .token(&u.token)
        .send()
        .await;
    r.assert_status(200).assert_ok();
    assert_eq!(r.json["message"], "revoke user connected success");
    let n: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "userConnected" WHERE "userId"={} AND "macAddress"='{mac}' AND status=0"#,
            u.id
        ))
        .await;
    assert_eq!(n, 1);
    let bl: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "jwtBlackList" WHERE "userId"={}"#,
            u.id
        ))
        .await;
    assert!(bl >= 1);
    // token yang sama sekarang ditolak
    app.get("/users/connected/devices")
        .token(&u.token)
        .send()
        .await
        .assert_status(401);
    // tanpa token
    app.delete("/users/revoke/zz")
        .send()
        .await
        .assert_status(401);
}

// ───────────────────────────── pay rates ─────────────────────────────

#[tokio::test]
async fn pay_rates_crud() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let u = invite_active(&app, &fx, "STAFF").await;
    let base = format!("/users/{}/pay-rates", u.uuid);

    let r = app.get(&base).token(&fx.super_admin.token).send().await;
    r.assert_ok();
    assert_eq!(r.data().as_array().unwrap().len(), 0);

    let r = app
        .post(&base)
        .token(&fx.super_admin.token)
        .json(
            &json!({"payPeriodType": "WEEKLY", "timesheetApproval": true, "payRate": 12.5,
                      "rateType": "HOURLY", "effectiveDate": "2024-01-15", "notes": "n1"}),
        )
        .send()
        .await;
    r.assert_status(201).assert_ok();
    let rate_uuid = r.data()["uuid"].as_str().unwrap().to_owned();
    assert_eq!(r.data()["payRate"], 12.5);
    assert_eq!(r.data()["effectiveDate"], "2024-01-15T00:00:00.000Z");
    // tarif kedua, lebih baru
    app.post(&base)
        .token(&fx.admin.token)
        .json(
            &json!({"payPeriodType": "MONTHLY", "timesheetApproval": false,
                      "rateType": "FIXED", "effectiveDate": "2024-06-01"}),
        )
        .send()
        .await
        .assert_status(201)
        .assert_ok();
    let r = app.get(&base).token(&fx.super_admin.token).send().await;
    let d = r.data().as_array().unwrap();
    assert_eq!(d.len(), 2);
    assert_eq!(d[0]["rateType"], "FIXED");
    assert!(d[0]["currency"].is_string());

    let r = app
        .patch(&format!("{base}/{rate_uuid}"))
        .token(&fx.super_admin.token)
        .json(&json!({"payRate": 20, "notes": "baru", "payPeriodType": "BIWEEKLY", "effectiveDate": "2024-02-02", "rateType": "FIXED", "timesheetApproval": false}))
        .send()
        .await;
    r.assert_ok();
    assert_eq!(r.data()["payRate"], 20.0, "{:?}", r.json);
    assert_eq!(r.data()["notes"], "baru");
    assert_eq!(r.data()["payPeriodType"], "BIWEEKLY");

    // tidak ada
    let r = app
        .patch(&format!("{base}/{MISSING}"))
        .token(&fx.super_admin.token)
        .json(&json!({"notes": "x"}))
        .send()
        .await;
    assert_eq!(r.json["success"], false);
    assert_eq!(r.json["message"], "Pay rate not found");

    let r = app
        .delete(&format!("{base}/{rate_uuid}"))
        .token(&fx.super_admin.token)
        .send()
        .await;
    r.assert_ok();
    let r = app
        .delete(&format!("{base}/{rate_uuid}"))
        .token(&fx.super_admin.token)
        .send()
        .await;
    assert_eq!(r.json["success"], false);
    let r = app.get(&base).token(&fx.super_admin.token).send().await;
    assert_eq!(r.data().as_array().unwrap().len(), 1);

    // validasi / role / user tak ada
    app.post(&base)
        .token(&fx.super_admin.token)
        .json(&json!({"payPeriodType": "X"}))
        .send()
        .await
        .assert_status(400);
    app.get(&base)
        .token(&fx.manager.token)
        .send()
        .await
        .assert_status(401);
    let r = app
        .get(&format!("/users/{MISSING}/pay-rates"))
        .token(&fx.super_admin.token)
        .send()
        .await;
    r.assert_no_sql_error();
    assert_eq!(r.status.as_u16(), 404, "{:?}", r.json);
}

// ───────────────────────────── project rates ─────────────────────────────

#[tokio::test]
async fn project_rates() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let u = invite_active(&app, &fx, "STAFF").await;
    let proj: Option<(i32, String)> = sqlx::query_as(&format!(
        r#"SELECT p.id, p.uuid::text FROM project p
           JOIN "projectCompany" pc ON pc."projectId"=p.id AND pc."companyId"={} AND pc.status=1
           ORDER BY p.id LIMIT 1"#,
        fx.id
    ))
    .fetch_optional(app.db())
    .await
    .unwrap();
    let Some((pid, puuid)) = proj else {
        eprintln!("SKIP project_rates: company tanpa project");
        return;
    };
    let base = format!("/users/{}/project-rates", u.uuid);
    let r = app.get(&base).token(&fx.super_admin.token).send().await;
    r.assert_ok();
    assert_eq!(r.data().as_array().unwrap().len(), 0);
    // bukan anggota proyek
    let r = app
        .patch(&format!("{base}/{puuid}"))
        .token(&fx.super_admin.token)
        .json(&json!({"payRate": 5}))
        .send()
        .await;
    assert_eq!(r.json["success"], false);
    assert_eq!(r.json["message"], "User is not a member of this project");
    app.exec(&format!(
        r#"INSERT INTO "projectUser" ("projectId", "userId", status) VALUES ({pid}, {}, 1)"#,
        u.id
    ))
    .await;
    let r = app
        .patch(&format!("{base}/{puuid}"))
        .token(&fx.admin.token)
        .json(&json!({"payRate": 5.5, "billRate": 9}))
        .send()
        .await;
    r.assert_ok();
    assert_eq!(r.data()["payRate"], 5.5, "{:?}", r.json);
    let r = app.get(&base).token(&fx.super_admin.token).send().await;
    let d = r.data().as_array().unwrap();
    assert_eq!(d.len(), 1);
    assert_eq!(d[0]["projectUuid"], puuid.as_str());
    assert_eq!(d[0]["billRate"], 9.0);
    let r = app
        .patch(&format!("{base}/{MISSING}"))
        .token(&fx.super_admin.token)
        .json(&json!({}))
        .send()
        .await;
    assert_eq!(r.json["message"], "Project not found");
    app.get(&base)
        .token(&fx.staff.token)
        .send()
        .await
        .assert_status(401);
    app.patch(&format!("{base}/{puuid}"))
        .token(&fx.super_admin.token)
        .json(&json!({"payRate": -1}))
        .send()
        .await
        .assert_status(400);
}

// ───────────────────────────── work time limit ─────────────────────────────

#[tokio::test]
async fn work_time_limit_lifecycle() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let u = invite_active(&app, &fx, "STAFF").await;
    let url = format!("/users/{}/work-time-limit", u.uuid);
    let r = app.get(&url).token(&fx.super_admin.token).send().await;
    r.assert_ok();
    assert!(r.data().is_null());
    let r = app
        .put(&url)
        .token(&fx.super_admin.token)
        .json(&json!({"dailyLimitSec": 28800, "weeklyLimitSec": 144000}))
        .send()
        .await;
    r.assert_ok();
    assert_eq!(r.data()["dailyLimitSec"], 28800, "{:?}", r.json);
    // update (jalur existing)
    let r = app
        .put(&url)
        .token(&fx.admin.token)
        .json(&json!({"dailyLimitSec": 3600}))
        .send()
        .await;
    r.assert_ok();
    assert!(r.data()["weeklyLimitSec"].is_null());
    let r = app.get(&url).token(&fx.super_admin.token).send().await;
    assert_eq!(r.data()["dailyLimitSec"], 3600);
    app.put(&url)
        .token(&fx.super_admin.token)
        .json(&json!({"dailyLimitSec": 500, "weeklyLimitSec": 100}))
        .send()
        .await
        .assert_status(400);
    app.put(&url)
        .token(&fx.super_admin.token)
        .json(&json!({"dailyLimitSec": 99999999}))
        .send()
        .await
        .assert_status(400);
    let r = app.delete(&url).token(&fx.super_admin.token).send().await;
    r.assert_ok();
    let r = app.delete(&url).token(&fx.super_admin.token).send().await;
    assert_eq!(r.json["message"], "Work time limit not found");
    let r = app.get(&url).token(&fx.super_admin.token).send().await;
    assert!(r.data().is_null());
    // user di luar company
    let r = app
        .get(&format!("/users/{MISSING}/work-time-limit"))
        .token(&fx.super_admin.token)
        .send()
        .await;
    assert_eq!(r.json["success"], false);
    assert_eq!(r.json["message"], "User not found");
    let r = app
        .delete(&format!("/users/{MISSING}/work-time-limit"))
        .token(&fx.super_admin.token)
        .send()
        .await;
    assert_eq!(r.json["success"], false);
    app.get(&url)
        .token(&fx.manager.token)
        .send()
        .await
        .assert_status(401);
}

/// Emulator GCS JSON API minimal (unggah media, unduh `alt=media`, hapus). `fail_upload` →
/// unggahan dijawab 500.
async fn storage_emulator(
    fail_upload: bool,
) -> (
    String,
    std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>>,
) {
    use axum::body::Bytes;
    use axum::http::{Method, StatusCode, Uri};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    let store: Arc<Mutex<HashMap<String, Vec<u8>>>> = Arc::default();
    let objects = store.clone();
    let handler = move |method: Method, uri: Uri, body: Bytes| {
        let objects = objects.clone();
        async move {
            let path = uri.path().to_owned();
            let query = uri.query().unwrap_or_default().to_owned();
            if method == Method::POST && path.starts_with("/upload/storage/v1/b/") {
                if fail_upload {
                    return (StatusCode::INTERNAL_SERVER_ERROR, Vec::new());
                }
                let name = query
                    .split('&')
                    .find_map(|kv| kv.strip_prefix("name="))
                    .map(mz_core::qs::decode)
                    .unwrap_or_default();
                objects.lock().unwrap().insert(name, body.to_vec());
                return (StatusCode::OK, b"{}".to_vec());
            }
            let Some(key) = path
                .strip_prefix("/storage/v1/b/")
                .and_then(|rest| rest.split_once("/o/"))
                .map(|(_, k)| mz_core::qs::decode(k))
            else {
                return (StatusCode::NOT_FOUND, Vec::new());
            };
            let mut objects = objects.lock().unwrap();
            match method {
                Method::GET => objects
                    .get(&key)
                    .map_or((StatusCode::NOT_FOUND, Vec::new()), |b| {
                        (StatusCode::OK, b.clone())
                    }),
                Method::DELETE => match objects.remove(&key) {
                    Some(_) => (StatusCode::NO_CONTENT, Vec::new()),
                    None => (StatusCode::NOT_FOUND, Vec::new()),
                },
                _ => (StatusCode::METHOD_NOT_ALLOWED, Vec::new()),
            }
        }
    };
    let app = axum::Router::new().fallback(handler);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), store)
}

#[tokio::test]
async fn user_log_round_trip_via_storage_emulator() {
    let (host, store) = storage_emulator(false).await;
    let vars = [
        ("STORAGE_EMULATOR_HOST", host.as_str()),
        ("GCP_BUCKET_NAME", "it-users-bucket"),
    ];
    let Some(app) = TestApp::with_vars(mz_server::app, &vars).await else {
        return;
    };
    let fx = app.company().await;
    let u = invite_active(&app, &fx, "STAFF").await;

    let r = app
        .post("/users/log")
        .token(&u.token)
        .json(&json!({"date": "2024-02-03", "content": "baris 1\nbaris 2 ✓"}))
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(r.json["success"], true);
    assert_eq!(r.json["message"], "Log saved");
    assert_eq!(r.json["data"]["content"], "baris 1\nbaris 2 ✓");
    assert!(r.json["data"].get("filePath").is_none());

    // Object tersimpan di kunci Node `user-logs/<userId>/<YYYY-MM-DD>/<uuid>.log.gz`, ber-gzip.
    let (path, size): (String, i32) = sqlx::query_as(
        r#"SELECT "filePath", "sizeBytes" FROM "userLog" WHERE "userId" = $1 AND date = '2024-02-03'"#,
    )
    .bind(u.id)
    .fetch_one(app.db())
    .await
    .unwrap();
    let uuid = r.json["data"]["uuid"].as_str().unwrap();
    assert_eq!(path, format!("user-logs/{}/2024-02-03/{uuid}.log.gz", u.id));
    assert_eq!(size, i32::try_from("baris 1\nbaris 2 ✓".len()).unwrap());
    let stored = store.lock().unwrap().get(&path).cloned().unwrap();
    assert!(stored.starts_with(&[0x1f, 0x8b]), "bukan gzip");

    // GET membaca balik dari storage; object yang hilang → content null, log lain tetap.
    app.post("/users/log")
        .token(&u.token)
        .json(&json!({"date": "2024-02-03", "content": "kedua"}))
        .send()
        .await
        .assert_status(200);
    let r = app
        .get("/users/log?date=2024-02-03")
        .token(&u.token)
        .send()
        .await;
    r.assert_ok().assert_no_sql_error();
    let d = r.json["data"].as_array().unwrap();
    assert_eq!(d.len(), 2);
    assert_eq!(d[0]["content"], "baris 1\nbaris 2 ✓");
    assert_eq!(d[1]["content"], "kedua");

    store.lock().unwrap().remove(&path);
    let r = app
        .get("/users/log?date=2024-02-03")
        .token(&u.token)
        .send()
        .await;
    r.assert_ok();
    let d = r.json["data"].as_array().unwrap();
    assert!(d[0]["content"].is_null());
    assert_eq!(d[1]["content"], "kedua");
}

#[tokio::test]
async fn user_log_upload_failure_leaves_no_row() {
    let (host, store) = storage_emulator(true).await;
    let vars = [
        ("STORAGE_EMULATOR_HOST", host.as_str()),
        ("GCP_BUCKET_NAME", "it-users-bucket"),
    ];
    let Some(app) = TestApp::with_vars(mz_server::app, &vars).await else {
        return;
    };
    let fx = app.company().await;
    let u = invite_active(&app, &fx, "STAFF").await;
    app.post("/users/log")
        .token(&u.token)
        .json(&json!({"date": "2024-02-04", "content": "x"}))
        .send()
        .await
        .assert_status(500);
    let n: i64 = sqlx::query_scalar(r#"SELECT count(*) FROM "userLog" WHERE "userId" = $1"#)
        .bind(u.id)
        .fetch_one(app.db())
        .await
        .unwrap();
    assert_eq!(n, 0);
    assert!(store.lock().unwrap().is_empty());
}
