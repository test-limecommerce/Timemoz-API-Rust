//! Uji integrasi 5 route `/access-levels*` lewat HTTP (gerbang lengkap + layer audit) terhadap
//! Postgres/Redis nyata. Jalankan:
//! `MZ_TEST_DATABASE_URL=... cargo test -p mz-companies --test it_companies_access`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::time::Duration;

use axum::Router;
use fred::clients::SubscriberClient;
use fred::prelude::*;
use fred::types::Message;
use mz_core::AppState;
use mz_testkit::{Company, TestApp, TestResponse};
use serde_json::{Value, json};
use tokio::sync::Mutex;

static SERIAL: Mutex<()> = Mutex::const_new(());

const SYSADMIN: &str = "it_companies_access_sysadmin";

fn build(state: &AppState) -> Router {
    mz_companies::router(state)
        .with_state(state.clone())
        .layer(mz_audit::layer())
}

struct Ctx {
    app: TestApp,
    c: Company,
    channel: String,
    sysadmin: String,
}

async fn setup() -> Option<Ctx> {
    let channel = format!("mz:test:cache-invalidate:{}", uuid::Uuid::new_v4().simple());
    let app = TestApp::with_vars(build, &[("CACHE_INVALIDATION_CHANNEL", &channel)]).await?;
    let c = app.company().await;
    app.exec(&format!(
        r#"INSERT INTO users (username, password, name, status)
           SELECT '{SYSADMIN}', 'x', '{SYSADMIN}', 1
           WHERE NOT EXISTS (SELECT 1 FROM users WHERE username = '{SYSADMIN}');
           INSERT INTO "userCompany" ("userId", "companyId", status, "idleTimeInSecond", "isDefault")
           SELECT u.id, {0}, 1, 300, true FROM users u WHERE u.username = '{SYSADMIN}'
             AND NOT EXISTS (SELECT 1 FROM "userCompany" x WHERE x."userId" = u.id AND x."companyId" = {0});
           INSERT INTO "userAccessLevel" ("userId", "accessLevelId", "companyId", status)
           SELECT u.id, 1, {0}, 1 FROM users u WHERE u.username = '{SYSADMIN}'
             AND NOT EXISTS (SELECT 1 FROM "userAccessLevel" x WHERE x."userId" = u.id AND x."companyId" = {0});
           INSERT INTO "rolePermission" ("accessLevelId", "featureKey", action)
           SELECT al.id, 'access-levels', a::"PermissionAction" FROM "accessLevel" al
           CROSS JOIN (VALUES ('READ'), ('WRITE'), ('DELETE')) AS p(a)
           WHERE al."isSystem" AND al.code IN ('SYSTEM_ADMIN', 'SUPER_ADMIN', 'ADMIN', 'MANAGER')
           ON CONFLICT DO NOTHING;
           DELETE FROM "accessLevel" WHERE code LIKE 'IT_COMPANIES_%';"#,
        c.id
    ))
    .await;
    let id: i32 = app
        .scalar(&format!(
            "SELECT id FROM users WHERE username = '{SYSADMIN}'"
        ))
        .await;
    let sysadmin = app.token(id);
    Some(Ctx {
        app,
        c,
        channel,
        sysadmin,
    })
}

