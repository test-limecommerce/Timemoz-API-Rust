//! Helper bersama uji integrasi `mz-paypal-api` (PayPal TIRUAN, Postgres uji).
#![allow(
    dead_code,
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

pub fn build(state: &AppState) -> Router {
    mz_paypal_api::router(state)
        .with_state(state.clone())
        .layer(mz_audit::layer())
}

pub fn uniq() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

pub const STARTER_MONTHLY: i32 = 1;
pub const STARTER_YEARLY: i32 = 2;
pub const GROWTH_MONTHLY: i32 = 4;
pub const FREE: i32 = 6;

/// Server PayPal tiruan + app. `extra` menambah/menimpa var (mis. tanpa webhook id).
pub async fn app_with(extra: &[(&str, &str)]) -> Option<(TestApp, MockServer)> {
    let mock = MockServer::start().await;
    mock.on(
        "POST",
        "/v1/oauth2/token",
        200,
        json!({ "access_token": "tok", "expires_in": 3600 }),
    );
    mock.on(
        "POST",
        "/v1/notifications/verify-webhook-signature",
        200,
        json!({ "verification_status": "SUCCESS" }),
    );
    let base = mock.url();
    let mut vars: Vec<(&str, &str)> = vec![
        ("MZ_PAYPAL_API_BASE", base.as_str()),
        ("PAYPAL_CLIENT_ID", "cid"),
        ("PAYPAL_CLIENT_SECRET", "sec"),
        ("FRONTEND_URL", "https://app.example.test"),
        ("MZ_PAYPAL_STATUS_POLL_MS", "50"),
        ("MZ_PAYPAL_STATUS_MAX_WAIT_MS", "400"),
    ];
    vars.extend_from_slice(extra);
    let app = TestApp::with_vars(build, &vars).await?;
    Some((app, mock))
}

pub async fn app() -> Option<(TestApp, MockServer)> {
    app_with(&[("PAYPAL_WEBHOOK_ID", "WH-ID")]).await
}

pub struct Fx {
    pub company: i32,
    pub owner: i32,
    pub owner_email: String,
    pub sub: i32,
    /// `paypalSubscriptionId`.
    pub pp: String,
}

/// Company + owner (userCompany default) + subscription PayPal. `sub_sql` = kolom tambahan
/// `SET` (mis. `"autoRenew" = false`).
pub async fn fixture(app: &TestApp, tier: i32, is_trial: bool, sub_set: &str) -> Fx {
    let u = uniq();
    let company: i32 = app
        .scalar(&format!(
            r#"INSERT INTO company (name, status, "timeZone", "isTrial", "maxUsers")
               VALUES ('it-pp-{u}', 1, 'Asia/Jakarta', {is_trial}, 1) RETURNING id"#
        ))
        .await;
    let email = format!("it-pp-{u}@example.com");
    let owner: i32 = app
        .scalar(&format!(
            r#"INSERT INTO users (username, password, name, status)
               VALUES ('{email}', 'x', 'Owner {u}', 1) RETURNING id"#
        ))
        .await;
    app.exec(&format!(
        r#"INSERT INTO "userCompany" ("userId", "companyId", status, "idleTimeInSecond", "isDefault")
           VALUES ({owner}, {company}, 1, 180, true);
           INSERT INTO "userAccessLevel" ("userId", "companyId", "accessLevelId", status)
           VALUES ({owner}, {company}, (SELECT id FROM "accessLevel" WHERE "isSystem" = true AND code = 'SUPER_ADMIN'
                                       AND "companyId" IS NULL ORDER BY id LIMIT 1), 1);
           UPDATE company SET "ownerUserId" = {owner} WHERE id = {company};"#
    ))
    .await;
    let pp = format!("I-{}", u[..12].to_uppercase());
    let sub: i32 = app
        .scalar(&format!(
            r#"INSERT INTO subscriptions ("companyId", "pricingTierId", "startDate", "endDate", status, "maxUsers",
                   quantity, "pricePerUser", "billingInterval", "paypalSubscriptionId", "paypalStatus")
               SELECT {company}, {tier}, NOW() - interval '25 day', NOW() + interval '5 day', 'ACTIVE', 2, 2,
                      t."pricePerUserUsd", t."billingInterval", '{pp}', 'ACTIVE'
                 FROM "pricingTiers" t WHERE t.id = {tier} RETURNING id"#
        ))
        .await;
    if !sub_set.is_empty() {
        app.exec(&format!(
            "UPDATE subscriptions SET {sub_set} WHERE id = {sub}"
        ))
        .await;
    }
    Fx {
        company,
        owner,
        owner_email: email,
        sub,
        pp,
    }
}

