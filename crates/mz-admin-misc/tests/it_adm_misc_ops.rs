//! Uji integrasi `GET /admin/audit-logs(/actions)` dan `GET|PUT /admin/mobile-versions` terhadap Postgres.
//! Jalankan: `MZ_TEST_DATABASE_URL=... MZ_TEST_REQUIRE_DB=1 cargo test -p mz-admin-misc --test it_adm_misc_ops`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use axum::Router;
use mz_core::AppState;
use mz_testkit::TestApp;
use serde_json::{Value, json};
use tokio::sync::Mutex;

static SERIAL: Mutex<()> = Mutex::const_new(());

/// (action, targetType, operatorId, before, after, reason, companyId).
type OwnerAuditRow = (
    String,
    String,
    String,
    Option<Value>,
    Option<Value>,
    Option<String>,
    Option<i32>,
);

fn build(state: &AppState) -> Router {
    mz_admin_misc::router(state).with_state(state.clone())
}

fn uniq() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// Operator dengan role tertentu (`OWNER` | `FINANCE` | `SUPPORT`) dan status.
async fn operator(app: &TestApp, role: &str, status: i32) -> (i32, String) {
    let u = uniq();
    let id: i32 = app
        .scalar(&format!(
            r#"INSERT INTO operators (username, password, name, status, role)
               VALUES ('op{u}', 'x', 'IT Operator', {status}, '{role}'::"OperatorRole") RETURNING id"#
        ))
        .await;
    (id, app.token_with(json!({ "sub": id })))
}

