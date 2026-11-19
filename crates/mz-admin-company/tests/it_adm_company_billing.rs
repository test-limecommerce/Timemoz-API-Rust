//! Uji integrasi langganan manual + `PATCH set/subscriptions` + boost TIME MANUAL, dengan
//! PayPal TIRUAN (`MZ_PAYPAL_API_BASE` → `mz_int_common::mock::MockServer`). Tidak ada
//! panggilan ke PayPal sungguhan.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use chrono::{Duration, Utc};
use common::{build, company, member, op_audit, operator, uniq};
use mz_int_common::mock::MockServer;
use mz_testkit::TestApp;
use serde_json::{Value, json};
use tokio::sync::Mutex;

static SERIAL: Mutex<()> = Mutex::const_new(());

const PAID_TIER: i32 = 1;
const FREE_TIME_TIER: i32 = 6;

fn iso(days: i64) -> String {
    (Utc::now() + Duration::days(days)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

async fn app_with_paypal() -> Option<(TestApp, MockServer)> {
    let mock = MockServer::start().await;
    mock.on(
        "POST",
        "/v1/oauth2/token",
        200,
        json!({ "access_token": "tok", "expires_in": 3600 }),
    );
    let base = mock.url();
    let app = TestApp::with_vars(build, &[("MZ_PAYPAL_API_BASE", &base)]).await?;
    Some((app, mock))
}

async fn tier_uuid(app: &TestApp, id: i32) -> String {
    app.scalar(&format!(
        r#"SELECT uuid::text FROM "pricingTiers" WHERE id = {id}"#
    ))
    .await
}

#[tokio::test]
async fn set_subscription_syncs_paypal() {
    let _g = SERIAL.lock().await;
    let Some((app, mock)) = app_with_paypal().await else {
        return;
    };
    let (op, token) = operator(&app, "OWNER").await;
    let fx = company(&app, "ACTIVE", "PAYPAL", PAID_TIER).await;
    let pid = format!("I-{}", &uniq()[..10]);
    app.exec(&format!(
        r#"UPDATE subscriptions SET "paypalSubscriptionId" = '{pid}', "paypalStatus" = 'ACTIVE' WHERE "companyId" = {}"#,
        fx.id
    ))
    .await;
    mock.on(
        "GET",
        "/v1/billing/subscriptions/*",
        200,
        json!({ "status": "ACTIVE" }),
    );
    mock.on("POST", "/v1/billing/subscriptions/*", 204, Value::Null);
    let path = format!("/admin-company/set/subscriptions/{}", fx.uuid);

    let r = app
        .patch(&path)
        .token(&token)
        .json(&json!({ "status": "SUSPENDED", "endAt": iso(30) }))
        .send()
        .await;
    r.assert_ok().assert_no_sql_error();
    assert_eq!(
        r.json["data"],
        json!({ "pricingChanged": false, "pricingTier": null })
    );
    let calls = mock.requests_to("POST", &format!("/v1/billing/subscriptions/{pid}/suspend"));
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].json(), json!({ "reason": "Suspended by admin" }));
    assert_eq!(calls[0].header("authorization"), Some("Bearer tok"));
    let (st, pst, auto): (String, Option<String>, bool) = sqlx::query_as(
        r#"SELECT status::text, "paypalStatus", "autoRenew" FROM subscriptions WHERE "companyId" = $1"#,
    )
    .bind(fx.id)
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!(
        (st.as_str(), pst.as_deref(), auto),
        ("SUSPENDED", Some("SUSPENDED"), false)
    );

    // Ganti tier ke Free: limit company disalin dari tier.
    let free = tier_uuid(&app, FREE_TIME_TIER).await;
    let r = app
        .patch(&path)
        .token(&token)
        .json(&json!({ "status": "ACTIVE", "endAt": iso(40), "pricingUuid": free }))
        .send()
        .await;
    r.assert_ok();
    assert_eq!(r.json["data"]["pricingChanged"], true);
    let (max_users, tier_id): (i32, i32) = sqlx::query_as(
        r#"SELECT c."maxUsers", s."pricingTierId" FROM company c JOIN subscriptions s ON s."companyId" = c.id
           WHERE c.id = $1"#,
    )
    .bind(fx.id)
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!((max_users, tier_id), (2, FREE_TIME_TIER));

    // PayPal gagal → success:false dengan pesan axios, tanpa tulisan.
    mock.on(
        "GET",
        "/v1/billing/subscriptions/*",
        500,
        json!({ "name": "INTERNAL_SERVER_ERROR" }),
    );
    let r = app
        .patch(&path)
        .token(&token)
        .json(&json!({ "status": "CANCELLED", "endAt": iso(30) }))
        .send()
        .await;
    assert_eq!(
        r.json,
        json!({ "success": false, "message": "Request failed with status code 500", "data": null })
    );
    // Validasi pipe global (tanpa whitelist).
    let r = app
        .patch(&path)
        .token(&token)
        .json(&json!({ "status": "TRIAL", "endAt": "", "foo": 1 }))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        json!([
            "endAt should not be empty",
            "endAt must be a valid ISO 8601 date string",
            "status must be one of the following values: ACTIVE, INACTIVE, CANCELLED, PENDING, SUSPENDED"
        ])
    );
    let actions: Vec<String> = op_audit(&app, op)
        .await
        .iter()
        .map(|r| r[0].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        actions,
        vec!["company.subscription_set", "company.subscription_set"]
    );
}

