//! Uji integrasi `admin-pricing/**` dan `admin-prohibited-apps/**` (operator) terhadap Postgres.
//! Jalankan: `MZ_TEST_DATABASE_URL=... MZ_TEST_REQUIRE_DB=1 cargo test -p mz-admin-misc --test it_adm_misc_catalog`.

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

/// (action, targetType, targetUuid, after, reason).
type AuditRow = (
    String,
    String,
    Option<String>,
    Option<Value>,
    Option<String>,
);

fn build(state: &AppState) -> Router {
    mz_admin_misc::router(state).with_state(state.clone())
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
    (id, app.token_with(json!({ "sub": id })))
}

/// (action, targetType, targetUuid, after, reason) milik operator.
async fn audit_rows(app: &TestApp, op: i32) -> Vec<Value> {
    let rows: Vec<AuditRow> = sqlx::query_as(
        r#"SELECT action, "targetType", "targetUuid"::text, after, reason
                 FROM "operatorAuditLog" WHERE "operatorId" = $1 ORDER BY id"#,
    )
    .bind(op)
    .fetch_all(app.db())
    .await
    .unwrap();
    rows.into_iter()
        .map(|(a, t, u, after, r)| json!([a, t, u, after, r]))
        .collect()
}

fn pricing_body(name: &str) -> Value {
    json!({
        "name": name,
        "minUsers": 1,
        "maxUsers": 10,
        "basePrice": 100,
        "extraUserPrice": 10,
        "status": 1,
        "reason": "uji audit pricing",
    })
}

/// Semua route pricing + prohibited: tanpa token, token user, operator nonaktif → 401.
async fn assert_gate(app: &TestApp, company_admin: i32, inactive: &str, paths: &[(&str, &str)]) {
    for (method, path) in paths {
        let make = |_: ()| match *method {
            "GET" => app.get(path),
            "POST" => app.post(path),
            "PATCH" => app.patch(path),
            _ => app.delete(path),
        };
        make(()).send().await.assert_status(401);
        make(()).user(company_admin).send().await.assert_status(401);
        make(()).token(inactive).send().await.assert_status(401);
    }
}

