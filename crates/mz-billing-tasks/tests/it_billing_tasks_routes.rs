//! Uji integrasi 5 route operator `POST /admin/*/force*` terhadap Postgres nyata. PayPal diarahkan
//! ke `MockServer` lewat `MZ_PAYPAL_API_BASE`; Mailtrap mati (`MAILTRAP_TOKEN` kosong). Setiap
//! panggilan sukses dibatasi `companyId` company uji (tanpa body = semua company template).
//! Jalankan: `MZ_TEST_DATABASE_URL=... MZ_TEST_REQUIRE_DB=1 cargo test -p mz-billing-tasks --test it_billing_tasks_routes`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use axum::Router;
use mz_core::AppState;
use mz_int_common::mock::MockServer;
use mz_testkit::TestApp;
use serde_json::{Value, json};
use tokio::sync::Mutex;

static SERIAL: Mutex<()> = Mutex::const_new(());

const PATHS: [(&str, &str); 5] = [
    ("/admin/invoice-scheduler/force", "billing_force.invoice"),
    ("/admin/plan-change/force", "billing_force.plan_change"),
    (
        "/admin/subscription-expiration/force",
        "billing_force.subscription_expiration",
    ),
    (
        "/admin/invoice-scheduler/force-proration",
        "billing_force.proration",
    ),
    (
        "/admin/proration-expiration/force",
        "billing_force.proration_expiration",
    ),
];

fn build(state: &AppState) -> Router {
    mz_billing_tasks::router(state)
        .with_state(state.clone())
        .layer(mz_audit::layer())
}

