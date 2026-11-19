//! Uji integrasi 17 route `/settings*` lewat HTTP (gerbang lengkap + layer audit) terhadap
//! Postgres nyata; API ClickUp diganti `MockServer`. Webhook Midtrans memakai tanda tangan
//! SHA-512 sungguhan.
//! Jalankan: `MZ_TEST_DATABASE_URL=... cargo test -p mz-settings --test it_settings_api`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::time::Duration;

use axum::Router;
use mz_core::AppState;
use mz_int_common::mock::MockServer;
use mz_testkit::{Company, TestApp};
use serde_json::{Value, json};
use sha2::{Digest, Sha512};
use tokio::sync::Mutex;

/// Semua test memakai company fixture yang sama: dijalankan berurutan.
static SERIAL: Mutex<()> = Mutex::const_new(());

const SERVER_KEY: &str = "SB-test-server-key";
const HOOK: &str = "http://be.test/integrations/clickup/webhook";

fn build(state: &AppState) -> Router {
    mz_settings::router(state)
        .with_state(state.clone())
        .layer(mz_audit::layer())
}

struct Ctx {
    app: TestApp,
    mock: MockServer,
    c: Company,
    sys: String,
}

async fn setup(vars: &[(&str, &str)]) -> Option<Ctx> {
    let mock = MockServer::start().await;
    let base = mock.url();
    let mut all = vec![
        ("MZ_CLICKUP_API_BASE", base.as_str()),
        ("CLICKUP_WEBHOOK_URL", HOOK),
    ];
    all.extend_from_slice(vars);
    let app = TestApp::with_vars(build, &all).await?;
    let c = app.company().await;
    // Gerbang fitur `company-settings` untuk semua role bawaan (matriks tidak kosong di template).
    app.exec(
        r#"INSERT INTO "rolePermission" ("accessLevelId", "featureKey", action)
           SELECT al.id, 'company-settings', a::"PermissionAction" FROM "accessLevel" al
           CROSS JOIN (VALUES ('READ'), ('WRITE'), ('DELETE')) AS f(a)
           WHERE al."isSystem" AND al.code IN ('SYSTEM_ADMIN', 'SUPER_ADMIN', 'ADMIN', 'MANAGER')
           ON CONFLICT DO NOTHING;"#,
    )
    .await;
    let sys = system_admin_token(&app, &c).await;
    Some(Ctx { app, mock, c, sys })
}

/// Token SYSTEM_ADMIN di company fixture (dibuat bila template tidak punya).
async fn system_admin_token(app: &TestApp, fx: &Company) -> String {
    let company = fx.id;
    let id: Option<i32> = sqlx::query_scalar(
        r#"SELECT u.id FROM users u
           JOIN "userCompany" uc ON uc."userId" = u.id AND uc.status = 1 AND uc."companyId" = $1
           JOIN "userAccessLevel" ual ON ual."userId" = u.id AND ual.status = 1 AND ual."companyId" = $1
           JOIN "accessLevel" al ON al.id = ual."accessLevelId"
           WHERE u.status = 1 AND al.code = 'SYSTEM_ADMIN'
           ORDER BY u.id LIMIT 1"#,
    )
    .bind(company)
    .fetch_optional(app.db())
    .await
    .unwrap();
    let id = match id {
        Some(id) => id,
        None => {
            let tag = uuid::Uuid::new_v4().simple().to_string();
            let id: i32 = app
                .scalar(&format!(
                    r#"INSERT INTO users (username, password, name, status)
                       VALUES ('it.settings.sys.{tag}@test.local', 'x', 'IT Sys', 1) RETURNING id"#
                ))
                .await;
            app.exec(&format!(
                r#"INSERT INTO "userCompany" ("userId", "companyId", status, "isDefault", "idleTimeInSecond")
                     VALUES ({id}, {company}, 1, true, 300);
                   INSERT INTO "userAccessLevel" ("userId", "accessLevelId", "companyId", status)
                     SELECT {id}, al.id, {company}, 1 FROM "accessLevel" al
                     WHERE al.code = 'SYSTEM_ADMIN' AND al."isSystem";"#
            ))
            .await;
            id
        }
    };
    app.token(id)
}

fn keys(v: &Value) -> Vec<String> {
    v.as_object()
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default()
}