#[tokio::test]
async fn pricing_gate_validation_and_crud() {
    let _g = SERIAL.lock().await;
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let c = app.company().await;
    let (op, token) = operator(&app, 1).await;
    let (_, inactive) = operator(&app, 0).await;

    assert_gate(
        &app,
        c.admin.id,
        &inactive,
        &[
            ("GET", "/admin-pricing"),
            ("POST", "/admin-pricing/create"),
            (
                "PATCH",
                "/admin-pricing/update/00000000-0000-4000-8000-000000000000",
            ),
            (
                "DELETE",
                "/admin-pricing/delete/00000000-0000-4000-8000-000000000000",
            ),
        ],
    )
    .await;

    // Validasi CreatePricingDto: semua wajib kosong → 12 pesan (urutan deklarasi).
    let r = app
        .post("/admin-pricing/create")
        .token(&token)
        .json(&json!({}))
        .send()
        .await;
    r.assert_status(400).assert_no_sql_error();
    assert_eq!(
        r.json["message"],
        json!([
            "name should not be empty",
            "name must be a string",
            "minUsers should not be empty",
            "minUsers must be a number conforming to the specified constraints",
            "maxUsers should not be empty",
            "maxUsers must be a number conforming to the specified constraints",
            "basePrice should not be empty",
            "basePrice must be a number conforming to the specified constraints",
            "extraUserPrice should not be empty",
            "extraUserPrice must be a number conforming to the specified constraints",
            "status should not be empty",
            "status must be a number conforming to the specified constraints"
        ])
    );
    // Enum opsional di luar daftar → pesan IsEnum.
    let mut bad = pricing_body("it-pr-bad");
    bad["budgetAlertDeliveryChannel"] = json!("SMS");
    let r = app
        .post("/admin-pricing/create")
        .token(&token)
        .json(&bad)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        json!([
            "budgetAlertDeliveryChannel must be one of the following values: INAPP_ONLY, EMAIL_SLACK"
        ])
    );
    assert!(audit_rows(&app, op).await.is_empty());

    // Create: 201, data {id, uuid}, audit `pricing.create` (targetUuid NULL).
    let name = format!("it-pr-{}", uniq());
    let body = pricing_body(&name);
    let r = app
        .post("/admin-pricing/create")
        .token(&token)
        .json(&body)
        .send()
        .await;
    r.assert_status(201).assert_no_sql_error();
    assert_eq!(r.json["success"], json!(true));
    assert_eq!(r.json["message"], json!("Successfully created pricing"));
    let uuid = r.json["data"]["uuid"].as_str().unwrap().to_owned();
    assert!(r.json["data"]["id"].is_i64());
    assert_eq!(
        audit_rows(&app, op).await,
        vec![json!([
            "pricing.create",
            "pricing",
            null,
            body,
            "uji audit pricing"
        ])]
    );

    // Nama sama → 201 success:false, tanpa audit baru.
    let r = app
        .post("/admin-pricing/create")
        .token(&token)
        .json(&body)
        .send()
        .await;
    r.assert_status(201);
    assert_eq!(
        r.json,
        json!({ "success": false, "message": "Pricing with this name already exists", "data": null })
    );
    assert_eq!(audit_rows(&app, op).await.len(), 1);

    // Detail: sukses, UUID tak valid → 400 ParseUUIDPipe, tak ditemukan → 200 success:false.
    let r = app
        .get(&format!("/admin-pricing/{uuid}"))
        .token(&token)
        .send()
        .await;
    r.assert_status(200).assert_ok().assert_no_sql_error();
    assert_eq!(r.json["data"]["name"], json!(name));
    app.get("/admin-pricing/bukan-uuid")
        .token(&token)
        .send()
        .await
        .assert_status(400);
    let r = app.get("/admin-pricing/create").token(&token).send().await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        json!("Validation failed (uuid is expected)")
    );
    let missing = "00000000-0000-4000-8000-000000000000";
    let r = app
        .get(&format!("/admin-pricing/{missing}"))
        .token(&token)
        .send()
        .await;
    assert_eq!(
        r.json,
        json!({ "success": false, "message": "Pricing not found", "data": null })
    );

    // Daftar: orderColumn di luar allowlist → 400; pencarian menemukan baris.
    let r = app
        .get("/admin-pricing?orderColumn=minUsers")
        .token(&token)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        json!([
            "orderColumn must be one of the following values: createdAt, name, basePrice, pricePerUserUsd, id"
        ])
    );
    let r = app
        .get(&format!(
            "/admin-pricing?search={}&orderBy=asc&itemsPerPage=5",
            &name[..12]
        ))
        .token(&token)
        .send()
        .await;
    r.assert_status(200).assert_ok().assert_no_sql_error();
    assert_eq!(r.data()["pageNumber"], json!(1));
    assert_eq!(r.data()["itemsPerPage"], json!(5));
    assert!(r.data()["total"].as_i64().unwrap() >= 1);
    assert!(
        r.data()["result"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["uuid"] == json!(uuid))
    );

    // Update. Node mencari `findFirst({ uuid, name })`: nama milik baris LAIN tidak terdeteksi
    // (quirk Node yang dipertahankan), jadi rename ke nama yang sudah ada tetap sukses.
    let other = format!("it-pr-other-{}", uniq());
    app.post("/admin-pricing/create")
        .token(&token)
        .json(&pricing_body(&other))
        .send()
        .await
        .assert_status(201);
    let mut upd = pricing_body(&other);
    let r = app
        .patch(&format!("/admin-pricing/update/{uuid}"))
        .token(&token)
        .json(&upd)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json["success"], json!(true));
    // `listFeatures: null` ke kolom Json non-null = galat Prisma → 200 success:false, tanpa audit.
    upd["name"] = json!(format!("it-pr-renamed-{}", uniq()));
    upd["listFeatures"] = Value::Null;
    let r = app
        .patch(&format!("/admin-pricing/update/{uuid}"))
        .token(&token)
        .json(&upd)
        .send()
        .await;
    assert_eq!(
        r.json,
        json!({ "success": false, "message": "Failed to update pricing", "data": null })
    );
    upd.as_object_mut().unwrap().remove("listFeatures");
    upd["basePrice"] = json!(250.5);
    let r = app
        .patch(&format!("/admin-pricing/update/{uuid}"))
        .token(&token)
        .json(&upd)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(r.json["message"], json!("Successfully updated pricing"));
    assert_eq!(r.json["data"]["uuid"], json!(uuid));
    // create, create(other), update(rename ke nama lain), update(basePrice).
    let rows = audit_rows(&app, op).await;
    assert_eq!(rows.len(), 4, "{rows:?}");
    assert_eq!(rows[2][0], json!("pricing.update"));
    assert_eq!(rows[3][0], json!("pricing.update"));
    assert_eq!(rows[3][2], json!(uuid));

    // Delete: sukses 200 + audit; setelah itu tak ditemukan.
    let r = app
        .delete(&format!("/admin-pricing/delete/{uuid}"))
        .token(&token)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(
        r.json,
        json!({ "success": true, "message": "Successfully deleted pricing", "data": null })
    );
    let rows = audit_rows(&app, op).await;
    assert_eq!(rows.last().unwrap()[0], json!("pricing.delete"));
    assert_eq!(rows.last().unwrap()[2], json!(uuid));
    let r = app
        .delete(&format!("/admin-pricing/delete/{uuid}"))
        .token(&token)
        .send()
        .await;
    assert_eq!(r.json["message"], json!("Pricing not found"));
}