async fn setup() -> Option<(TestApp, MockServer)> {
    let mock = MockServer::start().await;
    mock.on(
        "POST",
        "/v1/oauth2/token",
        200,
        json!({"access_token": "TOK", "expires_in": 3600}),
    );
    mock.on("POST", "/v1/billing/subscriptions/*", 204, Value::Null);
    mock.on("PATCH", "/v1/billing/subscriptions/*", 200, json!({}));
    let url = mock.url();
    let app = TestApp::with_vars(
        build,
        &[
            ("MAILTRAP_TOKEN", ""),
            ("MZ_PAYPAL_API_BASE", url.as_str()),
            ("PAYPAL_CLIENT_ID", "cid"),
            ("PAYPAL_CLIENT_SECRET", "sec"),
        ],
    )
    .await?;
    Some((app, mock))
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

async fn company(app: &TestApp) -> i32 {
    app.scalar(&format!(
        r#"INSERT INTO company (name, status, "isTrial") VALUES ('it-btr-{}', 1, true) RETURNING id"#,
        uniq()
    ))
    .await
}

async fn audit_rows(app: &TestApp, op: i32, action: &str) -> Vec<(Option<i32>, String)> {
    sqlx::query_as(
        r#"SELECT "companyId", "targetType" FROM "operatorAuditLog"
            WHERE "operatorId" = $1 AND action = $2 ORDER BY id"#,
    )
    .bind(op)
    .bind(action)
    .fetch_all(app.db())
    .await
    .unwrap()
}

#[tokio::test]
async fn gates_and_validation() {
    let _g = SERIAL.lock().await;
    let Some((app, _mock)) = setup().await else {
        return;
    };
    let fx = app.company().await;
    let (op, token) = operator(&app, 1).await;
    let (_, inactive) = operator(&app, 0).await;

    for (path, action) in PATHS {
        app.post(path).send().await.assert_status(401);
        app.post(path)
            .token(&fx.admin.token)
            .json(&json!({"companyId": 1}))
            .send()
            .await
            .assert_status(401);
        app.post(path)
            .token(&inactive)
            .json(&json!({"companyId": 1}))
            .send()
            .await
            .assert_status(401);

        let r = app
            .post(path)
            .token(&token)
            .json(&json!({"companyId": 0}))
            .send()
            .await;
        r.assert_status(400).assert_no_sql_error();
        assert_eq!(
            r.json["message"],
            json!(["companyId must not be less than 1"])
        );
        let r = app
            .post(path)
            .token(&token)
            .json(&json!({"companyId": "abc", "x": 1}))
            .send()
            .await;
        r.assert_status(400).assert_no_sql_error();
        // Pipe global Node melempar lebih dulu → pesan whitelist tidak ikut (mz-core global_pipe_first).
        assert_eq!(
            r.json["message"],
            json!([
                "companyId must not be less than 1",
                "companyId must be an integer number"
            ])
        );
        let r = app
            .post(path)
            .token(&token)
            .json(&json!({"companyId": 1, "x": 1}))
            .send()
            .await;
        r.assert_status(400).assert_no_sql_error();
        assert_eq!(r.json["message"], json!(["property x should not exist"]));
        assert!(audit_rows(&app, op, action).await.is_empty());
    }
}

#[tokio::test]
async fn success_scoped_to_company_writes_operator_audit() {
    let _g = SERIAL.lock().await;
    let Some((app, _mock)) = setup().await else {
        return;
    };
    let (op, token) = operator(&app, 1).await;
    let c = company(&app).await;
    let expected = [
        json!({"success": true, "message": "Processed 0 subscription(s)"}),
        json!({"success": true, "processed": 0, "skipped": 0, "details": []}),
        json!({"success": true, "d1Expired": 0, "d0Expired": 0, "manualExpired": 0,
               "downgradedToFree": 0, "details": []}),
        json!({"success": true, "processed": 0, "skipped": 0, "details": []}),
        json!({"success": true, "expired": 0, "details": []}),
    ];
    for ((path, action), want) in PATHS.iter().zip(expected) {
        // `@Type(() => Number)`: string angka diterima.
        let r = app
            .post(path)
            .token(&token)
            .json(&json!({"companyId": c.to_string()}))
            .send()
            .await;
        r.assert_status(201).assert_no_sql_error();
        assert_eq!(r.json, want, "{path}");
        // Interceptor membaca `req.body` mentah: string → companyId NULL.
        assert_eq!(
            audit_rows(&app, op, action).await,
            [(None, "system".to_owned())]
        );
        let r = app
            .post(path)
            .token(&token)
            .json(&json!({"companyId": c}))
            .send()
            .await;
        r.assert_status(201);
        assert_eq!(
            audit_rows(&app, op, action).await.last().cloned(),
            Some((Some(c), "system".to_owned()))
        );
    }
}

#[tokio::test]
async fn subscription_expiration_force_audits_as_operator() {
    let _g = SERIAL.lock().await;
    let Some((app, _mock)) = setup().await else {
        return;
    };
    let (_, token) = operator(&app, 1).await;
    let c = company(&app).await;
    let tier: i32 = app
        .scalar(&format!(
            r#"INSERT INTO "pricingTiers" (name, status, "productType", "maxUsers", "maxProject")
               VALUES ('Trial-{}', 1, 'FULL', 5, 5) RETURNING id"#,
            uniq()
        ))
        .await;
    let sid: i32 = app
        .scalar(&format!(
            r#"INSERT INTO subscriptions ("companyId", "pricingTierId", "startDate", "endDate", status)
               VALUES ({c}, {tier}, NOW() - interval '14 days', NOW() - interval '1 hour', 'TRIAL')
               RETURNING id"#
        ))
        .await;
    let r = app
        .post("/admin/subscription-expiration/force")
        .token(&token)
        .json(&json!({"companyId": c}))
        .send()
        .await;
    r.assert_status(201).assert_no_sql_error();
    assert_eq!(r.json["details"][0]["subscriptionId"], json!(sid));
    assert_eq!(r.json["details"][0]["stage"], json!("TRIAL-EXPIRED"));
    let is_trial: bool = app
        .scalar(&format!(r#"SELECT "isTrial" FROM company WHERE id = {c}"#))
        .await;
    assert!(!is_trial);
    // Audit model ditulis sesudah respons (buffer request): tunggu sebentar.
    let mut found = 0i64;
    for _ in 0..40 {
        found = app
            .scalar(&format!(
                r#"SELECT COUNT(*) FROM "auditLog" WHERE "entityType" = 'company' AND "entityId" = {c}
                     AND "actorType" = 'system' AND "actorName" = 'Timemoz Support (IT Operator)'"#
            ))
            .await;
        if found > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(found > 0, "audit company.update dengan pelaku operator");
}