async fn audit_count(app: &TestApp) -> i64 {
    app.scalar(r#"SELECT COUNT(*) FROM "operatorAuditLog""#)
        .await
}

#[tokio::test]
async fn audit_logs_gate_filters_and_actions() {
    let _g = SERIAL.lock().await;
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let c = app.company().await;
    let (op, token) = operator(&app, "SUPPORT", 1).await;
    let (_, inactive) = operator(&app, "OWNER", 0).await;

    // 401: tanpa token, token user, operator nonaktif.
    app.get("/admin/audit-logs").send().await.assert_status(401);
    app.get("/admin/audit-logs")
        .user(c.admin.id)
        .send()
        .await
        .assert_status(401);
    app.get("/admin/audit-logs")
        .token(&inactive)
        .send()
        .await
        .assert_status(401);

    // Data: tiga baris audit untuk operator ini (satu dengan company).
    app.exec(&format!(
        r#"INSERT INTO "operatorAuditLog" ("operatorId", action, "targetType", after, reason, "companyId", "createdAt")
           VALUES ({op}, 'pricing.create', 'pricing', '{{"name":"a"}}', 'alasan satu', {c_id}, '2026-11-10T08:00:00'),
                  ({op}, 'pricing.update', 'pricing', NULL, NULL, NULL, '2026-11-12T08:00:00'),
                  ({op}, 'mobile_version.set', 'mobileVersion', '{{"type":"IOS"}}', NULL, NULL, '2026-11-14T08:00:00')"#,
        c_id = c.id
    ))
    .await;

    // Daftar: urutan createdAt desc; kunci respons result/pageNumber/itemsPerPage/total.
    let r = app
        .get(&format!(
            "/admin/audit-logs?operatorUuid={}",
            op_uuid(&app, op).await
        ))
        .token(&token)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(r.json["total"], json!(3));
    assert_eq!(r.json["pageNumber"], json!(1));
    assert_eq!(r.json["itemsPerPage"], json!(25));
    let keys: Vec<&str> = r
        .json
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, ["result", "pageNumber", "itemsPerPage", "total"]);
    let first = &r.json["result"][0];
    assert_eq!(first["action"], json!("mobile_version.set"));
    assert_eq!(first["operator"]["name"], json!("IT Operator"));
    assert_eq!(first["operator"]["role"], json!("SUPPORT"));
    assert_eq!(first["company"], Value::Null);
    assert_eq!(first["createdAt"], json!("2026-11-14T08:00:00.000Z"));
    let pricing = &r.json["result"][2];
    assert_eq!(pricing["company"]["uuid"], json!(c.uuid));
    assert_eq!(pricing["reason"], json!("alasan satu"));
    assert_eq!(pricing["after"], json!({ "name": "a" }));

    // Filter aksi + paging.
    let r = app
        .get("/admin/audit-logs?action=pricing.update&itemsPerPage=1&pageNumber=1")
        .token(&token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json["total"], json!(1));
    assert_eq!(r.json["result"][0]["action"], json!("pricing.update"));

    // Filter rentang: from > to → INVALID_DATE_RANGE; > 366 hari → DATE_RANGE_TOO_LARGE.
    let r = app
        .get("/admin/audit-logs?from=2026-11-15&to=2026-11-01")
        .token(&token)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json,
        json!({ "code": "INVALID_DATE_RANGE", "message": "`from` must be before or equal to `to`" })
    );
    let r = app
        .get("/admin/audit-logs?from=2025-01-01&to=2026-11-01")
        .token(&token)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json,
        json!({ "code": "DATE_RANGE_TOO_LARGE", "message": "Date range must not exceed 366 days" })
    );

    // Pipe global Node jalan lebih dulu: properti asing tidak dilaporkan selama ada nilai tak valid.
    let r = app
        .get("/admin/audit-logs?foo=1&action=bukan.aksi&operatorUuid=bukan-uuid")
        .token(&token)
        .send()
        .await;
    r.assert_status(400);
    let msgs = r.json["message"].as_array().unwrap();
    assert_eq!(msgs[0], json!("operatorUuid must be a UUID"));
    assert!(
        msgs[1]
            .as_str()
            .unwrap()
            .starts_with("action must be one of the following values: operator.create, ")
    );
    let r = app
        .get("/admin/audit-logs?from=2026-02-30T00:00:00Z&pageNumber=0")
        .token(&token)
        .send()
        .await;
    assert_eq!(
        r.json["message"],
        json!([
            "from must be a valid ISO 8601 date string",
            "pageNumber must not be less than 1"
        ])
    );

    // Daftar aksi: 74 entri, urutan Node.
    let r = app
        .get("/admin/audit-logs/actions")
        .token(&token)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    let list = r.json.as_array().unwrap();
    assert_eq!(list.len(), 74);
    assert_eq!(
        list[0],
        json!({ "action": "operator.create", "label": "Create operator" })
    );
    assert_eq!(
        list[73],
        json!({ "action": "pricing.delete", "label": "Delete pricing tier" })
    );

    // Baca tidak menulis audit (Node tanpa OperatorAuditInterceptor di controller ini).
    let before = audit_count(&app).await;
    app.get("/admin/audit-logs")
        .token(&token)
        .send()
        .await
        .assert_status(200);
    assert_eq!(audit_count(&app).await, before);
}

async fn op_uuid(app: &TestApp, id: i32) -> String {
    app.scalar(&format!(
        r#"SELECT uuid::text FROM operators WHERE id = {id}"#
    ))
    .await
}

