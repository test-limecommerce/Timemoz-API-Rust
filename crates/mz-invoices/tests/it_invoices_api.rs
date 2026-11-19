//! Uji integrasi 11 route `/invoices/**` terhadap Postgres nyata, PayPal + Midtrans tiruan
//! (`MockServer`, `MZ_PAYPAL_API_BASE` / `MIDTRANS_HOST`) dan emulator GCS di proses uji. Tidak ada
//! panggilan ke layanan sungguhan. Jalankan:
//! `MZ_TEST_DATABASE_URL=... MZ_TEST_REQUIRE_DB=1 cargo test -p mz-invoices --test it_invoices_api`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use axum::Router;
use common::{BUCKET, Emulator, start_emulator};
use mz_core::AppState;
use mz_int_common::mock::MockServer;
use mz_testkit::{OtherCompany, TestApp};
use serde_json::{Value, json};

fn build(state: &AppState) -> Router {
    mz_invoices::router(state)
        .with_state(state.clone())
        .layer(mz_audit::layer())
}

fn uniq() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

struct Env {
    app: TestApp,
    mock: MockServer,
    gcs: Emulator,
}

async fn setup() -> Option<Env> {
    let mock = MockServer::start().await;
    mock.on(
        "POST",
        "/v1/oauth2/token",
        200,
        json!({ "access_token": "tok", "expires_in": 3600 }),
    );
    let gcs = start_emulator().await;
    let base = mock.url();
    let vars = [
        ("MZ_PAYPAL_API_BASE", base.as_str()),
        ("PAYPAL_CLIENT_ID", "cid"),
        ("PAYPAL_CLIENT_SECRET", "secret"),
        ("PAYPAL_RETURN_URL", "https://fe.test/return"),
        ("PAYPAL_CANCEL_URL", "https://fe.test/cancel"),
        ("MIDTRANS_HOST", base.as_str()),
        ("MIDTRANS_KEY", "mkey"),
        ("MIDTRANS_CALLBACK", "https://fe.test/finish"),
        ("STORAGE_EMULATOR_HOST", gcs.host.as_str()),
        ("GCP_BUCKET_NAME", BUCKET),
    ];
    let app = TestApp::with_vars(build, &vars).await?;
    Some(Env { app, mock, gcs })
}

/// Balasan `createSubscription` PayPal tiruan.
fn paypal_created(id: &str) -> Value {
    json!({
        "id": id,
        "status": "APPROVAL_PENDING",
        "links": [
            { "rel": "self", "href": format!("https://paypal.test/{id}") },
            { "rel": "approve", "href": format!("https://paypal.test/approve/{id}") },
        ],
    })
}

struct Tier {
    id: i32,
    uuid: String,
}

/// Tier harga baru (nama persis `name`, mis. `Grow`/`Starter` untuk `TIER_LEVEL_ORDER`).
async fn tier(app: &TestApp, name: &str, price: f64, plan: Option<&str>, interval: &str) -> Tier {
    let plan = plan.map_or("NULL".to_owned(), |p| format!("'{p}'"));
    let (id, uuid): (i32, String) = sqlx::query_as(&format!(
        r#"INSERT INTO "pricingTiers" (name, status, "pricePerUserUsd", "basePrice", "minUsers", "maxUsers",
               "maxProject", "paypalPlanId", "billingInterval", "screenshotResolution", "screenshotRetentionDays",
               "reportExportsPerMonth", "dataHistoryMonths", "budgetAlertDeliveryChannel", "clientProofExportAllowed")
           VALUES ('{name}', 1, {price}, {price} * 2, 3, 77, 33, {plan}, '{interval}', '1920x1080', 90, 55, 24,
                   'EMAIL_SLACK', true)
           RETURNING id, uuid::text"#
    ))
    .fetch_one(app.db())
    .await
    .unwrap();
    Tier { id, uuid }
}

/// Company baru (satu ADMIN, satu langganan) dengan langganan disetel ulang.
struct Fx {
    c: OtherCompany,
    sub_id: i32,
}

async fn company(app: &TestApp, is_trial: bool, sub_set: &str) -> Fx {
    let c = app.other_company_admin().await;
    app.exec(&format!(
        r#"UPDATE company SET "isTrial" = {is_trial}, "maxUsers" = 1, "reportExportsUsedThisMonth" = 9 WHERE id = {}"#,
        c.id
    ))
    .await;
    let sub_id: i32 = app
        .scalar(&format!(
            r#"SELECT id FROM subscriptions WHERE "companyId" = {} ORDER BY id LIMIT 1"#,
            c.id
        ))
        .await;
    if !sub_set.is_empty() {
        app.exec(&format!(
            "UPDATE subscriptions SET {sub_set} WHERE id = {sub_id}"
        ))
        .await;
    }
    Fx { c, sub_id }
}

async fn invoice(app: &TestApp, sub_id: i32, status: &str, ty: &str) -> (i32, String, String) {
    let number = format!("INV-IT-{}", &uniq()[..12]);
    let (id, uuid): (i32, String) = sqlx::query_as(&format!(
        r#"INSERT INTO invoices ("subscriptionId", "invoiceNumber", "paymentUrl", "subPrice", "totalPrice", status,
               "invoiceType", "expiredAt")
           VALUES ({sub_id}, '{number}', 'https://pay.test/{number}', 10, 10.5, '{status}', '{ty}', '2026-12-01')
           RETURNING id, uuid::text"#
    ))
    .fetch_one(app.db())
    .await
    .unwrap();
    (id, uuid, number)
}

async fn row(app: &TestApp, sql: &str) -> Value {
    sqlx::query_scalar::<_, Value>(&format!("SELECT to_jsonb(x) FROM ({sql}) x"))
        .fetch_one(app.db())
        .await
        .unwrap()
}

async fn sub_row(app: &TestApp, sub_id: i32) -> Value {
    row(
        app,
        &format!(
            r#"SELECT status::text, "pricingTierId", "paypalSubscriptionId", "paypalPlanId", "paypalStatus",
                  "pricePerUser", "billingInterval", "billingSource"::text, "autoRenew", "nextInvoiceDate",
                  "pendingPricingTierId", "pendingPaypalSubscriptionId", "pendingPaypalPlanId",
                  "startDate", "endDate", (EXTRACT(EPOCH FROM ("endDate" - NOW())) / 86400)::int AS days_left,
                  (EXTRACT(EPOCH FROM ("endDate" - "nextInvoiceDate")) / 86400)::int AS next_gap
             FROM subscriptions WHERE id = {sub_id}"#
        ),
    )
    .await
}