#[tokio::test]
async fn prohibited_apps_gate_validation_and_crud() {
    let _g = SERIAL.lock().await;
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let c = app.company().await;
    let (op, token) = operator(&app, 1).await;
    let (_, inactive) = operator(&app, 0).await;

    assert_gate(
        &app,
        c.admin.id,
        &inactive,
        &[
            ("GET", "/admin-prohibited-apps"),
            ("POST", "/admin-prohibited-apps/create"),
            (
                "PATCH",
                "/admin-prohibited-apps/update/00000000-0000-4000-8000-000000000000",
            ),
            (
                "DELETE",
                "/admin-prohibited-apps/delete/00000000-0000-4000-8000-000000000000",
            ),
        ],
    )
    .await;

    // MaxLength lalu IsNotEmpty lalu IsString, per properti.
    let r = app
        .post("/admin-prohibited-apps/create")
        .token(&token)
        .json(&json!({}))
        .send()
        .await;
    r.assert_status(400).assert_no_sql_error();
    assert_eq!(
        r.json["message"],
        json!([
            "appName must be shorter than or equal to 100 characters",
            "appName should not be empty",
            "appName must be a string",
            "platform must be shorter than or equal to 20 characters",
            "platform should not be empty",
            "platform must be a string"
        ])
    );
    let r = app
        .get("/admin-prohibited-apps?orderColumn=uuid&pageNumber=0")
        .token(&token)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        json!([
            "pageNumber must not be less than 1",
            "orderColumn must be one of the following values: createdAt, name, appName"
        ])
    );

    // Create 201; duplikat tanpa memperhatikan huruf → 201 success:false.
    let app_name = format!("It-App-{}", uniq());
    let body = json!({ "appName": app_name, "platform": "windows", "reason": "uji" });
    let r = app
        .post("/admin-prohibited-apps/create")
        .token(&token)
        .json(&body)
        .send()
        .await;
    r.assert_status(201).assert_no_sql_error();
    assert_eq!(
        r.json["message"],
        json!("Successfully created prohibited app")
    );
    let uuid = r.json["data"]["uuid"].as_str().unwrap().to_owned();
    assert_eq!(
        audit_rows(&app, op).await,
        vec![json!([
            "prohibited_app.create",
            "prohibitedApp",
            null,
            body,
            "uji"
        ])]
    );
    let r = app
        .post("/admin-prohibited-apps/create")
        .token(&token)
        .json(&json!({ "appName": app_name.to_lowercase(), "platform": "WINDOWS" }))
        .send()
        .await;
    r.assert_status(201);
    assert_eq!(
        r.json,
        json!({ "success": false, "message": "App with this name and platform already exists", "data": null })
    );

    // Detail: UUID tak valid tanpa pipe → 200 success:false; `create` juga.
    let r = app
        .get("/admin-prohibited-apps/bukan-uuid")
        .token(&token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(
        r.json,
        json!({ "success": false, "message": "Failed to get prohibited app", "data": null })
    );
    let r = app
        .get("/admin-prohibited-apps/create")
        .token(&token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json["message"], json!("Failed to get prohibited app"));
    let r = app
        .get(&format!("/admin-prohibited-apps/{uuid}"))
        .token(&token)
        .send()
        .await;
    r.assert_status(200).assert_ok().assert_no_sql_error();
    assert_eq!(r.data()["appName"], json!(app_name));
    assert_eq!(r.data()["platform"], json!("windows"));

    // Daftar: pencarian mencocokkan appName atau platform.
    let r = app
        .get(&format!(
            "/admin-prohibited-apps?search={}",
            &app_name[..10]
        ))
        .token(&token)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert!(r.data()["total"].as_i64().unwrap() >= 1);

    // Update: nama bentrok dengan baris lain → 200 success:false; sukses → audit.
    let other = format!("Other-{}", uniq());
    app.post("/admin-prohibited-apps/create")
        .token(&token)
        .json(&json!({ "appName": other, "platform": "mac" }))
        .send()
        .await
        .assert_status(201);
    let r = app
        .patch(&format!("/admin-prohibited-apps/update/{uuid}"))
        .token(&token)
        .json(&json!({ "appName": other, "platform": "mac" }))
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(
        r.json,
        json!({ "success": false, "message": "Another app with this name and platform already exists", "data": null })
    );
    let r = app
        .patch(&format!("/admin-prohibited-apps/update/{uuid}"))
        .token(&token)
        .json(&json!({ "appName": "Renamed", "platform": "linux", "reason": "uji" }))
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(
        r.json,
        json!({ "success": true, "message": "Successfully updated prohibited app", "data": { "uuid": uuid } })
    );
    let rows = audit_rows(&app, op).await;
    assert_eq!(rows.last().unwrap()[0], json!("prohibited_app.update"));
    assert_eq!(rows.last().unwrap()[2], json!(uuid));

    // Delete: 200 + audit; lalu tak ditemukan.
    let r = app
        .delete(&format!("/admin-prohibited-apps/delete/{uuid}"))
        .token(&token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(
        r.json,
        json!({ "success": true, "message": "Successfully deleted prohibited app", "data": null })
    );
    let rows = audit_rows(&app, op).await;
    assert_eq!(rows.last().unwrap()[0], json!("prohibited_app.delete"));
    let r = app
        .delete(&format!("/admin-prohibited-apps/delete/{uuid}"))
        .token(&token)
        .send()
        .await;
    assert_eq!(r.json["message"], json!("Prohibited app not found"));
}