#[tokio::test]
async fn manual_subscription_lifecycle() {
    let _g = SERIAL.lock().await;
    let Some((app, mock)) = app_with_paypal().await else {
        return;
    };
    let (op, finance) = operator(&app, "FINANCE").await;
    let (_, support) = operator(&app, "SUPPORT").await;
    let fx = company(&app, "ACTIVE", "PAYPAL", PAID_TIER).await;
    let pid = format!("I-{}", &uniq()[..10]);
    app.exec(&format!(
        r#"UPDATE subscriptions SET "paypalSubscriptionId" = '{pid}', "paypalStatus" = 'ACTIVE' WHERE "companyId" = {}"#,
        fx.id
    ))
    .await;
    mock.on(
        "GET",
        "/v1/billing/subscriptions/*",
        200,
        json!({ "status": "ACTIVE" }),
    );
    mock.on("POST", "/v1/billing/subscriptions/*", 204, Value::Null);
    let base = format!("/admin-company/{}/subscription", fx.uuid);
    let tier = tier_uuid(&app, PAID_TIER).await;
    let u = uniq();
    let reference = format!("TRF-{}", &u[..8]);

    // SUPPORT boleh membaca ringkasan.
    let r = app.get(&base).token(&support).send().await;
    r.assert_ok().assert_no_sql_error();
    assert_eq!(r.data()["billingSource"], "PAYPAL");
    assert_eq!(r.data()["limits"].as_array().unwrap().len(), 19);

    // Kursi di bawah anggota aktif.
    member(&app, fx.id, "STAFF").await;
    let activate = json!({
        "pricingUuid": tier, "startDate": iso(0), "endDate": iso(30), "maxUsers": 1,
        "payment": { "amount": 500000, "method": "BANK_TRANSFER", "reference": reference, "paidAt": iso(-1) },
    });
    let r = app
        .post(&format!("{base}/manual/activate"))
        .token(&finance)
        .json(&activate)
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(r.json["code"], "MAX_USERS_BELOW_MEMBERS");
    assert_eq!(r.json["members"], 2);
    assert!(
        mock.requests_to("POST", &format!("/v1/billing/subscriptions/{pid}/cancel"))
            .is_empty()
    );

    let mut ok_body = activate.clone();
    ok_body["maxUsers"] = json!(5);
    let r = app
        .post(&format!("{base}/manual/activate"))
        .token(&finance)
        .json(&ok_body)
        .send()
        .await;
    r.assert_status(201).assert_no_sql_error();
    let d = r.data();
    assert_eq!(d["billingSource"], "MANUAL");
    assert_eq!(d["status"], "ACTIVE");
    assert_eq!(d["maxUsers"], 5);
    assert_eq!(d["paypal"]["status"], "CANCELLED");
    assert_eq!(d["invoices"][0]["status"], "PAID");
    assert_eq!(d["invoices"][0]["totalPrice"], 500000);
    assert_eq!(d["invoices"][0]["dueDate"], Value::Null);
    assert_eq!(d["lock"]["graceDays"], 7);
    let cancel = mock.requests_to("POST", &format!("/v1/billing/subscriptions/{pid}/cancel"));
    assert_eq!(cancel.len(), 1);
    assert_eq!(
        cancel[0].json(),
        json!({ "reason": "Moved to manual billing" })
    );
    let (trial, max_users): (bool, i32) =
        sqlx::query_as(r#"SELECT "isTrial", "maxUsers" FROM company WHERE id = $1"#)
            .bind(fx.id)
            .fetch_one(app.db())
            .await
            .unwrap();
    assert_eq!((trial, max_users), (false, 5));

    let r = app
        .post(&format!("{base}/manual/activate"))
        .token(&finance)
        .json(&ok_body)
        .send()
        .await;
    assert_eq!(r.json["code"], "ALREADY_MANUAL_ACTIVE");
    let r = app
        .patch(&format!("/admin-company/set/subscriptions/{}", fx.uuid))
        .token(&finance)
        .json(&json!({ "status": "ACTIVE", "endAt": iso(30) }))
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(r.json["code"], "MANUAL_BILLING_USE_NEW_ENDPOINT");

    // extend
    let r = app
        .post(&format!("{base}/manual/extend"))
        .token(&finance)
        .json(&json!({ "endDate": iso(10), "payment": null, "reason": "gratis sebulan" }))
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(r.json["code"], "END_DATE_NOT_LATER");
    let r = app
        .post(&format!("{base}/manual/extend"))
        .token(&finance)
        .json(&json!({ "endDate": iso(60), "payment": null }))
        .send()
        .await;
    assert_eq!(
        r.json["message"],
        json!(["reason must be 5-500 characters and is required when payment is null"])
    );
    let r = app
        .post(&format!("{base}/manual/extend"))
        .token(&finance)
        .json(&json!({ "endDate": iso(60), "payment": {
            "amount": 1, "method": "CASH", "reference": reference.to_lowercase(), "paidAt": iso(-1) } }))
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(r.json["code"], "DUPLICATE_PAYMENT_REFERENCE");
    let r = app
        .post(&format!("{base}/manual/extend"))
        .token(&finance)
        .json(&json!({ "endDate": iso(60), "payment": null, "maxUsers": 6, "reason": "gratis sebulan" }))
        .send()
        .await;
    r.assert_status(201);
    assert_eq!(r.data()["maxUsers"], 6);

    // contract + invoice
    let r = app
        .post(&format!("{base}/manual/invoices"))
        .token(&finance)
        .json(&json!({}))
        .send()
        .await;
    assert_eq!(r.json["code"], "CONTRACT_NOT_SET");
    let r = app
        .put(&format!("{base}/manual/contract"))
        .token(&finance)
        .json(&json!({ "pricePerUser": 30000, "periodMonths": 3, "taxPercent": 11 }))
        .send()
        .await;
    r.assert_status(200);
    let c = &r.data()["contract"];
    assert_eq!(c["currency"], "IDR");
    assert_eq!(c["seats"], 6);
    assert_eq!(
        c["perPeriod"],
        json!({ "subtotal": 540000, "tax": 59400, "total": 599400 })
    );
    let r = app
        .post(&format!("{base}/manual/invoices"))
        .token(&finance)
        .json(&json!({ "note": "  Q1  " }))
        .send()
        .await;
    r.assert_status(201).assert_no_sql_error();
    let inv = r.data()["invoices"][0].clone();
    assert_eq!(inv["status"], "UNPAID");
    assert_eq!(inv["totalPrice"], 599400);
    assert_eq!(inv["note"], "Q1");
    assert!(inv["dueDate"].is_string());
    let item: (String, f64) = sqlx::query_as(
        r#"SELECT ii.description, ii.quantity FROM "invoiceItems" ii JOIN invoices i ON i.id = ii."invoiceId"
           WHERE i.uuid = $1::uuid"#,
    )
    .bind(inv["uuid"].as_str().unwrap())
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!(
        item.0,
        "Langganan Starter Monthly (3 bulanan): 6 user × 3 bulan"
    );
    assert_eq!(item.1, 18.0);
    let r = app
        .post(&format!("{base}/manual/invoices"))
        .token(&finance)
        .json(&json!({}))
        .send()
        .await;
    assert_eq!(r.json["code"], "OPEN_INVOICE_EXISTS");

    let pay_path = format!(
        "{base}/manual/invoices/{}/pay",
        inv["uuid"].as_str().unwrap()
    );
    let r = app
        .post(&pay_path)
        .token(&finance)
        .json(&json!({ "method": "BANK_TRANSFER", "reference": reference, "paidAt": iso(-1) }))
        .send()
        .await;
    assert_eq!(r.json["code"], "DUPLICATE_PAYMENT_REFERENCE");
    let r = app
        .post(&pay_path)
        .token(&finance)
        .json(&json!({ "method": "BANK_TRANSFER", "reference": format!("{reference}-2"), "paidAt": iso(-1) }))
        .send()
        .await;
    r.assert_status(201);
    assert_eq!(r.data()["invoices"][0]["status"], "PAID");
    assert_eq!(r.data()["invoices"][0]["note"], "Q1");
    let r = app
        .post(&pay_path)
        .token(&finance)
        .json(&json!({ "method": "BANK_TRANSFER", "reference": format!("{reference}-3"), "paidAt": iso(-1) }))
        .send()
        .await;
    assert_eq!(r.json["code"], "NOT_UNPAID");
    app.post(&format!(
        "{base}/manual/invoices/00000000-0000-4000-8000-000000000000/pay"
    ))
    .token(&finance)
    .json(&json!({ "method": "CASH", "reference": "x", "paidAt": iso(-1) }))
    .send()
    .await
    .assert_status(404);

    // stop
    let r = app
        .post(&format!("{base}/manual/stop"))
        .token(&finance)
        .json(&json!({ "mode": "AT_END", "reason": "tidak diperpanjang" }))
        .send()
        .await;
    r.assert_status(201);
    assert_eq!(r.data()["status"], "ACTIVE");
    let r = app
        .post(&format!("{base}/manual/stop"))
        .token(&finance)
        .json(&json!({ "mode": "NOW", "reason": "berhenti sekarang" }))
        .send()
        .await;
    r.assert_status(201);
    assert_eq!(r.data()["status"], "INACTIVE");
    assert_eq!(r.data()["lock"]["inGracePeriod"], true);
    let r = app
        .post(&format!("{base}/manual/stop"))
        .token(&finance)
        .json(&json!({ "mode": "NOW", "reason": "berhenti sekarang" }))
        .send()
        .await;
    assert_eq!(r.json["code"], "NOT_ACTIVE");

    // Boost TIME pada company MANUAL: dihitung ulang lewat applyCompanyPlanLimits.
    let r = app
        .patch(&format!("/admin-company/set/plan-boost/{}", fx.uuid))
        .token(&finance)
        .json(&json!({ "product": "TIME", "enabled": true }))
        .send()
        .await;
    r.assert_ok().assert_no_sql_error();
    let r = app
        .patch(&format!("/admin-company/set/plan-boost/{}", fx.uuid))
        .token(&finance)
        .json(&json!({ "product": "TIME", "enabled": false }))
        .send()
        .await;
    r.assert_ok();
    assert_eq!(r.data()["keptFields"], json!([]));

    let actions: Vec<String> = op_audit(&app, op)
        .await
        .iter()
        .map(|r| r[0].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        actions,
        vec![
            "subscription.manual.activate",
            "subscription.manual.extend",
            "subscription.manual.contract",
            "invoice.manual.create",
            "invoice.manual.pay",
            "subscription.manual.stop",
            "subscription.manual.stop",
            "company.plan_boost",
            "company.plan_boost",
        ]
    );
    let rows = op_audit(&app, op).await;
    assert_eq!(rows[0][4]["billingSource"], "PAYPAL");
    assert_eq!(rows[0][5]["billingSource"], "MANUAL");
    assert_eq!(rows[0][5]["complimentary"], false);
    assert_eq!(rows[3][1], "invoice");
}

#[tokio::test]
async fn paypal_cancel_failure_and_issue_invoice() {
    let _g = SERIAL.lock().await;
    let Some((app, mock)) = app_with_paypal().await else {
        return;
    };
    let (_, owner) = operator(&app, "OWNER").await;
    let fx = company(&app, "ACTIVE", "PAYPAL", PAID_TIER).await;
    let pid = format!("I-{}", &uniq()[..10]);
    app.exec(&format!(
        r#"UPDATE subscriptions SET "paypalSubscriptionId" = '{pid}', "paypalStatus" = 'SUSPENDED' WHERE "companyId" = {}"#,
        fx.id
    ))
    .await;
    mock.on("GET", "/v1/billing/subscriptions/*", 500, json!({}));
    mock.on(
        "POST",
        "/v1/billing/subscriptions/*",
        500,
        json!({ "name": "INTERNAL" }),
    );
    let base = format!("/admin-company/{}/subscription/manual", fx.uuid);
    let tier = tier_uuid(&app, PAID_TIER).await;
    let start = Utc::now();
    let start_s = start.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let end_ok = mz_admin_company::manual::add_months(start.naive_utc(), 1)
        .and_utc()
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let body = json!({
        "pricingUuid": tier, "startDate": start_s, "endDate": end_ok, "maxUsers": 3, "payment": null,
        "issueInvoice": true, "contract": { "pricePerUser": 25000, "periodMonths": 1 },
    });

    let r = app
        .post(&format!("{base}/activate"))
        .token(&owner)
        .json(&body)
        .send()
        .await;
    r.assert_status(502);
    assert_eq!(r.json["code"], "PAYPAL_CANCEL_FAILED");
    let src: String = app
        .scalar(&format!(
            r#"SELECT "billingSource"::text FROM subscriptions WHERE "companyId" = {}"#,
            fx.id
        ))
        .await;
    assert_eq!(src, "PAYPAL");

    // SUBSCRIPTION_STATUS_INVALID = sudah batal.
    mock.on(
        "POST",
        "/v1/billing/subscriptions/*",
        422,
        json!({ "details": [{ "issue": "SUBSCRIPTION_STATUS_INVALID" }] }),
    );
    let mut wrong = body.clone();
    wrong["endDate"] = json!(iso(45));
    let r = app
        .post(&format!("{base}/activate"))
        .token(&owner)
        .json(&wrong)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(r.json["code"], "PERIOD_MISMATCH_CONTRACT");

    let r = app
        .post(&format!("{base}/activate"))
        .token(&owner)
        .json(&body)
        .send()
        .await;
    r.assert_status(201).assert_no_sql_error();
    let d = r.data();
    assert_eq!(d["invoices"][0]["status"], "UNPAID");
    assert_eq!(d["invoices"][0]["totalPrice"], 75000);
    assert_eq!(d["contract"]["pricePerUser"], 25000);
    assert_eq!(d["paypal"]["status"], "CANCELLED");
}