async fn invoice_status(app: &TestApp, id: i32) -> String {
    app.scalar(&format!(
        "SELECT status::text FROM invoices WHERE id = {id}"
    ))
    .await
}

fn post<'a>(env: &'a Env, uri: &str, token: &str, body: Value) -> mz_testkit::RequestBuilder<'a> {
    env.app.post(uri).token(token).json(&body)
}

// ------------------------------------------------------------------ gerbang & validasi

#[tokio::test]
async fn gerbang_dan_validasi() {
    let Some(env) = setup().await else { return };
    let app = &env.app;
    let shared = app.company().await;
    let u = uuid::Uuid::new_v4();
    let routes: Vec<(http::Method, String)> = vec![
        (http::Method::GET, "/invoices".into()),
        (http::Method::GET, format!("/invoices/{u}")),
        (http::Method::PUT, format!("/invoices/renew/{u}")),
        (http::Method::POST, "/invoices/change-plan".into()),
        (http::Method::POST, "/invoices/change-active-plan".into()),
        (http::Method::POST, "/invoices/subscribe".into()),
        (http::Method::POST, "/invoices/cancel-subscription".into()),
        (http::Method::POST, "/invoices/resume-subscription".into()),
        (
            http::Method::POST,
            "/invoices/reactivate-subscription".into(),
        ),
        (http::Method::GET, format!("/invoices/download/pdf/{u}")),
        (http::Method::GET, format!("/invoices/{u}/pdf")),
    ];
    for (m, uri) in &routes {
        app.request(m.clone(), uri).send().await.assert_status(401);
        // RolesGuard: role salah = 401 `Access denied: insufficient permissions`.
        let r = app
            .request(m.clone(), uri)
            .token(&shared.staff.token)
            .send()
            .await;
        r.assert_status(401);
        assert_eq!(r.json["message"], "Access denied: insufficient permissions");
    }
    // MANAGER: hanya route PDF yang meloloskannya (lalu 404).
    app.get("/invoices")
        .token(&shared.manager.token)
        .send()
        .await
        .assert_status(401);
    let r = app
        .get(&format!("/invoices/{u}/pdf"))
        .token(&shared.manager.token)
        .send()
        .await;
    r.assert_status(404);
    assert_eq!(r.json["message"], "Invoice not found");

    // ParseUUIDPipe.
    let admin = &shared.admin.token;
    for uri in [
        "/invoices/xyz".to_owned(),
        "/invoices/xyz/pdf".into(),
        "/invoices/download/pdf/xyz".into(),
        "/invoices/change-plan".into(),
        // Nest: `:uuid` = `renew` / `:uuid/pdf` dengan `download`.
        "/invoices/renew".into(),
        "/invoices/download/pdf".into(),
    ] {
        let r = app.get(&uri).token(admin).send().await;
        r.assert_status(400);
        assert_eq!(
            r.json["message"], "Validation failed (uuid is expected)",
            "{uri}"
        );
    }
    let r = app.put("/invoices/renew/xyz").token(admin).send().await;
    r.assert_status(400);

    // ChangePlanDto.
    for uri in [
        "/invoices/change-plan",
        "/invoices/change-active-plan",
        "/invoices/subscribe",
    ] {
        let r = post(&env, uri, admin, json!({})).send().await;
        r.assert_status(400);
        assert_eq!(
            r.json["message"],
            json!([
                "newPricingUuid must be a UUID",
                "newPricingUuid must be a string"
            ])
        );
        let r = post(&env, uri, admin, json!({"newPricingUuid": "nope"}))
            .send()
            .await;
        assert_eq!(r.json["message"], json!(["newPricingUuid must be a UUID"]));
    }
    // Tier tak dikenal → 201 `{success:false}`.
    let r = post(
        &env,
        "/invoices/change-plan",
        admin,
        json!({"newPricingUuid": uuid::Uuid::new_v4().to_string()}),
    )
    .send()
    .await;
    r.assert_status(201).assert_no_sql_error();
    assert_eq!(r.json["success"], false);
}

// ------------------------------------------------------------------ list / detail / pdf