async fn wait_count(app: &TestApp, sql: &str, want: i64) -> i64 {
    let mut n = 0;
    for _ in 0..40 {
        n = app.scalar::<i64>(sql).await;
        if n >= want {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    n
}

#[tokio::test]
async fn it_settings_crud_and_public() {
    let _g = SERIAL.lock().await;
    let Some(ctx) = setup(&[("HOST_BE", "http://be.test")]).await else {
        return;
    };
    let (app, c, sys) = (&ctx.app, &ctx.c, &ctx.sys);

    // ── gerbang: tanpa token 401; ADMIN (bukan SYSTEM_ADMIN) 401.
    app.get("/settings").send().await.assert_status(401);
    app.get("/settings")
        .token(&c.admin.token)
        .send()
        .await
        .assert_status(401);

    // ── create: validasi (semua field wajib walau label/description diabaikan).
    let res = app
        .post("/settings")
        .token(sys)
        .json(&json!({ "code": "", "value": 5 }))
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        json!([
            "code should not be empty",
            "value must be a string",
            "label should not be empty",
            "label must be a string",
            "description should not be empty",
            "description must be a string"
        ])
    );
    assert_eq!(res.json["error"], "Bad Request");

    let res = app
        .post("/settings")
        .token(sys)
        .json(&json!({ "code": "IT_CODE_1", "value": "val1", "label": "ignored", "description": "ignored" }))
        .send()
        .await;
    res.assert_status(200).assert_no_sql_error();
    let s = &res.json["data"]["setting"];
    assert_eq!(res.json["message"], "create setting success");
    // Node: label = code, description = value.
    assert_eq!(s["label"], "IT_CODE_1");
    assert_eq!(s["description"], "val1");
    assert_eq!(s["status"], 1);
    let uuid1 = s["uuid"].as_str().unwrap().to_owned();
    // Urutan kunci sesuai schema.prisma.
    let text = res.text();
    let pos = |k: &str| text.find(&format!("\"{k}\":")).unwrap();
    assert!(pos("id") < pos("uuid") && pos("uuid") < pos("code") && pos("code") < pos("value"));
    assert!(pos("description") < pos("createdAt") && pos("createdAt") < pos("updatedAt"));
    assert!(text.contains("Z\""), "{text}");

    let res = app
        .post("/settings")
        .token(sys)
        .json(
            &json!({ "code": "IT_CODE_2", "value": "needle-x", "label": "l", "description": "d" }),
        )
        .send()
        .await;
    let uuid2 = res.json["data"]["setting"]["uuid"]
        .as_str()
        .unwrap()
        .to_owned();

    // ── list: default (tanpa paging) = semua, id desc.
    let res = app.get("/settings").token(sys).send().await;
    res.assert_status(200).assert_no_sql_error();
    let d = &res.json["data"];
    assert_eq!(res.json["message"], "Success get all data settings");
    let total = d["total"].as_i64().unwrap();
    assert_eq!(d["itemsPerPage"].as_i64().unwrap(), total);
    assert_eq!(d["pageNumber"], 1);
    let result = d["result"].as_array().unwrap();
    assert_eq!(result.len() as i64, total);
    assert_eq!(result[0]["code"], "IT_CODE_2");
    // search (case-insensitive) pada value.
    let res = app.get("/settings?search=NEEDLE-X").token(sys).send().await;
    assert_eq!(res.json["data"]["total"], 1);
    // wildcard di-escape: `%` tidak cocok semua.
    let res = app.get("/settings?search=%25").token(sys).send().await;
    assert_eq!(res.json["data"]["total"], 0);
    // paging eksplisit.
    let res = app
        .get("/settings?pageNumber=1&itemsPerPage=1&orderBy=asc&orderColumn=code")
        .token(sys)
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(res.json["data"]["result"].as_array().unwrap().len(), 1);
    assert_eq!(res.json["data"]["itemsPerPage"], 1);
    // paging setengah (itemsPerPage hilang) → Prisma menolak (500); kolom order tak dikenal 500.
    app.get("/settings?pageNumber=1")
        .token(sys)
        .send()
        .await
        .assert_status(500);
    app.get("/settings?pageNumber=1&itemsPerPage=1&orderBy=asc&orderColumn=nope")
        .token(sys)
        .send()
        .await
        .assert_status(500);
    // validasi paginator.
    let res = app.get("/settings?pageNumber=abc").token(sys).send().await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        json!(["pageNumber must be a number conforming to the specified constraints"])
    );

    // ── update.
    let res = app
        .patch(&format!("/settings/{uuid1}"))
        .token(sys)
        .json(&json!({ "value": "new-val", "label": "New Label" }))
        .send()
        .await;
    res.assert_status(200).assert_no_sql_error();
    let s = &res.json["data"]["setting"];
    assert_eq!(res.json["message"], "update setting success");
    assert_eq!(s["value"], "new-val");
    assert_eq!(s["label"], "New Label");
    assert_eq!(s["code"], "IT_CODE_1");
    assert_eq!(s["description"], "val1");
    app.patch(&format!("/settings/{uuid1}"))
        .token(sys)
        .json(&json!({ "code": "" }))
        .send()
        .await
        .assert_status(400);
    // uuid bukan uuid → ParseUUIDPipe; uuid tak dikenal → Prisma P2025 → 500.
    let res = app
        .patch("/settings/not-a-uuid")
        .token(sys)
        .json(&json!({}))
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(res.json["message"], "Validation failed (uuid is expected)");
    app.patch("/settings/11111111-1111-4111-8111-111111111111")
        .token(sys)
        .json(&json!({}))
        .send()
        .await
        .assert_status(500);
    // Nest mencocokkan path statis ke `PATCH :uuid`.
    let res = app
        .patch("/settings/prohibited-apps")
        .token(sys)
        .json(&json!({}))
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(res.json["message"], "Validation failed (uuid is expected)");
    // Method yang tidak ada: router ini membalas 405; `mz-server` mengubahnya jadi 404 `Cannot GET`.
    app.get(&format!("/settings/{uuid1}"))
        .token(sys)
        .send()
        .await
        .assert_status(405);

    // ── delete (soft).
    let res = app
        .delete(&format!("/settings/{uuid2}"))
        .token(sys)
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(
        res.text(),
        r#"{"success":true,"message":"delete setting success","data":{"isDeleted":true}}"#
    );
    let st: i32 = app
        .scalar(&format!(
            "SELECT status FROM setting WHERE uuid = '{uuid2}'"
        ))
        .await;
    assert_eq!(st, 0);
    let res = app.get("/settings?search=needle-x").token(sys).send().await;
    assert_eq!(res.json["data"]["total"], 0);
    app.delete("/settings/11111111-1111-4111-8111-111111111111")
        .token(sys)
        .send()
        .await
        .assert_status(500);
    app.delete("/settings/xyz")
        .token(sys)
        .send()
        .await
        .assert_status(500);
    app.delete("/settings/prohibited-apps")
        .token(sys)
        .send()
        .await
        .assert_status(500);

    // ── latest-version (publik).
    app.exec(
        r#"DELETE FROM "appRelease" WHERE version LIKE 'IT-%';
           INSERT INTO "latestVersion" (version, mac_url, windows_url, linux_url)
           SELECT '9.0.0', '', '', '' WHERE NOT EXISTS (SELECT 1 FROM "latestVersion");
           UPDATE "latestVersion" SET mac_url = 'http://old/mac', windows_url = 'http://old/win',
                  linux_url = 'http://old/linux'
            WHERE id = (SELECT min(id) FROM "latestVersion");"#,
    )
    .await;
    let res = app.get("/settings/latest-version").send().await;
    res.assert_status(200).assert_no_sql_error();
    assert_eq!(res.json["success"], true);
    assert_eq!(res.json["message"], "Success get latest version");
    let base_version = res.json["data"].as_str().unwrap().to_owned();
    assert_eq!(res.json["versions"]["windows"], base_version.as_str());
    let text = res.text();
    assert!(
        text.find("\"data\"").unwrap() < text.find("\"versions\"").unwrap()
            && text.find("\"versions\"").unwrap() < text.find("\"download_url\"").unwrap()
    );
    // Release LATEST aktif terbaru menimpa versi & URL platform itu.
    app.exec(
        r#"INSERT INTO "appRelease" (version, platform, channel, "fileName", sha512, "downloadUrl", "gcsPath", "releaseDate", "isActive")
           VALUES ('IT-1.0.0', 'WINDOWS', 'LATEST', 'Setup 1.0.0.exe', 'x', 'u', 'g', '2026-01-01', true),
                  ('IT-2.0.0', 'WINDOWS', 'LATEST', 'Setup 2.0.0.exe', 'x', 'u', 'g', '2026-02-01', true),
                  ('IT-3.0.0', 'WINDOWS', 'BETA', 'Setup 3.0.0.exe', 'x', 'u', 'g', '2026-03-01', true),
                  ('IT-4.0.0', 'MAC', 'LATEST', 'Mac 4.0.0.dmg', 'x', 'u', 'g', '2026-03-01', false);"#,
    )
    .await;
    let res = app.get("/settings/latest-version").send().await;
    assert_eq!(res.json["versions"]["windows"], "IT-2.0.0");
    assert_eq!(res.json["versions"]["mac"], base_version.as_str());
    assert_eq!(
        res.json["download_url"]["windows_url"],
        "http://be.test/app-releases/download/win/IT-2.0.0/Setup%202.0.0.exe"
    );
    assert_eq!(res.json["download_url"]["mac_url"], "http://old/mac");

    // ── latest-version-mobile.
    let res = app.get("/settings/latest-version-mobile").send().await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        json!([
            "type must be one of the following values: ANDROID, IOS",
            "type should not be empty"
        ])
    );
    app.exec(
        r#"DELETE FROM "latestVersionMobile" WHERE type IN ('ANDROID', 'IOS');
           INSERT INTO "latestVersionMobile" (version, type, "force") VALUES ('1.2.3', 'ANDROID', true);"#,
    )
    .await;
    let res = app
        .get("/settings/latest-version-mobile?type=ANDROID")
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(
        res.text(),
        r#"{"success":true,"message":"Success get latest version mobile","data":{"version":"1.2.3","type":"ANDROID","force":true}}"#
    );
    let res = app
        .get("/settings/latest-version-mobile?type=IOS")
        .send()
        .await;
    assert_eq!(
        res.text(),
        r#"{"success":false,"message":"Latest version not found","data":null}"#
    );

    // ── prohibited-apps.
    app.exec(
        r#"DELETE FROM "prohibitedApps" WHERE "appName" LIKE 'IT-%';
           INSERT INTO "prohibitedApps" ("appName", platform) VALUES ('IT-Game', 'WINDOWS');"#,
    )
    .await;
    let res = app.get("/settings/prohibited-apps").send().await;
    res.assert_status(200).assert_no_sql_error();
    assert_eq!(res.json["message"], "Success get prohibited apps");
    let rows = res.json["data"].as_array().unwrap();
    let last = rows.last().unwrap();
    assert_eq!(last["appName"], "IT-Game");
    assert_eq!(
        keys(last).len(),
        6,
        "id, uuid, appName, platform, createdAt, updatedAt"
    );

    // ── pricing-lists.
    let res = app.get("/settings/pricing-lists").send().await;
    res.assert_status(200).assert_no_sql_error();
    assert_eq!(res.json["message"], "success");
    let all = res.json["data"].as_array().unwrap().len();
    assert!(all > 0, "template harus punya paket aktif");
    let text = res.text();
    let order = [
        "uuid",
        "name",
        "basePrice",
        "extraUserPrice",
        "minUsers",
        "maxUsers",
        "productType",
        "listFeatures",
    ];
    let first = text.find("\"uuid\"").unwrap();
    let mut prev = first;
    for k in &order[1..] {
        let p = text[first..].find(&format!("\"{k}\"")).unwrap() + first;
        assert!(p > prev, "urutan kunci {k}");
        prev = p;
    }
    let res = app
        .get("/settings/pricing-lists?product=SALES")
        .send()
        .await;
    res.assert_status(200);
    assert!(
        res.json["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["productType"] == "SALES")
    );
    let res = app.get("/settings/pricing-lists?product=NOPE").send().await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        json!(["product must be one of the following values: SALES, TIME, FULL"])
    );
}