/// Invoice dengan status/tipe tertentu → id.
pub async fn invoice(
    app: &TestApp,
    sub: i32,
    status: &str,
    kind: &str,
    paypal_payment_id: Option<&str>,
    total: f64,
) -> i32 {
    let u = uniq();
    let pid = paypal_payment_id.map_or("NULL".to_owned(), |p| format!("'{p}'"));
    app.scalar(&format!(
        r#"INSERT INTO invoices ("subscriptionId", "invoiceNumber", "paymentUrl", "subPrice", "totalPrice",
               status, "invoiceType", "paypalPaymentId")
           VALUES ({sub}, 'INV-IT-{}', 'https://pay.example/old', {total}, {total},
                   '{status}'::"InvoiceStatus", '{kind}'::"InvoiceType", {pid}) RETURNING id"#,
        &u[..10]
    ))
    .await
}

pub async fn sub_row(app: &TestApp, sub: i32) -> Value {
    let v: Value = app
        .scalar(&format!(
            r#"SELECT json_build_object('status', status::text, 'paypalStatus', "paypalStatus",
                   'quantity', quantity, 'autoRenew', "autoRenew", 'lastBilledQuantity', "lastBilledQuantity",
                   'pricingTierId', "pricingTierId", 'paypalSubscriptionId', "paypalSubscriptionId",
                   'paypalPlanId', "paypalPlanId", 'pendingPricingTierId', "pendingPricingTierId",
                   'pendingPaypalSubscriptionId', "pendingPaypalSubscriptionId",
                   'endInDays', round(extract(epoch from ("endDate" - NOW())) / 86400),
                   'nextInvoiceSet', "nextInvoiceDate" IS NOT NULL)
                 FROM subscriptions WHERE id = {sub}"#
        ))
        .await;
    v
}

pub async fn invoice_status(app: &TestApp, id: i32) -> String {
    app.scalar(&format!(
        "SELECT status::text FROM invoices WHERE id = {id}"
    ))
    .await
}

pub async fn company_row(app: &TestApp, company: i32) -> Value {
    app.scalar(&format!(
        r#"SELECT json_build_object('maxUsers', "maxUsers", 'isTrial', "isTrial", 'maxProject', "maxProject",
               'screenshotResolution', "screenshotResolution", 'reportExportsUsedThisMonth', "reportExportsUsedThisMonth")
             FROM company WHERE id = {company}"#
    ))
    .await
}

/// Operator aktif → (id, token).
pub async fn operator(app: &TestApp, role: &str) -> (i32, String) {
    let u = uniq();
    let id: i32 = app
        .scalar(&format!(
            r#"INSERT INTO operators (username, password, name, status, role)
               VALUES ('op{u}', 'x', 'IT Op {role}', 1, '{role}'::"OperatorRole") RETURNING id"#
        ))
        .await;
    (
        id,
        app.token_with(json!({ "username": format!("op{u}"), "sub": id })),
    )
}

/// Tunggu sampai kondisi SQL (scalar bool) benar (proses latar).
pub async fn wait_until(app: &TestApp, sql: &str) -> bool {
    for _ in 0..100 {
        let ok: bool = app.scalar(sql).await;
        if ok {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    false
}