#[tokio::test]
async fn daftar_detail_dan_pdf() {
    let Some(env) = setup().await else { return };
    let app = &env.app;
    let fx = company(app, false, "").await;
    let token = &fx.c.admin.token;
    let (paid_id, paid_uuid, paid_no) = invoice(app, fx.sub_id, "PAID", "SUBSCRIPTION").await;
    let (_, unpaid_uuid, _) = invoice(app, fx.sub_id, "UNPAID", "SUBSCRIPTION").await;
    app.exec(&format!(
        r#"UPDATE invoices SET "createdAt" = NOW() - interval '1 day' WHERE id = {paid_id};
           INSERT INTO "invoiceItems" ("invoiceId", description, quantity, "unitPrice", "totalPrice")
           VALUES ({paid_id}, 'Monthly Subscription - 2 user(s)', 2, 5, 10)"#
    ))
    .await;

    let r = app
        .get("/invoices?pageNumber=1&itemsPerPage=10")
        .token(token)
        .send()
        .await;
    r.assert_status(200).assert_ok().assert_no_sql_error();
    let d = r.data();
    assert_eq!(d["total"], 2);
    assert_eq!(d["itemsPerPage"], 10);
    assert_eq!(d["pageNumber"], 1);
    let first = &d["result"][0];
    assert_eq!(first["uuid"], unpaid_uuid.as_str());
    assert_eq!(first["has_payment_link"], true);
    assert_eq!(first["totalPrice"], json!(10.5));
    assert_eq!(first["subPrice"], json!(10));
    assert_eq!(first["subscription"]["id"], fx.sub_id);
    assert_eq!(first["prorationDeactivatedUserCompanyIds"], json!([]));
    let keys: Vec<&String> = first.as_object().unwrap().keys().collect();
    assert_eq!(keys.first().map(|s| s.as_str()), Some("id"));
    assert_eq!(keys[keys.len() - 2], "subscription");
    assert_eq!(keys[keys.len() - 1], "has_payment_link");
    assert_eq!(d["result"][1]["has_payment_link"], false);
    assert!(d["result"][1]["createdAt"].as_str().unwrap().ends_with('Z'));

    // Filter search/status, take negatif (urutan dibalik Prisma), paginasi hilang.
    let r = app
        .get(&format!(
            "/invoices?pageNumber=1&itemsPerPage=10&search={}",
            &paid_no[4..]
        ))
        .token(token)
        .send()
        .await;
    assert_eq!(r.data()["total"], 1);
    let r = app
        .get("/invoices?pageNumber=1&itemsPerPage=10&status=PAID")
        .token(token)
        .send()
        .await;
    assert_eq!(r.data()["result"][0]["uuid"], paid_uuid.as_str());
    let r = app
        .get("/invoices?pageNumber=1&itemsPerPage=-1")
        .token(token)
        .send()
        .await;
    assert_eq!(r.data()["result"][0]["uuid"], paid_uuid.as_str());
    for q in [
        "status=NOPE&pageNumber=1&itemsPerPage=10",
        "pageNumber=1",
        "",
    ] {
        let r = app.get(&format!("/invoices?{q}")).token(token).send().await;
        r.assert_status(200);
        assert_eq!(r.json["status"], false, "{q}");
        assert_eq!(r.json["message"], "Failed to get all invoices data");
    }
    let r = app.get("/invoices?pageNumber=x").token(token).send().await;
    r.assert_status(400);

    // Detail: milik sendiri vs company lain.
    let r = app
        .get(&format!("/invoices/{paid_uuid}"))
        .token(token)
        .send()
        .await;
    r.assert_status(200).assert_ok();
    assert_eq!(r.json["message"], "Success");
    assert_eq!(r.data()["invoiceNumber"], paid_no.as_str());
    assert_eq!(r.data()["has_payment_link"], false);
    let other = app.other_company_admin().await;
    let r = app
        .get(&format!("/invoices/{paid_uuid}"))
        .token(&other.admin.token)
        .send()
        .await;
    assert_eq!(
        r.json,
        json!({"status": false, "message": "Data not found"})
    );

    // URL PDF.
    let r = app
        .get(&format!("/invoices/download/pdf/{paid_uuid}"))
        .token(token)
        .send()
        .await;
    r.assert_status(200).assert_ok();
    assert_eq!(r.data()["url"], format!("/invoices/{paid_uuid}/pdf"));
    let r = app
        .get(&format!("/invoices/download/pdf/{paid_uuid}"))
        .token(&other.admin.token)
        .send()
        .await;
    assert_eq!(
        r.json,
        json!({"success": false, "message": "Invoice not found"})
    );

    // Stream PDF (dibuat saat pertama dibuka, disimpan di bucket tiruan).
    let r = app
        .get(&format!("/invoices/{paid_uuid}/pdf"))
        .token(token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(
        r.headers.get("content-type").unwrap().to_str().unwrap(),
        "application/pdf"
    );
    assert!(r.bytes.starts_with(b"%PDF"));
    assert!(
        env.gcs
            .keys()
            .iter()
            .any(|k| k.starts_with(&format!("invoices/{paid_no}-")))
    );
    let r = app
        .get(&format!("/invoices/{paid_uuid}/pdf"))
        .token(&other.admin.token)
        .send()
        .await;
    r.assert_status(404);
}

// ------------------------------------------------------------------ change-plan

#[tokio::test]
async fn change_plan_trial_membuat_invoice_dan_menyalin_limit() {
    let Some(env) = setup().await else { return };
    let app = &env.app;
    let new_tier = tier(
        app,
        &format!("Pro-{}", &uniq()[..6]),
        4.5,
        Some("P-PRO"),
        "MONTH",
    )
    .await;
    let fx = company(app, true, r#""pricePerUser" = 3, quantity = 1"#).await;
    let (old_inv, _, _) = invoice(app, fx.sub_id, "UNPAID", "SUBSCRIPTION").await;
    env.mock.on(
        "POST",
        "/v1/billing/subscriptions",
        201,
        paypal_created("I-TRIAL"),
    );

    let r = post(
        &env,
        "/invoices/change-plan",
        &fx.c.admin.token,
        json!({"newPricingUuid": new_tier.uuid}),
    )
    .send()
    .await;
    r.assert_status(201).assert_ok();
    assert_eq!(
        r.json["message"],
        "Plan changed successfully. Please approve the new subscription."
    );
    let d = r.data();
    assert_eq!(d["paymentUrl"], "https://paypal.test/approve/I-TRIAL");
    assert_eq!(d["paypalSubscriptionId"], "I-TRIAL");
    // 1 user aktif, minUsers 3 → 3 × 4.5.
    assert_eq!(d["totalPrice"], json!(13.5));
    let keys: Vec<&String> = d.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        [
            "paymentUrl",
            "paypalSubscriptionId",
            "invoiceNumber",
            "totalPrice",
            "startDate",
            "endDate"
        ]
    );

    // PayPal: body createSubscription.
    let reqs = env.mock.requests_to("POST", "/v1/billing/subscriptions");
    assert_eq!(reqs.len(), 1);
    let body = reqs[0].json();
    assert_eq!(body["plan_id"], "P-PRO");
    assert_eq!(body["quantity"], "3");
    assert_eq!(body["custom_id"], format!("company-{}", fx.c.id));
    let number = d["invoiceNumber"].as_str().unwrap();
    assert_eq!(
        body["application_context"]["return_url"],
        format!("https://fe.test/return?invoice={number}")
    );
    assert!(body.get("start_time").is_none());

    // DB.
    let s = sub_row(app, fx.sub_id).await;
    assert_eq!(s["status"], "PENDING");
    assert_eq!(s["pricingTierId"], new_tier.id);
    assert_eq!(s["paypalSubscriptionId"], "I-TRIAL");
    assert_eq!(s["paypalPlanId"], "P-PRO");
    assert_eq!(s["paypalStatus"], "APPROVAL_PENDING");
    assert_eq!(s["pricePerUser"].as_f64(), Some(4.5));
    assert_eq!(s["billingSource"], "PAYPAL");
    assert!((29..=30).contains(&s["days_left"].as_i64().unwrap()));
    assert_eq!(invoice_status(app, old_inv).await, "CANCELLED");
    let sub = fx.sub_id;
    let inv = row(
        app,
        &format!(
            r#"SELECT i.status::text, i."subPrice", i."totalPrice", i."paymentUrl", i."paymentProvider",
                      it.description, it.quantity, it."unitPrice"
                 FROM invoices i JOIN "invoiceItems" it ON it."invoiceId" = i.id
                WHERE i."invoiceNumber" = '{number}' AND i."subscriptionId" = {sub}"#
        ),
    )
    .await;
    assert_eq!(inv["status"], "UNPAID");
    assert_eq!(inv["totalPrice"], json!(13.5));
    assert_eq!(inv["paymentUrl"], "https://paypal.test/approve/I-TRIAL");
    assert_eq!(
        inv["description"],
        "Monthly Subscription - 3 user(s) @ $4.5/user"
    );
    let co = row(
        app,
        &format!(
            r#"SELECT "isTrial", "maxUsers", "maxProject", "reportExportsUsedThisMonth",
                      "budgetAlertDeliveryChannel"::text FROM company WHERE id = {}"#,
            fx.c.id
        ),
    )
    .await;
    assert_eq!(co["isTrial"], true);
    assert_eq!(co["maxUsers"], 77);
    assert_eq!(co["maxProject"], 33);
    assert_eq!(co["reportExportsUsedThisMonth"], 0);
    assert_eq!(co["budgetAlertDeliveryChannel"], "EMAIL_SLACK");

    // Audit `company` update (layer audit menulis sesudah respons).
    let mut audited = 0i64;
    for _ in 0..40 {
        audited = app
            .scalar(&format!(
                r#"SELECT COUNT(*) FROM "auditLog" WHERE "entityType" = 'company' AND "companyId" = {}
                     AND action = 'update'"#,
                fx.c.id
            ))
            .await;
        if audited > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(audited > 0, "audit company update tidak tertulis");
}

#[tokio::test]
async fn change_plan_aktif_membatalkan_paypal_lama() {
    let Some(env) = setup().await else { return };
    let app = &env.app;
    let new_tier = tier(
        app,
        &format!("Biz-{}", &uniq()[..6]),
        10.0,
        Some("P-BIZ"),
        "YEAR",
    )
    .await;
    let fx = company(
        app,
        false,
        r#""pricePerUser" = 3, "paypalSubscriptionId" = 'I-OLD'"#,
    )
    .await;
    let (inv_id, _, inv_no) = invoice(app, fx.sub_id, "PAID", "SUBSCRIPTION").await;
    env.mock.on(
        "POST",
        "/v1/billing/subscriptions/I-OLD/cancel",
        204,
        Value::Null,
    );
    env.mock.on(
        "POST",
        "/v1/billing/subscriptions",
        201,
        paypal_created("I-BIZ"),
    );

    let r = post(
        &env,
        "/invoices/change-plan",
        &fx.c.admin.token,
        json!({"newPricingUuid": new_tier.uuid}),
    )
    .send()
    .await;
    r.assert_status(201).assert_ok();
    // 3 × 10 × 12; nomor invoice = invoice terbaru yang ada.
    assert_eq!(r.data()["totalPrice"], json!(360));
    assert_eq!(r.data()["invoiceNumber"], inv_no.as_str());
    let cancel = env
        .mock
        .requests_to("POST", "/v1/billing/subscriptions/I-OLD/cancel");
    assert_eq!(cancel[0].json()["reason"], "Plan changed by user");
    let s = sub_row(app, fx.sub_id).await;
    assert_eq!(s["billingInterval"], "YEAR");
    assert_eq!(s["paypalSubscriptionId"], "I-BIZ");
    assert_eq!(invoice_status(app, inv_id).await, "UNPAID");
    let total: f64 = app
        .scalar(&format!(
            r#"SELECT "totalPrice" FROM invoices WHERE id = {inv_id}"#
        ))
        .await;
    assert_eq!(total, 360.0);

    // PayPal gagal → 201 `{success:false, message: axios}`.
    let again = tier(
        app,
        &format!("Biz2-{}", &uniq()[..6]),
        11.0,
        Some("P-B2"),
        "MONTH",
    )
    .await;
    env.mock.on(
        "POST",
        "/v1/billing/subscriptions",
        422,
        json!({"name": "UNPROCESSABLE_ENTITY"}),
    );
    env.mock.on(
        "POST",
        "/v1/billing/subscriptions/I-BIZ/cancel",
        204,
        Value::Null,
    );
    let r = post(
        &env,
        "/invoices/change-plan",
        &fx.c.admin.token,
        json!({"newPricingUuid": again.uuid}),
    )
    .send()
    .await;
    r.assert_status(201);
    assert_eq!(
        r.json,
        json!({"success": false, "message": "Request failed with status code 422"})
    );
}

#[tokio::test]
async fn change_plan_ke_free_dan_dari_free() {
    let Some(env) = setup().await else { return };
    let app = &env.app;
    let free = tier(app, &format!("Free-{}", &uniq()[..6]), 0.0, None, "MONTH").await;
    let paid = tier(
        app,
        &format!("Paid-{}", &uniq()[..6]),
        5.0,
        Some("P-PAID"),
        "MONTH",
    )
    .await;

    // Berbayar → Free: PayPal aktif & pending dibatalkan, langganan Free ACTIVE.
    let fx = company(
        app,
        false,
        r#""pricePerUser" = 5, "paypalSubscriptionId" = 'I-A', "pendingPaypalSubscriptionId" = 'I-P'"#,
    )
    .await;
    env.mock
        .on("POST", "/v1/billing/subscriptions/*", 204, Value::Null);
    let r = post(
        &env,
        "/invoices/change-plan",
        &fx.c.admin.token,
        json!({"newPricingUuid": free.uuid}),
    )
    .send()
    .await;
    r.assert_status(201).assert_ok();
    assert_eq!(r.json["message"], "Free plan activated successfully");
    assert_eq!(
        env.mock
            .requests_to("POST", "/v1/billing/subscriptions/I-A/cancel")
            .len(),
        1
    );
    assert_eq!(
        env.mock
            .requests_to("POST", "/v1/billing/subscriptions/I-P/cancel")
            .len(),
        1
    );
    let s = sub_row(app, fx.sub_id).await;
    assert_eq!(s["pricingTierId"], free.id);
    assert_eq!(s["pricePerUser"].as_f64(), Some(0.0));
    assert_eq!(s["paypalSubscriptionId"], Value::Null);
    // Tier sama → pesan.
    let r = post(
        &env,
        "/invoices/change-plan",
        &fx.c.admin.token,
        json!({"newPricingUuid": free.uuid}),
    )
    .send()
    .await;
    assert_eq!(
        r.json["message"],
        "New plan is the same as the current plan"
    );

    // Free → berbayar lewat change-plan dialihkan ke changeActivePlan (jalur pending).
    env.mock.on(
        "POST",
        "/v1/billing/subscriptions",
        201,
        paypal_created("I-UP"),
    );
    let r = post(
        &env,
        "/invoices/change-plan",
        &fx.c.admin.token,
        json!({"newPricingUuid": paid.uuid}),
    )
    .send()
    .await;
    r.assert_status(201).assert_ok();
    assert_eq!(
        r.json["message"],
        "Upgrade initiated. Please complete payment to activate the new plan."
    );
    let s = sub_row(app, fx.sub_id).await;
    assert_eq!(
        s["pricingTierId"], free.id,
        "tier baru hanya ditulis webhook"
    );
    assert_eq!(s["pendingPricingTierId"], paid.id);
    assert_eq!(s["pendingPaypalSubscriptionId"], "I-UP");
}

#[tokio::test]
async fn change_active_plan_upgrade_dan_downgrade() {
    let Some(env) = setup().await else { return };
    let app = &env.app;
    let starter = tier(app, "Starter", 4.0, Some("P-STARTER"), "MONTH").await;
    let grow = tier(app, "Grow", 8.0, Some("P-GROW"), "MONTH").await;

    // Upgrade Starter → Grow.
    let fx = company(
        app,
        false,
        &format!(
            r#""pricingTierId" = {}, "pricePerUser" = 4, "paypalSubscriptionId" = 'I-CUR',
               "pendingPaypalSubscriptionId" = 'I-STALE', "nextInvoiceDate" = NOW() + interval '5 day'"#,
            starter.id
        ),
    )
    .await;
    env.mock
        .on("POST", "/v1/billing/subscriptions/*", 204, Value::Null);
    env.mock.on(
        "POST",
        "/v1/billing/subscriptions",
        201,
        paypal_created("I-GROW"),
    );
    let r = post(
        &env,
        "/invoices/change-active-plan",
        &fx.c.admin.token,
        json!({"newPricingUuid": grow.uuid}),
    )
    .send()
    .await;
    r.assert_status(201).assert_ok();
    let d = r.data();
    assert_eq!(d["approvalUrl"], "https://paypal.test/approve/I-GROW");
    assert_eq!(d["currentPlan"], "Starter");
    assert_eq!(d["newPlan"], "Grow");
    assert_eq!(d["totalPrice"], json!(24));
    let stale = env
        .mock
        .requests_to("POST", "/v1/billing/subscriptions/I-STALE/cancel");
    assert_eq!(
        stale[0].json()["reason"],
        "Replaced by new plan change request"
    );
    assert!(
        env.mock
            .requests_to("POST", "/v1/billing/subscriptions/I-CUR/cancel")
            .is_empty()
    );
    let s = sub_row(app, fx.sub_id).await;
    assert_eq!(s["pricingTierId"], starter.id);
    assert_eq!(s["pendingPricingTierId"], grow.id);
    assert_eq!(s["pendingPaypalPlanId"], "P-GROW");
    assert_eq!(s["nextInvoiceDate"], Value::Null);
    let number = d["invoiceNumber"].as_str().unwrap();
    let sub = fx.sub_id;
    let inv = row(
        app,
        &format!(
            r#"SELECT i.status::text, i."totalPrice", it.description FROM invoices i
                 JOIN "invoiceItems" it ON it."invoiceId" = i.id WHERE i."invoiceNumber" = '{number}' AND i."subscriptionId" = {sub}"#
        ),
    )
    .await;
    assert_eq!(inv["status"], "UNPAID");
    assert_eq!(
        inv["description"],
        "Monthly Subscription (upgrade to Grow) - 3 user(s) @ $8/user"
    );

    // Downgrade Grow → Starter: PayPal lama dibatalkan, yang baru mulai di endDate.
    let fx = company(
        app,
        false,
        &format!(
            r#""pricingTierId" = {}, "pricePerUser" = 8, "paypalSubscriptionId" = 'I-G',
               "endDate" = '2027-03-04 05:06:07.089'"#,
            grow.id
        ),
    )
    .await;
    let (unpaid, _, _) = invoice(app, fx.sub_id, "UNPAID", "SUBSCRIPTION").await;
    env.mock.clear_log();
    env.mock.on(
        "POST",
        "/v1/billing/subscriptions",
        201,
        paypal_created("I-DOWN"),
    );
    let r = post(
        &env,
        "/invoices/change-active-plan",
        &fx.c.admin.token,
        json!({"newPricingUuid": starter.uuid}),
    )
    .send()
    .await;
    r.assert_status(201).assert_ok();
    assert_eq!(
        r.json["message"],
        "Downgrade scheduled. Current plan stays active until 04-03-2027, then switches to Starter."
    );
    assert_eq!(r.data()["effectiveDate"], "2027-03-04T05:06:07.089Z");
    let cancel = env
        .mock
        .requests_to("POST", "/v1/billing/subscriptions/I-G/cancel");
    assert_eq!(
        cancel[0].json()["reason"],
        "Downgrade — old plan cancelled, new plan scheduled at cycle end"
    );
    let created = env.mock.requests_to("POST", "/v1/billing/subscriptions");
    assert_eq!(created[0].json()["start_time"], "2027-03-04T05:06:07.089Z");
    let s = sub_row(app, fx.sub_id).await;
    assert_eq!(s["paypalStatus"], "CANCELLED");
    assert_eq!(s["pendingPricingTierId"], starter.id);
    assert_eq!(invoice_status(app, unpaid).await, "CANCELLED");

    // Langganan tidak ACTIVE (dan bukan Free) → ditolak.
    let fx = company(app, false, r#"status = 'PENDING', "pricePerUser" = 4"#).await;
    let r = post(
        &env,
        "/invoices/change-active-plan",
        &fx.c.admin.token,
        json!({"newPricingUuid": grow.uuid}),
    )
    .send()
    .await;
    assert_eq!(
        r.json,
        json!({"success": false, "message": "No active subscription found"})
    );
}

#[tokio::test]
async fn manual_billing_409() {
    let Some(env) = setup().await else { return };
    let app = &env.app;
    let fx = company(
        app,
        false,
        r#""billingSource" = 'MANUAL', status = 'ACTIVE', "endDate" = NOW() + interval '30 day'"#,
    )
    .await;
    let (_, inv_uuid, _) = invoice(app, fx.sub_id, "UNPAID", "SUBSCRIPTION").await;
    let some = uuid::Uuid::new_v4().to_string();
    let token = &fx.c.admin.token;
    for (m, uri, body) in [
        (
            "POST",
            "/invoices/change-plan".to_owned(),
            json!({"newPricingUuid": some}),
        ),
        (
            "POST",
            "/invoices/change-active-plan".into(),
            json!({"newPricingUuid": some}),
        ),
        (
            "POST",
            "/invoices/subscribe".into(),
            json!({"newPricingUuid": some}),
        ),
        ("POST", "/invoices/cancel-subscription".into(), json!({})),
        ("POST", "/invoices/resume-subscription".into(), json!({})),
        (
            "POST",
            "/invoices/reactivate-subscription".into(),
            json!({}),
        ),
        ("PUT", format!("/invoices/renew/{inv_uuid}"), json!({})),
    ] {
        let r = app
            .request(m.parse().unwrap(), &uri)
            .token(token)
            .json(&body)
            .send()
            .await;
        r.assert_status(409);
        assert_eq!(r.json["code"], "MANUAL_BILLING", "{uri}");
    }
    assert!(
        env.mock.requests().is_empty(),
        "PayPal tidak boleh dipanggil"
    );
}

// ------------------------------------------------------------------ cancel / resume / reactivate

#[tokio::test]
async fn cancel_normal_dan_trial() {
    let Some(env) = setup().await else { return };
    let app = &env.app;
    env.mock
        .on("POST", "/v1/billing/subscriptions/*", 204, Value::Null);
    env.mock.on(
        "POST",
        "/v1/billing/subscriptions/I-N/capture",
        201,
        json!({"id": "CAP-1"}),
    );

    let fx = company(
        app,
        false,
        r#""paypalSubscriptionId" = 'I-N', "autoRenew" = true, "nextInvoiceDate" = NOW()"#,
    )
    .await;
    let (sub_inv, _, sub_no) = invoice(app, fx.sub_id, "UNPAID", "SUBSCRIPTION").await;
    let (pro_inv, _, _) = invoice(app, fx.sub_id, "UNPAID", "PRORATION").await;
    let r = post(
        &env,
        "/invoices/cancel-subscription",
        &fx.c.admin.token,
        json!({}),
    )
    .send()
    .await;
    r.assert_status(201).assert_ok();
    assert_eq!(r.data()["autoRenew"], false);
    let cap = env
        .mock
        .requests_to("POST", "/v1/billing/subscriptions/I-N/capture");
    assert_eq!(cap.len(), 1);
    assert_eq!(
        cap[0].json()["note"],
        format!("Outstanding payment: {sub_no}")
    );
    assert_eq!(cap[0].json()["amount"]["value"], "10.50");
    assert_eq!(invoice_status(app, sub_inv).await, "PAID");
    assert_eq!(invoice_status(app, pro_inv).await, "UNPAID");
    let susp = env
        .mock
        .requests_to("POST", "/v1/billing/subscriptions/I-N/suspend");
    assert_eq!(susp[0].json()["reason"], "Auto-renewal disabled by user");
    let s = sub_row(app, fx.sub_id).await;
    assert_eq!(s["autoRenew"], false);
    assert_eq!(s["nextInvoiceDate"], Value::Null);
    assert_eq!(s["status"], "ACTIVE");
    let r = post(
        &env,
        "/invoices/cancel-subscription",
        &fx.c.admin.token,
        json!({}),
    )
    .send()
    .await;
    assert_eq!(
        r.json["message"],
        "Subscription is already set to not renew"
    );

    // Trial: batal keras, kembali TRIAL.
    let fx = company(
        app,
        true,
        r#""paypalSubscriptionId" = 'I-T', "paypalPlanId" = 'P-T'"#,
    )
    .await;
    let (inv, _, _) = invoice(app, fx.sub_id, "UNPAID", "SUBSCRIPTION").await;
    let r = post(
        &env,
        "/invoices/cancel-subscription",
        &fx.c.admin.token,
        json!({}),
    )
    .send()
    .await;
    r.assert_status(201).assert_ok();
    let cancel = env
        .mock
        .requests_to("POST", "/v1/billing/subscriptions/I-T/cancel");
    assert_eq!(
        cancel[0].json()["reason"],
        "Trial subscription cancelled by user before billing started"
    );
    let s = sub_row(app, fx.sub_id).await;
    assert_eq!(s["status"], "TRIAL");
    assert_eq!(s["paypalSubscriptionId"], Value::Null);
    assert_eq!(s["paypalPlanId"], Value::Null);
    assert_eq!(invoice_status(app, inv).await, "CANCELLED");
}

#[tokio::test]
async fn resume_dan_reactivate() {
    let Some(env) = setup().await else { return };
    let app = &env.app;
    env.mock
        .on("POST", "/v1/billing/subscriptions/*", 204, Value::Null);

    // Resume.
    let fx = company(
        app,
        false,
        r#""paypalSubscriptionId" = 'I-R', "autoRenew" = false, "endDate" = NOW() + interval '10 day'"#,
    )
    .await;
    let r = post(
        &env,
        "/invoices/resume-subscription",
        &fx.c.admin.token,
        json!({}),
    )
    .send()
    .await;
    r.assert_status(201).assert_ok();
    assert_eq!(
        r.json["message"],
        "Subscription resumed. Auto-renewal has been re-enabled."
    );
    let act = env
        .mock
        .requests_to("POST", "/v1/billing/subscriptions/I-R/activate");
    assert_eq!(act[0].json()["reason"], "Auto-renewal re-enabled by user");
    let s = sub_row(app, fx.sub_id).await;
    assert_eq!(s["autoRenew"], true);
    assert_eq!(s["paypalStatus"], "ACTIVE");
    assert_eq!(s["next_gap"], 3);
    let r = post(
        &env,
        "/invoices/resume-subscription",
        &fx.c.admin.token,
        json!({}),
    )
    .send()
    .await;
    assert_eq!(
        r.json["message"],
        "Subscription is already set to auto-renew"
    );
    let fx = company(
        app,
        false,
        r#""autoRenew" = false, "endDate" = NOW() - interval '1 day'"#,
    )
    .await;
    let r = post(
        &env,
        "/invoices/resume-subscription",
        &fx.c.admin.token,
        json!({}),
    )
    .send()
    .await;
    assert_eq!(
        r.json["message"],
        "Subscription period has already ended. Please subscribe again."
    );

    // Reactivate: status PayPal SUSPENDED / CANCELLED / ACTIVE.
    let fx = company(
        app,
        false,
        r#"status = 'SUSPENDED', "paypalSubscriptionId" = 'I-S', "autoRenew" = false"#,
    )
    .await;
    env.mock.on(
        "GET",
        "/v1/billing/subscriptions/I-S",
        200,
        json!({"id": "I-S", "status": "SUSPENDED"}),
    );
    let r = post(
        &env,
        "/invoices/reactivate-subscription",
        &fx.c.admin.token,
        json!({}),
    )
    .send()
    .await;
    r.assert_status(201).assert_ok();
    assert_eq!(r.json["message"], "Subscription reactivated successfully.");
    let s = sub_row(app, fx.sub_id).await;
    assert_eq!(s["status"], "ACTIVE");
    assert_eq!(s["autoRenew"], true);
    let r = post(
        &env,
        "/invoices/reactivate-subscription",
        &fx.c.admin.token,
        json!({}),
    )
    .send()
    .await;
    assert_eq!(r.json["message"], "Subscription is not suspended");

    for (status, msg) in [
        (
            "CANCELLED",
            "PayPal subscription has been fully cancelled and cannot be reactivated. Please create a new subscription.",
        ),
        ("ACTIVE", "Cannot reactivate subscription in status: ACTIVE"),
    ] {
        let id = format!("I-{status}");
        let fx = company(
            app,
            false,
            &format!(r#"status = 'SUSPENDED', "paypalSubscriptionId" = '{id}'"#),
        )
        .await;
        env.mock.on(
            "GET",
            &format!("/v1/billing/subscriptions/{id}"),
            200,
            json!({"id": id, "status": status}),
        );
        let r = post(
            &env,
            "/invoices/reactivate-subscription",
            &fx.c.admin.token,
            json!({}),
        )
        .send()
        .await;
        r.assert_status(201);
        assert_eq!(r.json, json!({"success": false, "message": msg}));
    }
}

// ------------------------------------------------------------------ subscribe

#[tokio::test]
async fn subscribe_dari_trial() {
    let Some(env) = setup().await else { return };
    let app = &env.app;
    let paid = tier(
        app,
        &format!("Sub-{}", &uniq()[..6]),
        6.0,
        Some("P-SUB"),
        "YEAR",
    )
    .await;
    let fx = company(
        app,
        true,
        r#"status = 'TRIAL', quantity = 0, "endDate" = '2027-01-02 03:04:05.006'"#,
    )
    .await;
    env.mock.on(
        "POST",
        "/v1/billing/subscriptions",
        201,
        paypal_created("I-SUB"),
    );
    let r = post(
        &env,
        "/invoices/subscribe",
        &fx.c.admin.token,
        json!({"newPricingUuid": paid.uuid}),
    )
    .send()
    .await;
    r.assert_status(201).assert_ok();
    // quantity 0 → 1; basePrice (12) × 12.
    assert_eq!(
        r.json,
        json!({
            "success": true,
            "message": "Subscription created, please approve payment",
            "data": {
                "paymentUrl": "https://paypal.test/approve/I-SUB",
                "paypalSubscriptionId": "I-SUB",
                "totalPrice": 144,
            },
        })
    );
    let body = env.mock.requests_to("POST", "/v1/billing/subscriptions")[0].json();
    assert_eq!(body["start_time"], "2027-01-02T03:04:05.006Z");
    assert_eq!(body["quantity"], "1");
    assert_eq!(
        body["application_context"]["return_url"],
        "https://fe.test/return"
    );
    let s = sub_row(app, fx.sub_id).await;
    assert_eq!(s["status"], "PENDING");
    assert_eq!(s["pricingTierId"], paid.id);
    assert_eq!(s["paypalPlanId"], "P-SUB");

    // Bukan trial.
    let fx = company(app, false, "").await;
    let r = post(
        &env,
        "/invoices/subscribe",
        &fx.c.admin.token,
        json!({"newPricingUuid": paid.uuid}),
    )
    .send()
    .await;
    assert_eq!(
        r.json["message"],
        "Only trial companies can subscribe through this API"
    );

    // Tier gratis → aktivasi Free.
    let free = tier(app, &format!("Free-{}", &uniq()[..6]), 0.0, None, "MONTH").await;
    let fx = company(app, true, r#"status = 'TRIAL'"#).await;
    let r = post(
        &env,
        "/invoices/subscribe",
        &fx.c.admin.token,
        json!({"newPricingUuid": free.uuid}),
    )
    .send()
    .await;
    r.assert_status(201).assert_ok();
    assert_eq!(
        r.data()["newPlan"],
        json!(
            app.scalar::<String>(&format!(
                r#"SELECT name FROM "pricingTiers" WHERE id = {}"#,
                free.id
            ))
            .await
        )
    );
    let is_trial: bool = app
        .scalar(&format!(
            r#"SELECT "isTrial" FROM company WHERE id = {}"#,
            fx.c.id
        ))
        .await;
    assert!(!is_trial);
}

// ------------------------------------------------------------------ renew

#[tokio::test]
async fn renew_paypal() {
    let Some(env) = setup().await else { return };
    let app = &env.app;
    env.mock
        .on("POST", "/v1/billing/subscriptions/*", 204, Value::Null);

    // SUSPENDED → aktifkan, invoice PAID.
    let fx = company(
        app,
        false,
        r#""paypalSubscriptionId" = 'I-SUS', "autoRenew" = false"#,
    )
    .await;
    let (inv, inv_uuid, _) = invoice(app, fx.sub_id, "UNPAID", "SUBSCRIPTION").await;
    env.mock.on(
        "GET",
        "/v1/billing/subscriptions/I-SUS",
        200,
        json!({"id": "I-SUS", "status": "SUSPENDED"}),
    );
    let r = app
        .put(&format!("/invoices/renew/{inv_uuid}"))
        .token(&fx.c.admin.token)
        .send()
        .await;
    r.assert_status(200).assert_ok();
    assert_eq!(
        r.json,
        json!({
            "success": true,
            "message": "Subscription reactivated successfully",
            "data": {"paypalSubscriptionId": "I-SUS", "status": "ACTIVE"},
        })
    );
    assert_eq!(invoice_status(app, inv).await, "PAID");
    assert_eq!(sub_row(app, fx.sub_id).await["autoRenew"], true);

    // APPROVAL_PENDING → pakai ulang tautan.
    let fx = company(app, false, r#""paypalSubscriptionId" = 'I-AP'"#).await;
    let (inv, inv_uuid, _) = invoice(app, fx.sub_id, "UNPAID", "SUBSCRIPTION").await;
    env.mock.on(
        "GET",
        "/v1/billing/subscriptions/I-AP",
        200,
        paypal_created("I-AP"),
    );
    let r = app
        .put(&format!("/invoices/renew/{inv_uuid}"))
        .token(&fx.c.admin.token)
        .send()
        .await;
    assert_eq!(r.data()["status"], "APPROVAL_PENDING");
    let url: String = app
        .scalar(&format!(
            r#"SELECT "paymentUrl" FROM invoices WHERE id = {inv}"#
        ))
        .await;
    assert_eq!(url, "https://paypal.test/approve/I-AP");

    // ACTIVE → "already active" (status dari PayPal diteruskan).
    let fx = company(app, false, r#""paypalSubscriptionId" = 'I-ACT'"#).await;
    let (_, inv_uuid, _) = invoice(app, fx.sub_id, "UNPAID", "SUBSCRIPTION").await;
    env.mock.on(
        "GET",
        "/v1/billing/subscriptions/I-ACT",
        200,
        json!({"id": "I-ACT", "status": "ACTIVE"}),
    );
    let r = app
        .put(&format!("/invoices/renew/{inv_uuid}"))
        .token(&fx.c.admin.token)
        .send()
        .await;
    assert_eq!(r.json["message"], "Subscription is already active");
    assert_eq!(r.data()["status"], "ACTIVE");

    // CANCELLED dan 404 → langganan PayPal baru, langganan PENDING.
    for (id, status, body) in [
        ("I-CAN", 200, json!({"id": "I-CAN", "status": "CANCELLED"})),
        ("I-404", 404, json!({"name": "RESOURCE_NOT_FOUND"})),
    ] {
        let fx = company(
            app,
            false,
            &format!(r#""paypalSubscriptionId" = '{id}', "paypalPlanId" = 'P-OLD'"#),
        )
        .await;
        let (inv, inv_uuid, inv_no) = invoice(app, fx.sub_id, "EXPIRED", "SUBSCRIPTION").await;
        let new_id = format!("{id}-NEW");
        env.mock.on(
            "GET",
            &format!("/v1/billing/subscriptions/{id}"),
            status,
            body,
        );
        env.mock.on(
            "POST",
            "/v1/billing/subscriptions",
            201,
            paypal_created(&new_id),
        );
        env.mock.clear_log();
        let r = app
            .put(&format!("/invoices/renew/{inv_uuid}"))
            .token(&fx.c.admin.token)
            .send()
            .await;
        r.assert_status(200).assert_ok();
        assert_eq!(
            r.json,
            json!({
                "success": true,
                "message": "New subscription created, please approve payment",
                "data": {
                    "paymentUrl": format!("https://paypal.test/approve/{new_id}"),
                    "paypalSubscriptionId": new_id,
                },
            })
        );
        let body = env.mock.requests_to("POST", "/v1/billing/subscriptions")[0].json();
        assert_eq!(body["plan_id"], "P-OLD");
        assert_eq!(
            body["application_context"]["cancel_url"],
            format!("https://fe.test/cancel?invoice={inv_no}")
        );
        let s = sub_row(app, fx.sub_id).await;
        assert_eq!(s["status"], "PENDING");
        assert_eq!(s["paypalSubscriptionId"], new_id.as_str());
        assert_eq!(invoice_status(app, inv).await, "UNPAID");
    }

    // Galat PayPal lain → pesan tetap.
    let fx = company(app, false, r#""paypalSubscriptionId" = 'I-500'"#).await;
    let (_, inv_uuid, _) = invoice(app, fx.sub_id, "UNPAID", "SUBSCRIPTION").await;
    env.mock
        .on("GET", "/v1/billing/subscriptions/I-500", 500, json!({}));
    let r = app
        .put(&format!("/invoices/renew/{inv_uuid}"))
        .token(&fx.c.admin.token)
        .send()
        .await;
    assert_eq!(
        r.json,
        json!({"success": false, "message": "Failed to process PayPal subscription"})
    );

    // Invoice company lain = tidak ada.
    let other = app.other_company_admin().await;
    let r = app
        .put(&format!("/invoices/renew/{inv_uuid}"))
        .token(&other.admin.token)
        .send()
        .await;
    assert_eq!(
        r.json,
        json!({"success": false, "message": "Invoice not found", "data": null})
    );
}

#[tokio::test]
async fn renew_midtrans_lama() {
    let Some(env) = setup().await else { return };
    let app = &env.app;
    let fx = company(
        app,
        false,
        r#""paypalSubscriptionId" = NULL, "startDate" = '2026-10-01', "endDate" = '2026-11-01'"#,
    )
    .await;
    let (inv, inv_uuid, inv_no) = invoice(app, fx.sub_id, "EXPIRED", "SUBSCRIPTION").await;
    env.mock.on(
        "GET",
        &format!("/v1/payment-links/{inv_no}"),
        404,
        json!({}),
    );
    env.mock.on(
        "POST",
        "/v1/payment-links",
        200,
        json!({"order_id": inv_no, "payment_url": "https://midtrans.test/pay/1"}),
    );
    let r = app
        .put(&format!("/invoices/renew/{inv_uuid}"))
        .token(&fx.c.admin.token)
        .send()
        .await;
    r.assert_status(200).assert_ok();
    assert_eq!(
        r.json,
        json!({"success": true, "message": "Success renew payment", "data": {"paymentUrl": "https://midtrans.test/pay/1"}})
    );
    assert!(
        env.mock
            .requests_to("DELETE", &format!("/v1/payment-links/{inv_no}"))
            .is_empty()
    );
    let post_req = &env.mock.requests_to("POST", "/v1/payment-links")[0];
    assert_eq!(post_req.header("authorization"), Some("Basic mkey"));
    let body = post_req.json();
    assert_eq!(body["transaction_details"]["order_id"], inv_no.as_str());
    assert_eq!(body["transaction_details"]["gross_amount"], json!(10.5));
    assert!(
        body["transaction_details"]["payment_link_id"]
            .as_str()
            .unwrap()
            .starts_with(&format!("{inv_no}-renew-"))
    );
    assert_eq!(body["expiry"]["duration"], 3);
    assert!(
        body["expiry"]["start_time"]
            .as_str()
            .unwrap()
            .ends_with(" +0700")
    );
    assert_eq!(
        body["callbacks"]["finish"],
        format!("https://fe.test/finish/{inv_no}")
    );
    assert!(body.get("webhook_key").is_none());
    let i = row(
        app,
        &format!(
            r#"SELECT status::text, "paymentUrl", (EXTRACT(EPOCH FROM ("expiredAt" - NOW())) / 86400)::int AS d
                 FROM invoices WHERE id = {inv}"#
        ),
    )
    .await;
    assert_eq!(i["status"], "UNPAID");
    assert_eq!(i["paymentUrl"], "https://midtrans.test/pay/1");
    assert_eq!(i["d"], 3);

    // Midtrans menolak → pesan axios.
    env.mock.on(
        "GET",
        &format!("/v1/payment-links/{inv_no}"),
        200,
        json!({}),
    );
    env.mock.on(
        "DELETE",
        &format!("/v1/payment-links/{inv_no}"),
        200,
        json!({}),
    );
    env.mock.on(
        "POST",
        "/v1/payment-links",
        400,
        json!({"error_messages": ["x"]}),
    );
    let r = app
        .put(&format!("/invoices/renew/{inv_uuid}"))
        .token(&fx.c.admin.token)
        .send()
        .await;
    assert_eq!(
        r.json,
        json!({"success": false, "message": "Request failed with status code 400"})
    );
    assert_eq!(
        env.mock
            .requests_to("DELETE", &format!("/v1/payment-links/{inv_no}"))
            .len(),
        1
    );
}