#[tokio::test]
async fn mobile_versions_roles_validation_and_audit() {
    let _g = SERIAL.lock().await;
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let c = app.company().await;
    let (owner_op, owner) = operator(&app, "OWNER", 1).await;
    let (_, finance) = operator(&app, "FINANCE", 1).await;
    let (_, inactive) = operator(&app, "OWNER", 0).await;

    // 401 untuk semua; GET terbuka untuk ketiga role.
    app.get("/admin/mobile-versions")
        .send()
        .await
        .assert_status(401);
    app.get("/admin/mobile-versions")
        .user(c.admin.id)
        .send()
        .await
        .assert_status(401);
    app.put("/admin/mobile-versions/ANDROID")
        .token(&inactive)
        .json(&json!({}))
        .send()
        .await
        .assert_status(401);
    app.get("/admin/mobile-versions")
        .token(&finance)
        .send()
        .await
        .assert_status(200)
        .assert_no_sql_error();

    // PUT hanya OWNER/SUPPORT: FINANCE → 403 (sebelum validasi body).
    let r = app
        .put("/admin/mobile-versions/ANDROID")
        .token(&finance)
        .json(&json!({ "version": "x" }))
        .send()
        .await;
    r.assert_status(403);
    assert_eq!(
        r.json,
        json!({ "code": "OPERATOR_ROLE_FORBIDDEN", "message": "Operator role is not allowed for this endpoint" })
    );

    // Tipe platform tak dikenal → INVALID_TYPE.
    let r = app
        .put("/admin/mobile-versions/WINDOWS")
        .token(&owner)
        .json(&json!({ "reason": "uji versi", "version": "2.5.0", "force": false }))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json,
        json!({ "code": "INVALID_TYPE", "message": "type must be ANDROID or IOS" })
    );

    // Validasi: versi bukan semver, properti asing, force bukan boolean.
    let r = app
        .put("/admin/mobile-versions/ANDROID")
        .token(&owner)
        .json(&json!({ "reason": "uji versi", "version": "2.5", "force": "ya", "extra": 1 }))
        .send()
        .await;
    r.assert_status(400).assert_no_sql_error();
    assert_eq!(
        r.json["message"],
        json!([
            "version must look like 2.5.0",
            "force must be a boolean value"
        ])
    );

    // Sukses oleh OWNER: 200 `{ data }`; baris audit `mobile_version.set` dengan before NULL.
    let before = audit_count(&app).await;
    let r = app
        .put("/admin/mobile-versions/ANDROID")
        .token(&owner)
        .json(&json!({ "reason": "  uji versi  ", "version": " 2.5.0 ", "force": true }))
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(
        r.json,
        json!({ "data": { "type": "ANDROID", "version": "2.5.0", "force": true } })
    );
    assert_eq!(audit_count(&app).await, before + 1);
    let row: OwnerAuditRow = sqlx::query_as(
        r#"SELECT action, "targetType", "operatorId"::text, before, after, reason, "companyId"
                 FROM "operatorAuditLog" WHERE "operatorId" = $1 ORDER BY id DESC LIMIT 1"#,
    )
    .bind(owner_op)
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!(row.0, "mobile_version.set");
    assert_eq!(row.1, "mobileVersion");
    assert_eq!(
        row.4,
        Some(json!({ "type": "ANDROID", "version": "2.5.0", "force": true }))
    );
    assert_eq!(row.5.as_deref(), Some("uji versi"));
    assert_eq!(row.6, None);
    // `before` bergantung isi template DB (mungkin sudah ada baris ANDROID); hanya dicek di langkah kedua.

    // Perubahan kedua oleh SUPPORT: `before` terisi nilai lama.
    let (_, support) = operator(&app, "SUPPORT", 1).await;
    app.put("/admin/mobile-versions/ANDROID")
        .token(&support)
        .json(&json!({ "reason": "naikkan versi", "version": "2.6.1", "force": false }))
        .send()
        .await
        .assert_status(200);
    let before_json: Option<Value> = app
        .scalar(r#"SELECT before FROM "operatorAuditLog" ORDER BY id DESC LIMIT 1"#)
        .await;
    assert_eq!(
        before_json,
        Some(json!({ "type": "ANDROID", "version": "2.5.0", "force": true }))
    );

    // Daftar mencerminkan nilai baru; diurutkan type asc.
    let r = app.get("/admin/mobile-versions").token(&owner).send().await;
    r.assert_status(200);
    let rows = r.data().as_array().unwrap();
    let android = rows.iter().find(|x| x["type"] == json!("ANDROID")).unwrap();
    assert_eq!(
        android,
        &json!({ "type": "ANDROID", "version": "2.6.1", "force": false })
    );
}