#[tokio::test]
async fn it_settings_approvers_clickup_and_webhook() {
    let _g = SERIAL.lock().await;
    let Some(ctx) = setup(&[("MIDTRANS_SERVER_KEY", SERVER_KEY)]).await else {
        return;
    };
    let (app, mock, c) = (&ctx.app, &ctx.mock, &ctx.c);
    let admin = &c.admin.token;

    // ───────── default-manual-time-approvers
    app.exec(&format!(
        r#"DELETE FROM "defaultManualTimeApprover" WHERE "companyId" = {0};
           DELETE FROM "projectManualTimeApprover" WHERE "userId" = {1};"#,
        c.id, c.staff.id
    ))
    .await;
    app.get("/settings/default-manual-time-approvers")
        .send()
        .await
        .assert_status(401);
    app.get("/settings/default-manual-time-approvers")
        .token(&c.staff.token)
        .send()
        .await
        .assert_status(401);
    let res = app
        .get("/settings/default-manual-time-approvers")
        .token(admin)
        .send()
        .await;
    res.assert_status(200).assert_no_sql_error();
    assert_eq!(
        res.text(),
        r#"{"success":true,"message":"Success","data":[]}"#
    );

    let res = app
        .post("/settings/default-manual-time-approvers")
        .token(admin)
        .json(&json!({ "userUuid": "x" }))
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(res.json["message"], json!(["userUuid must be a UUID"]));
    let res = app
        .post("/settings/default-manual-time-approvers")
        .token(admin)
        .json(&json!({ "userUuid": "11111111-1111-4111-8111-111111111111" }))
        .send()
        .await;
    assert_eq!(
        res.text(),
        r#"{"success":false,"message":"User not found"}"#
    );

    let res = app
        .post("/settings/default-manual-time-approvers")
        .token(admin)
        .json(&json!({ "userUuid": c.staff.uuid }))
        .send()
        .await;
    res.assert_status(200).assert_no_sql_error();
    assert_eq!(res.json["success"], true);
    assert_eq!(
        res.json["message"],
        "Default approver added and synced to all projects"
    );
    let rec_uuid = res.json["data"]["uuid"].as_str().unwrap().to_owned();
    // Sinkron ke semua project aktif company.
    let (projects, synced): (i64, i64) = sqlx::query_as(&format!(
        r#"SELECT (SELECT count(DISTINCT "projectId") FROM "projectCompany" WHERE "companyId" = {0} AND status = 1),
                  (SELECT count(*) FROM "projectManualTimeApprover" pa WHERE pa."userId" = {1} AND pa.status = 1
                     AND pa."projectId" IN (SELECT "projectId" FROM "projectCompany" WHERE "companyId" = {0} AND status = 1))"#,
        c.id, c.staff.id
    ))
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!(projects, synced);
    // Duplikat.
    let res = app
        .post("/settings/default-manual-time-approvers")
        .token(admin)
        .json(&json!({ "userUuid": c.staff.uuid }))
        .send()
        .await;
    assert_eq!(
        res.text(),
        r#"{"success":false,"message":"User is already a default approver"}"#
    );
    // Daftar.
    let res = app
        .get("/settings/default-manual-time-approvers")
        .token(admin)
        .send()
        .await;
    let item = &res.json["data"][0];
    assert_eq!(item["uuid"], rec_uuid.as_str());
    assert_eq!(item["user"]["uuid"], c.staff.uuid.as_str());
    assert_eq!(
        res.text().matches("\"username\"").count(),
        1,
        "{}",
        res.text()
    );
    // Hapus: uuid bukan uuid → 400; tak dikenal → success:false; sukses → status 0, lalu bisa
    // ditambahkan lagi (baris lama dihidupkan).
    let res = app
        .delete("/settings/default-manual-time-approvers/xyz")
        .token(admin)
        .send()
        .await;
    res.assert_status(400);
    let res = app
        .delete("/settings/default-manual-time-approvers/11111111-1111-4111-8111-111111111111")
        .token(admin)
        .send()
        .await;
    assert_eq!(
        res.text(),
        r#"{"success":false,"message":"Default approver not found"}"#
    );
    let res = app
        .delete(&format!(
            "/settings/default-manual-time-approvers/{rec_uuid}"
        ))
        .token(admin)
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(
        res.text(),
        r#"{"success":true,"message":"Default approver removed"}"#
    );
    let res = app
        .post("/settings/default-manual-time-approvers")
        .token(admin)
        .json(&json!({ "userUuid": c.staff.uuid }))
        .send()
        .await;
    assert_eq!(res.json["data"]["uuid"], rec_uuid.as_str());

    // ───────── ClickUp
    app.exec(&format!(
        r#"UPDATE company SET "clickupApiToken" = NULL, "clickupWorkspaceId" = NULL WHERE id = {}"#,
        c.id
    ))
    .await;
    app.get("/settings/company/clickup-token")
        .token(&c.manager.token)
        .send()
        .await
        .assert_status(401);
    let res = app
        .get("/settings/company/clickup-token")
        .token(admin)
        .send()
        .await;
    assert_eq!(
        res.text(),
        r#"{"success":true,"message":"Success","data":{"hasToken":false}}"#
    );
    // Token kosong → validasi.
    let res = app
        .post("/settings/company/clickup-token")
        .token(admin)
        .json(&json!({}))
        .send()
        .await;
    assert_eq!(
        res.json["message"],
        json!(["apiToken should not be empty", "apiToken must be a string"])
    );
    // Token ditolak ClickUp.
    mock.on("GET", "/team", 401, json!({ "err": "Token invalid" }));
    let res = app
        .post("/settings/company/clickup-token")
        .token(admin)
        .json(&json!({ "apiToken": "pk_bad" }))
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(
        res.text(),
        r#"{"success":false,"message":"Invalid ClickUp API token. Please check your token and try again.","data":null}"#
    );
    let has: Option<String> = sqlx::query_scalar(&format!(
        r#"SELECT "clickupApiToken" FROM company WHERE id = {}"#,
        c.id
    ))
    .fetch_one(app.db())
    .await
    .unwrap();
    assert!(has.is_none());

    // Token valid + registrasi webhook (webhook lama dihapus dulu).
    mock.on(
        "GET",
        "/team",
        200,
        json!({ "teams": [
            { "id": "9001", "name": "Team A", "color": "#fff", "avatar": null },
            { "id": "9002", "name": "Team B" }
        ] }),
    );
    mock.on(
        "GET",
        "/team/9001/webhook",
        200,
        json!({ "webhooks": [
            { "id": "w-old", "endpoint": HOOK },
            { "id": "w-other", "endpoint": "http://elsewhere/hook" }
        ] }),
    );
    mock.on("GET", "/team/9002/webhook", 500, json!({ "err": "boom" }));
    mock.on("DELETE", "/webhook/*", 200, json!({}));
    mock.on("POST", "/team/*", 200, json!({ "id": "new" }));
    mock.clear_log();
    let res = app
        .post("/settings/company/clickup-token")
        .token(admin)
        .json(&json!({ "apiToken": "pk_good" }))
        .send()
        .await;
    res.assert_status(200).assert_no_sql_error();
    assert_eq!(
        res.text(),
        r#"{"success":true,"message":"ClickUp API token saved successfully","data":null}"#
    );
    let team_calls = mock.requests_to("GET", "/team");
    assert_eq!(team_calls.len(), 1);
    assert_eq!(team_calls[0].header("authorization"), Some("pk_good"));
    let del = mock.requests();
    let del: Vec<_> = del.iter().filter(|r| r.method == "DELETE").collect();
    assert_eq!(del.len(), 1, "hanya webhook ber-endpoint kita yang dihapus");
    assert_eq!(del[0].path, "/webhook/w-old");
    let posts = mock.requests_to("POST", "/team/9001/webhook");
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].json()["endpoint"], HOOK);
    assert_eq!(posts[0].json()["events"].as_array().unwrap().len(), 8);
    // Tim B gagal (GET webhook 500) → tidak ada POST untuk B, tetapi token tersimpan.
    assert!(mock.requests_to("POST", "/team/9002/webhook").is_empty());
    let tok: String = app
        .scalar(&format!(
            r#"SELECT "clickupApiToken" FROM company WHERE id = {}"#,
            c.id
        ))
        .await;
    assert_eq!(tok, "pk_good");
    // Token tidak pernah bocor ke respons status.
    let res = app
        .get("/settings/company/clickup-token")
        .token(admin)
        .send()
        .await;
    assert_eq!(
        res.text(),
        r#"{"success":true,"message":"Success","data":{"hasToken":true}}"#
    );

    // workspaces + pilih workspace.
    let res = app
        .post("/settings/company/clickup-workspace")
        .token(admin)
        .json(&json!({ "workspaceId": "9001" }))
        .send()
        .await;
    assert_eq!(
        res.text(),
        r#"{"success":true,"message":"ClickUp workspace saved successfully","data":null}"#
    );
    assert!(
        wait_count(
            app,
            &format!(
                r#"SELECT count(*) FROM "auditLog" WHERE "entityType" = 'company' AND "entityId" = {}
                     AND "changedFields"::text LIKE '%clickupWorkspaceId%'"#,
                c.id
            ),
            1
        )
        .await
            > 0,
        "update company (clickupWorkspaceId) harus diaudit; clickupApiToken sendiri rahasia sehingga tidak tercatat"
    );
    let res = app
        .get("/settings/company/clickup-workspaces")
        .token(admin)
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(
        res.text(),
        r##"{"success":true,"message":"Success","data":[{"id":"9001","name":"Team A","color":"#fff","avatar":null,"isSelected":true},{"id":"9002","name":"Team B","isSelected":false}]}"##
    );
    mock.on("GET", "/team", 401, json!({}));
    let res = app
        .get("/settings/company/clickup-workspaces")
        .token(admin)
        .send()
        .await;
    assert_eq!(
        res.text(),
        r#"{"success":false,"message":"Invalid ClickUp API token. Please update your token.","data":null}"#
    );
    mock.on("GET", "/team", 500, json!({}));
    let res = app
        .get("/settings/company/clickup-workspaces")
        .token(admin)
        .send()
        .await;
    assert_eq!(res.json["message"], "Request failed with status code 500");

    // Disconnect: hapus webhook kita di workspace terpilih + kosongkan token/workspace.
    mock.clear_log();
    mock.on(
        "GET",
        "/team/9001/webhook",
        200,
        json!({ "webhooks": [{ "id": "w-new", "endpoint": HOOK }] }),
    );
    let res = app
        .delete("/settings/company/clickup")
        .token(admin)
        .send()
        .await;
    res.assert_status(200).assert_no_sql_error();
    assert_eq!(
        res.text(),
        r#"{"success":true,"message":"ClickUp disconnected and all synced data removed","data":null}"#
    );
    assert_eq!(mock.requests_to("DELETE", "/webhook/w-new").len(), 1);
    let (t, w): (Option<String>, Option<String>) = sqlx::query_as(&format!(
        r#"SELECT "clickupApiToken", "clickupWorkspaceId" FROM company WHERE id = {}"#,
        c.id
    ))
    .fetch_one(app.db())
    .await
    .unwrap();
    assert!(t.is_none() && w.is_none());
    let res = app
        .get("/settings/company/clickup-workspaces")
        .token(admin)
        .send()
        .await;
    assert_eq!(
        res.text(),
        r#"{"success":false,"message":"ClickUp API token not configured. Please save your token first.","data":null}"#
    );
    // Method yang tidak ada di path itu: 405 di sini, 404 `Cannot GET` lewat `mz-server`.
    app.get("/settings/company/clickup")
        .token(admin)
        .send()
        .await
        .assert_status(405);

    // ───────── webhook Midtrans
    let sub: i32 = app
        .scalar(&format!(
            r#"SELECT id FROM subscriptions WHERE "companyId" = {} ORDER BY "startDate" DESC, id DESC LIMIT 1"#,
            c.id
        ))
        .await;
    app.exec(&format!(
        r#"DELETE FROM invoices WHERE "invoiceNumber" LIKE 'INV-IT-%';
           UPDATE subscriptions SET status = 'PENDING', "endDate" = '2030-01-01 00:00:00',
                  "billingSource" = 'PAYPAL' WHERE id = {sub};
           UPDATE company SET "isTrial" = true WHERE id = {};
           INSERT INTO invoices ("subscriptionId", "invoiceNumber", "paymentUrl", "totalPrice")
           VALUES ({sub}, 'INV-IT-0001', 'http://pay', 10), ({sub}, 'INV-IT-0002', 'http://pay', 10);"#,
        c.id
    ))
    .await;
    let sign = |order: &str, code: &str, amount: &str, key: &str| {
        hex::encode(Sha512::digest(
            format!("{order}{code}{amount}{key}").as_bytes(),
        ))
    };
    let order = "INV-IT-0001-1700000000";
    let payload = |status: &str, sig: &str| {
        json!({ "order_id": order, "status_code": "200", "gross_amount": "10.00",
                "transaction_status": status, "signature_key": sig })
    };
    // Tanda tangan salah / hilang / kunci lain → 500 `Internal server error`, tidak ada efek.
    for bad in [
        payload("settlement", "deadbeef"),
        payload("settlement", &sign(order, "200", "10.00", "other-key")),
        json!({ "order_id": order, "status_code": "200", "gross_amount": "10.00", "transaction_status": "settlement" }),
        json!({}),
    ] {
        let res = app
            .post("/settings/webhook/payments")
            .json(&bad)
            .send()
            .await;
        res.assert_status(500);
        // Urutan kunci = Node (`statusCode` dulu).
        assert_eq!(
            res.text(),
            r#"{"statusCode":500,"message":"Internal server error"}"#
        );
    }
    let st: String = app
        .scalar(r#"SELECT status::text FROM invoices WHERE "invoiceNumber" = 'INV-IT-0001'"#)
        .await;
    assert_eq!(st, "UNPAID");
    // Tanda tangan benar (bukan settlement) → status invoice berubah, langganan tidak.
    let ok_sig = sign(order, "200", "10.00", SERVER_KEY);
    let res = app
        .post("/settings/webhook/payments")
        .json(&payload("expire", &ok_sig))
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(res.text(), r#"{"success":true,"message":"Success"}"#);
    let st: String = app
        .scalar(r#"SELECT status::text FROM invoices WHERE "invoiceNumber" = 'INV-IT-0001'"#)
        .await;
    assert_eq!(st, "EXPIRED");
    let sub_status: String = app
        .scalar(&format!(
            "SELECT status::text FROM subscriptions WHERE id = {sub}"
        ))
        .await;
    assert_eq!(sub_status, "PENDING");
    // Settlement → PAID, endDate +30 hari, ACTIVE, isTrial false (audit `system`).
    let res = app
        .post("/settings/webhook/payments")
        .json(&payload("settlement", &ok_sig))
        .send()
        .await;
    res.assert_status(200).assert_no_sql_error();
    assert_eq!(res.text(), r#"{"success":true,"message":"Success"}"#);
    let st: String = app
        .scalar(r#"SELECT status::text FROM invoices WHERE "invoiceNumber" = 'INV-IT-0001'"#)
        .await;
    assert_eq!(st, "PAID");
    let (sub_status, end): (String, String) = sqlx::query_as(&format!(
        r#"SELECT status::text, to_char("endDate", 'YYYY-MM-DD HH24:MI:SS') FROM subscriptions WHERE id = {sub}"#
    ))
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!(sub_status, "ACTIVE");
    assert_eq!(end, "2030-01-31 00:00:00");
    let trial: bool = app
        .scalar(&format!(
            "SELECT \"isTrial\" FROM company WHERE id = {}",
            c.id
        ))
        .await;
    assert!(!trial);
    assert!(
        wait_count(
            app,
            &format!(
                r#"SELECT count(*) FROM "auditLog" WHERE "entityType" = 'company' AND "entityId" = {}
                     AND "changedFields"::text LIKE '%isTrial%'"#,
                c.id
            ),
            1
        )
        .await
            > 0,
        "isTrial → false harus diaudit"
    );
    // Invoice PAID tidak turun lagi (`pending`/`expire` datang terlambat), dan `settlement`
    // yang dikirim ulang tidak memperpanjang langganan kedua kalinya.
    for late in ["pending", "expire", "settlement"] {
        let res = app
            .post("/settings/webhook/payments")
            .json(&payload(late, &ok_sig))
            .send()
            .await;
        res.assert_status(200).assert_no_sql_error();
        assert_eq!(res.text(), r#"{"success":true,"message":"Success"}"#);
    }
    let st: String = app
        .scalar(r#"SELECT status::text FROM invoices WHERE "invoiceNumber" = 'INV-IT-0001'"#)
        .await;
    assert_eq!(st, "PAID");
    let end: String = app
        .scalar(&format!(
            r#"SELECT to_char("endDate", 'YYYY-MM-DD HH24:MI:SS') FROM subscriptions WHERE id = {sub}"#
        ))
        .await;
    assert_eq!(
        end, "2030-01-31 00:00:00",
        "settlement ulang tidak boleh +30 hari lagi"
    );
    // Order id tanpa invoice yang cocok: tidak ada baris diubah, tetap sukses (updateMany).
    let order2 = "INV-IT-9999-1";
    let sig2 = sign(order2, "200", "10.00", SERVER_KEY);
    let res = app
        .post("/settings/webhook/payments")
        .json(
            &json!({ "order_id": order2, "status_code": "200", "gross_amount": "10.00",
                       "transaction_status": "pending", "signature_key": sig2 }),
        )
        .send()
        .await;
    assert_eq!(res.text(), r#"{"success":true,"message":"Success"}"#);
    // Settlement untuk invoice yang tidak ada → 'Subscription not found' → 500.
    let sig3 = sign("INV-IT-9998-1", "200", "10.00", SERVER_KEY);
    app.post("/settings/webhook/payments")
        .json(
            &json!({ "order_id": "INV-IT-9998-1", "status_code": "200", "gross_amount": "10.00",
                       "transaction_status": "settlement", "signature_key": sig3 }),
        )
        .send()
        .await
        .assert_status(500);
    // Langganan MANUAL diabaikan sepenuhnya.
    app.exec(&format!(
        r#"UPDATE subscriptions SET "billingSource" = 'MANUAL', status = 'PENDING' WHERE id = {sub}"#
    ))
    .await;
    let order3 = "INV-IT-0002-5";
    let sig4 = sign(order3, "200", "10.00", SERVER_KEY);
    let res = app
        .post("/settings/webhook/payments")
        .json(
            &json!({ "order_id": order3, "status_code": "200", "gross_amount": "10.00",
                       "transaction_status": "settlement", "signature_key": sig4 }),
        )
        .send()
        .await;
    assert_eq!(res.text(), r#"{"success":true,"message":"Success"}"#);
    let st: String = app
        .scalar(r#"SELECT status::text FROM invoices WHERE "invoiceNumber" = 'INV-IT-0002'"#)
        .await;
    assert_eq!(st, "UNPAID");
    let sub_status: String = app
        .scalar(&format!(
            "SELECT status::text FROM subscriptions WHERE id = {sub}"
        ))
        .await;
    assert_eq!(sub_status, "PENDING");

    // Pulihkan fixture bersama.
    app.exec(&format!(
        r#"UPDATE subscriptions SET status = 'ACTIVE', "billingSource" = 'PAYPAL',
                  "endDate" = NOW() + interval '1 year' WHERE id = {sub};
           DELETE FROM invoices WHERE "invoiceNumber" LIKE 'INV-IT-%';"#
    ))
    .await;

    // Tanpa MIDTRANS_SERVER_KEY (fail closed): tanda tangan yang dihitung dengan teks "undefined"
    // tidak boleh diterima.
    let Some(open) = TestApp::with_vars(build, &[]).await else {
        return;
    };
    let forged = sign(order, "200", "10.00", "undefined");
    open.post("/settings/webhook/payments")
        .json(&payload("settlement", &forged))
        .send()
        .await
        .assert_status(500);
}