async fn audit_count(app: &TestApp, filter: &str, min: i64) -> i64 {
    let mut n = 0;
    for _ in 0..60 {
        n = app
            .scalar(&format!(
                r#"SELECT count(*) FROM "auditLog" WHERE {filter}"#
            ))
            .await;
        if n >= min {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    n
}

struct Listener {
    rx: tokio::sync::broadcast::Receiver<Message>,
    _client: SubscriberClient,
}

async fn listen(channel: &str) -> Listener {
    let client = Builder::default_centralized()
        .build_subscriber_client()
        .unwrap();
    client.init().await.unwrap();
    client.subscribe(channel).await.unwrap();
    let rx = client.message_rx();
    Listener {
        rx,
        _client: client,
    }
}

impl Listener {
    async fn saw(&mut self, kind: &str, key: &str, id: i64) -> bool {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while let Ok(Ok(msg)) = tokio::time::timeout_at(deadline, self.rx.recv()).await {
            let Some(raw) = msg.value.as_string() else {
                continue;
            };
            let v: Value = serde_json::from_str(&raw).unwrap_or_default();
            if v["kind"] == kind && v[key] == id {
                return true;
            }
        }
        false
    }
}

fn denied(res: &TestResponse) {
    assert!(
        matches!(res.status.as_u16(), 401 | 403),
        "harus ditolak, dapat {} {}",
        res.status,
        res.text()
    );
}

#[tokio::test]
async fn it_companies_access_levels_read() {
    let _g = SERIAL.lock().await;
    let Some(ctx) = setup().await else { return };
    let (app, c) = (&ctx.app, &ctx.c);

    // Gerbang: tanpa token 401; ADMIN/MANAGER/STAFF ditolak (@Roles SYSTEM_ADMIN, SUPER_ADMIN).
    app.get("/access-levels").send().await.assert_status(401);
    denied(&app.get("/access-levels").token(&c.admin.token).send().await);
    denied(&app.get("/access-levels").token(&c.staff.token).send().await);

    // SYSTEM_ADMIN melihat semua role yang boleh dilihat company-nya, termasuk baris SYSTEM_ADMIN; urutan kunci = kolom Prisma.
    let res = app.get("/access-levels").token(&ctx.sysadmin).send().await;
    res.assert_status(200).assert_no_sql_error();
    // Daftar dibatasi ke role global + role custom company aktif (company fixture).
    let total: i64 = app
        .scalar(&format!(
            r#"SELECT count(*) FROM "accessLevel" WHERE status = 1
               AND ("companyId" IS NULL OR "companyId" = {})"#,
            c.id
        ))
        .await;
    assert_eq!(res.json["success"], true);
    assert_eq!(res.json["message"], "Success get all data accessLevel");
    assert_eq!(res.json["data"]["total"], total);
    assert_eq!(res.json["data"]["itemsPerPage"], total);
    assert_eq!(res.json["data"]["pageNumber"], 1);
    let result = res.json["data"]["result"].as_array().unwrap();
    assert!(result.iter().any(|r| r["code"] == "SYSTEM_ADMIN"));
    let keys: Vec<&str> = result[0]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert!(keys.contains(&"isSystem") && keys.contains(&"overridesRoleId"));
    let raw = res.text();
    assert!(
        raw.find("\"id\"") < raw.find("\"uuid\"") && raw.find("\"uuid\"") < raw.find("\"code\"")
    );

    // SUPER_ADMIN tidak melihat baris SYSTEM_ADMIN.
    let res = app
        .get("/access-levels")
        .token(&c.super_admin.token)
        .send()
        .await;
    res.assert_status(200);
    let result = res.json["data"]["result"].as_array().unwrap();
    assert!(!result.iter().any(|r| r["code"] == "SYSTEM_ADMIN"));
    assert_eq!(res.json["data"]["total"], total - 1);

    // Paginasi + urut; validasi 400; kombinasi tak lengkap 500.
    let res = app
        .get("/access-levels?pageNumber=2&itemsPerPage=2&orderBy=asc&orderColumn=id")
        .token(&ctx.sysadmin)
        .send()
        .await;
    res.assert_status(200);
    let ids: Vec<i64> = res.json["data"]["result"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids, vec![3, 4]);
    assert_eq!(res.json["data"]["pageNumber"], 2);
    let res = app
        .get("/access-levels?itemsPerPage=x")
        .token(&ctx.sysadmin)
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        json!(["itemsPerPage must be a number conforming to the specified constraints"])
    );
    app.get("/access-levels?itemsPerPage=2")
        .token(&ctx.sysadmin)
        .send()
        .await
        .assert_status(500);

    // GET /access-levels/:uuid
    let uuid: String = app
        .scalar(
            "SELECT uuid::text FROM \"accessLevel\" WHERE code = 'ADMIN' AND \"isSystem\" LIMIT 1",
        )
        .await;
    let res = app
        .get(&format!("/access-levels/{uuid}"))
        .token(&c.super_admin.token)
        .send()
        .await;
    res.assert_status(200).assert_no_sql_error();
    assert_eq!(res.json["success"], true);
    assert_eq!(
        res.json["message"],
        format!("Success get data accessLevel by uuid {uuid}")
    );
    assert_eq!(res.json["data"]["code"], "ADMIN");
    assert_eq!(res.json["data"]["isSystem"], true);
    assert!(res.json["data"]["companyId"].is_null());
    // uuid tak dikenal: guard `@UUID` (batas company, 2026-11-17) menjawab 404 `Data not found`,
    // sama dengan role company lain; body service hanya terjangkau untuk baris berstatus 0.
    let unknown = uuid::Uuid::new_v4();
    let res = app
        .get(&format!("/access-levels/{unknown}"))
        .token(&ctx.sysadmin)
        .send()
        .await;
    res.assert_status(404);
    assert_eq!(res.json["message"], "Data not found");
    app.get("/access-levels/zzz")
        .token(&ctx.sysadmin)
        .send()
        .await
        .assert_status(400);
    denied(
        &app.get(&format!("/access-levels/{uuid}"))
            .token(&c.manager.token)
            .send()
            .await,
    );
}

#[tokio::test]
async fn it_companies_access_levels_write() {
    let _g = SERIAL.lock().await;
    let Some(ctx) = setup().await else { return };
    let (app, c) = (&ctx.app, &ctx.c);
    let mut listen = listen(&ctx.channel).await;

    // POST selalu 422 (DTO divalidasi lebih dulu); hanya SYSTEM_ADMIN.
    let res = app
        .post("/access-levels")
        .token(&ctx.sysadmin)
        .json(&json!({}))
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        json!([
            "code should not be empty",
            "code must be a string",
            "name should not be empty",
            "name must be a string"
        ])
    );
    let res = app
        .post("/access-levels")
        .token(&ctx.sysadmin)
        .json(&json!({"code": "X", "name": "Y"}))
        .send()
        .await;
    res.assert_status(422);
    assert_eq!(
        res.json,
        json!({"code": "ACCESS_LEVELS_CREATE_DEPRECATED",
               "message": "Use POST /v1/admin/roles to create a role instead"})
    );
    denied(
        &app.post("/access-levels")
            .token(&c.super_admin.token)
            .json(&json!({"code": "X", "name": "Y"}))
            .send()
            .await,
    );

    // Role bawaan tidak boleh diubah/dihapus.
    let sys_uuid: String = app
        .scalar("SELECT uuid::text FROM \"accessLevel\" WHERE code = 'MANAGER' AND \"isSystem\" LIMIT 1")
        .await;
    for res in [
        app.patch(&format!("/access-levels/{sys_uuid}"))
            .token(&ctx.sysadmin)
            .json(&json!({"code": "RENAMED"}))
            .send()
            .await,
        app.delete(&format!("/access-levels/{sys_uuid}"))
            .token(&ctx.sysadmin)
            .send()
            .await,
    ] {
        res.assert_status(422);
        assert_eq!(res.json["code"], "ACCESS_LEVEL_SYSTEM_IMMUTABLE");
        assert_eq!(
            res.json["message"],
            "Access Level 'MANAGER' is a built-in role and cannot be modified or deleted"
        );
    }
    assert_eq!(
        app.scalar::<String>(&format!(
            "SELECT code FROM \"accessLevel\" WHERE uuid = '{sys_uuid}'"
        ))
        .await,
        "MANAGER"
    );

    // Role custom: PATCH (nama), invalidasi `role`, audit entitas `role`.
    let (rid, ruuid): (i32, String) = sqlx::query_as(
        r#"INSERT INTO "accessLevel" (code, name, status, "companyId", "isSystem", "baseRole")
           VALUES ('IT_COMPANIES_CUSTOM', 'IT Custom', 1, $1, false, 'STAFF') RETURNING id, uuid::text"#,
    )
    .bind(c.id)
    .fetch_one(app.db())
    .await
    .unwrap();
    let uri = format!("/access-levels/{ruuid}");
    denied(
        &app.patch(&uri)
            .token(&c.admin.token)
            .json(&json!({"name": "Hacked"}))
            .send()
            .await,
    );
    let res = app
        .patch(&uri)
        .token(&ctx.sysadmin)
        .json(&json!({"name": "IT Custom Renamed"}))
        .send()
        .await;
    res.assert_status(200).assert_no_sql_error();
    assert_eq!(res.json["success"], true);
    assert_eq!(res.json["message"], "update accessLevel success");
    assert_eq!(res.json["data"]["name"], "IT Custom Renamed");
    assert_eq!(res.json["data"]["code"], "IT_COMPANIES_CUSTOM");
    assert_eq!(
        app.scalar::<String>(&format!(
            "SELECT name FROM \"accessLevel\" WHERE id = {rid}"
        ))
        .await,
        "IT Custom Renamed"
    );
    assert!(
        listen.saw("role", "roleId", i64::from(rid)).await,
        "pesan protokol role harus terkirim"
    );
    let n = audit_count(
        app,
        &format!(r#""entityType" = 'role' AND "entityId" = {rid} AND action = 'update'"#),
        1,
    )
    .await;
    assert_eq!(n, 1, "update accessLevel harus ter-audit");
    // Body tidak valid (bukan string) → 400.
    let res = app
        .patch(&uri)
        .token(&ctx.sysadmin)
        .json(&json!({"code": 5}))
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(res.json["message"], json!(["code must be a string"]));

    // DELETE: soft delete.
    denied(&app.delete(&uri).token(&c.admin.token).send().await);
    let res = app.delete(&uri).token(&ctx.sysadmin).send().await;
    res.assert_status(200);
    assert_eq!(
        res.json,
        json!({"success": true, "message": "delete accessLevel success", "data": {"isDeleted": true}})
    );
    assert_eq!(
        app.scalar::<i32>(&format!(
            "SELECT status FROM \"accessLevel\" WHERE id = {rid}"
        ))
        .await,
        0
    );
    // uuid tak dikenal → 404 `Data not found` dari guard `@UUID` (dulu P2025 → 500); uuid rusak → 400.
    app.delete(&format!("/access-levels/{}", uuid::Uuid::new_v4()))
        .token(&ctx.sysadmin)
        .send()
        .await
        .assert_status(404);
    app.patch(&format!("/access-levels/{}", uuid::Uuid::new_v4()))
        .token(&ctx.sysadmin)
        .json(&json!({"name": "x"}))
        .send()
        .await
        .assert_status(404);
    app.patch("/access-levels/zzz")
        .token(&ctx.sysadmin)
        .json(&json!({}))
        .send()
        .await
        .assert_status(400);
    app.exec("DELETE FROM \"accessLevel\" WHERE code LIKE 'IT_COMPANIES_%'")
        .await;
}

/// Celah lintas-tenant `@UUID` (diperbaiki 2026-11-17): SYSTEM_ADMIN company LAIN tidak boleh
/// membaca/mengubah/menghapus role custom company fixture (404 `Data not found`, data utuh).
/// Role sistem global (`companyId IS NULL`) tetap bisa dibaca; ubah/hapus ditolak service (422).
#[tokio::test]
async fn it_companies_access_levels_cross_tenant() {
    let _g = SERIAL.lock().await;
    let Some(ctx) = setup().await else { return };
    let (app, c) = (&ctx.app, &ctx.c);

    let other = app.other_company_admin().await;
    app.exec(&format!(
        r#"UPDATE "userAccessLevel"
           SET "accessLevelId" = (SELECT id FROM "accessLevel" WHERE code = 'SYSTEM_ADMIN' AND "isSystem" LIMIT 1)
           WHERE "userId" = {} AND "companyId" = {}"#,
        other.admin.id, other.id
    ))
    .await;
    let other_sysadmin = &other.admin.token;

    let (rid, ruuid): (i32, String) = sqlx::query_as(
        r#"INSERT INTO "accessLevel" (code, name, status, "companyId", "isSystem", "baseRole")
           VALUES ('IT_COMPANIES_XTENANT', 'IT XTenant', 1, $1, false, 'STAFF') RETURNING id, uuid::text"#,
    )
    .bind(c.id)
    .fetch_one(app.db())
    .await
    .unwrap();
    let uri = format!("/access-levels/{ruuid}");

    // Gerbang @Roles tetap lolos untuk pelaku (SYSTEM_ADMIN di company-nya sendiri), jadi 404 di
    // bawah memang berasal dari guard company.
    app.get("/access-levels")
        .token(other_sysadmin)
        .send()
        .await
        .assert_status(200);
    let denied = [
        app.get(&uri).token(other_sysadmin).send().await,
        app.patch(&uri)
            .token(other_sysadmin)
            .json(&json!({"name": "Hijacked"}))
            .send()
            .await,
        app.delete(&uri).token(other_sysadmin).send().await,
    ];
    for res in &denied {
        res.assert_status(404);
        assert_eq!(res.json["message"], "Data not found", "{}", res.text());
    }
    let (name, status): (String, i32) = sqlx::query_as(&format!(
        r#"SELECT name, status FROM "accessLevel" WHERE id = {rid}"#
    ))
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!((name.as_str(), status), ("IT XTenant", 1));

    // Role sistem global: boleh dibaca, tidak boleh diubah/dihapus (422 dari service).
    let global: String = app
        .scalar(
            "SELECT uuid::text FROM \"accessLevel\" WHERE code = 'MANAGER' AND \"isSystem\" LIMIT 1",
        )
        .await;
    let res = app
        .get(&format!("/access-levels/{global}"))
        .token(other_sysadmin)
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(res.json["data"]["code"], "MANAGER");
    for res in [
        app.patch(&format!("/access-levels/{global}"))
            .token(other_sysadmin)
            .json(&json!({"name": "X"}))
            .send()
            .await,
        app.delete(&format!("/access-levels/{global}"))
            .token(other_sysadmin)
            .send()
            .await,
    ] {
        res.assert_status(422);
        assert_eq!(res.json["code"], "ACCESS_LEVEL_SYSTEM_IMMUTABLE");
    }

    // Admin company pemilik tetap berhasil.
    app.get(&uri)
        .token(&ctx.sysadmin)
        .send()
        .await
        .assert_status(200);
    let res = app
        .patch(&uri)
        .token(&ctx.sysadmin)
        .json(&json!({"name": "Renamed by owner"}))
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(res.json["data"]["name"], "Renamed by owner");
    app.delete(&uri)
        .token(&ctx.sysadmin)
        .send()
        .await
        .assert_status(200);
    app.exec("DELETE FROM \"accessLevel\" WHERE code LIKE 'IT_COMPANIES_%'")
        .await;
}

/// `GET /access-levels` (daftar): role sistem global + role custom company aktif saja. Role custom
/// company lain tidak muncul (diperbaiki 2026-11-17 di Node + Rust); `total` memakai filter yang sama.
#[tokio::test]
async fn it_companies_access_levels_list_scoped_to_company() {
    let _g = SERIAL.lock().await;
    let Some(ctx) = setup().await else { return };
    let (app, c) = (&ctx.app, &ctx.c);

    let other = app.other_company_admin().await;
    app.exec(&format!(
        r#"UPDATE "userAccessLevel"
           SET "accessLevelId" = (SELECT id FROM "accessLevel" WHERE code = 'SYSTEM_ADMIN' AND "isSystem" LIMIT 1)
           WHERE "userId" = {} AND "companyId" = {}"#,
        other.admin.id, other.id
    ))
    .await;
    for (code, company) in [
        ("IT_COMPANIES_LIST_MINE", c.id),
        ("IT_COMPANIES_LIST_THEIRS", other.id),
    ] {
        app.exec(&format!(
            r#"INSERT INTO "accessLevel" (code, name, status, "companyId", "isSystem", "baseRole")
               VALUES ('{code}', '{code}', 1, {company}, false, 'STAFF')"#
        ))
        .await;
    }

    let codes = |res: &TestResponse| -> Vec<String> {
        res.json["data"]["result"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["code"].as_str().unwrap().to_owned())
            .collect()
    };
    let visible = |company: i32| {
        format!(
            r#"SELECT count(*) FROM "accessLevel" WHERE status = 1
               AND ("companyId" IS NULL OR "companyId" = {company})"#
        )
    };

    // Company fixture: role global + miliknya, bukan milik company lain.
    let res = app.get("/access-levels").token(&ctx.sysadmin).send().await;
    res.assert_status(200);
    let mine = codes(&res);
    assert!(
        mine.contains(&"IT_COMPANIES_LIST_MINE".to_owned()),
        "{mine:?}"
    );
    assert!(mine.contains(&"MANAGER".to_owned()), "{mine:?}");
    assert!(
        !mine.contains(&"IT_COMPANIES_LIST_THEIRS".to_owned()),
        "{mine:?}"
    );
    assert_eq!(
        res.json["data"]["total"],
        app.scalar::<i64>(&visible(c.id)).await
    );

    // Paginasi + urutan tetap jalan; `total` tetap dihitung dengan filter yang sama.
    let res = app
        .get("/access-levels?pageNumber=1&itemsPerPage=2&orderBy=asc&orderColumn=id")
        .token(&ctx.sysadmin)
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(codes(&res).len(), 2);
    assert_eq!(
        res.json["data"]["total"],
        app.scalar::<i64>(&visible(c.id)).await
    );

    // Company lain: kebalikannya.
    let res = app
        .get("/access-levels")
        .token(&other.admin.token)
        .send()
        .await;
    res.assert_status(200);
    let theirs = codes(&res);
    assert!(
        theirs.contains(&"IT_COMPANIES_LIST_THEIRS".to_owned()),
        "{theirs:?}"
    );
    assert!(theirs.contains(&"MANAGER".to_owned()), "{theirs:?}");
    assert!(
        !theirs.contains(&"IT_COMPANIES_LIST_MINE".to_owned()),
        "{theirs:?}"
    );
    assert_eq!(
        res.json["data"]["total"],
        app.scalar::<i64>(&visible(other.id)).await
    );

    app.exec("DELETE FROM \"accessLevel\" WHERE code LIKE 'IT_COMPANIES_%'")
        .await;
}
