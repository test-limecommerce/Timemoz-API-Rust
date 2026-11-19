//! Uji integrasi cron billing terhadap Postgres nyata, PayPal tiruan (`MockServer`), email
//! tercatat (`Mailer::recording`), dan jam yang dikendalikan (`Ctx::now` di tahun 2001, jadi data
//! template — semuanya jauh sesudahnya — tidak ikut tersaring query berbasis waktu). Jalankan:
//! `MZ_TEST_DATABASE_URL=... MZ_TEST_REQUIRE_DB=1 cargo test -p mz-billing-tasks --test it_billing_tasks_cron`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use axum::Router;
use chrono::{Duration, NaiveDateTime};
use mz_audit::{ActorType, AuditHandle};
use mz_billing_tasks::mail::OutMail;
use mz_billing_tasks::{
    Ctx, Mailer, escalation, expiration, plan_change, proration_expiry, reminders, scheduler,
};
use mz_core::AppState;
use mz_int_common::mock::MockServer;
use mz_paypal::{PaypalClient, PaypalConfig};
use mz_testkit::TestApp;
use serde_json::{Value, json};
use tokio::sync::Mutex;

static SERIAL: Mutex<()> = Mutex::const_new(());

fn build(_state: &AppState) -> Router {
    Router::new()
}

async fn app(vars: &[(&str, &str)]) -> Option<TestApp> {
    let mut all = vec![("MAILTRAP_TOKEN", ""), ("PAYMENT_URL", "https://pay.test")];
    all.extend_from_slice(vars);
    TestApp::with_vars(build, &all).await
}

fn uniq() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// Jam uji: 2001-06-15 02:00:00 UTC.
fn t0() -> NaiveDateTime {
    NaiveDateTime::parse_from_str("2001-06-15 02:00:00", "%Y-%m-%d %H:%M:%S").unwrap()
}

/// Literal SQL `t0 + jam`.
fn at(hours: i64) -> String {
    format!(
        "'{}'",
        (t0() + Duration::hours(hours)).format("%Y-%m-%d %H:%M:%S%.3f")
    )
}

async fn paypal_mock() -> MockServer {
    let m = MockServer::start().await;
    m.on(
        "POST",
        "/v1/oauth2/token",
        200,
        json!({"access_token": "TOK", "expires_in": 3600}),
    );
    m.on("PATCH", "/v1/billing/subscriptions/*", 200, json!({}));
    m.on("POST", "/v1/billing/subscriptions/*", 204, Value::Null);
    m.on(
        "POST",
        "/v2/checkout/orders",
        201,
        json!({"id": "ORDER-1", "links": [{"rel": "approve", "href": "https://paypal.test/approve/ORDER-1"}]}),
    );
    m
}

fn ctx(app: &TestApp, mock: &MockServer, now: NaiveDateTime, actor: &str) -> Ctx {
    let url = mock.url();
    let cfg = PaypalConfig::from_lookup(|k| match k {
        "MZ_PAYPAL_API_BASE" => Some(url.clone()),
        "PAYPAL_CLIENT_ID" => Some("cid".into()),
        "PAYPAL_CLIENT_SECRET" => Some("sec".into()),
        _ => None,
    });
    Ctx {
        state: app.state().clone(),
        paypal: PaypalClient::new(cfg, app.state().http.clone()),
        mail: Mailer::recording(),
        audit: AuditHandle::run_as(app.db().clone(), ActorType::System, Some(actor)),
        now,
    }
}

async fn company(app: &TestApp, is_trial: bool, dont_charge: bool) -> i32 {
    app.scalar(&format!(
        r#"INSERT INTO company (name, status, "isTrial", "dontChargeInvite", "maxUsers")
           VALUES ('it-bt-{}', 1, {is_trial}, {dont_charge}, 5) RETURNING id"#,
        uniq()
    ))
    .await
}

/// User + keanggotaan aktif. Kembali `(userId, userCompanyId, username)`.
async fn member(
    app: &TestApp,
    company: i32,
    created: &str,
    is_default: bool,
) -> (i32, i32, String) {
    let username = format!("bt{}@x.test", uniq());
    let uid: i32 = app
        .scalar(&format!(
            r#"INSERT INTO users (username, password, name, status, "createdAt")
               VALUES ('{username}', 'x', 'Budi', 1, {created}) RETURNING id"#
        ))
        .await;
    let uc: i32 = app
        .scalar(&format!(
            r#"INSERT INTO "userCompany" ("userId", "companyId", status, "idleTimeInSecond", "isDefault", "createdAt")
               VALUES ({uid}, {company}, 1, 300, {is_default}, {created}) RETURNING id"#
        ))
        .await;
    (uid, uc, username)
}

async fn tier(app: &TestApp, price: f64, interval: &str, product: &str) -> i32 {
    app.scalar(&format!(
        r#"INSERT INTO "pricingTiers" (name, status, "productType", "pricePerUserUsd", "billingInterval",
               "maxUsers", "maxProject", "useReports")
           VALUES ('Pro-{}', 1, '{product}', {price}, '{interval}', 50, 20, true) RETURNING id"#,
        uniq()
    ))
    .await
}

async fn ensure_free_tier(app: &TestApp) {
    let has: i64 = app
        .scalar(r#"SELECT COUNT(*) FROM "pricingTiers" WHERE name ILIKE 'free' AND "productType" = 'FULL' AND status = 1"#)
        .await;
    if has == 0 {
        app.exec(
            r#"INSERT INTO "pricingTiers" (name, status, "productType", "maxUsers", "maxProject")
               VALUES ('Free', 1, 'FULL', 3, 2)"#,
        )
        .await;
    }
}

/// Subscription; `extra` = pasangan kolom/nilai SQL tambahan.
async fn sub(app: &TestApp, company: i32, tier: i32, extra: &[(&str, String)]) -> i32 {
    let mut cols = vec![
        r#""companyId""#.to_owned(),
        r#""pricingTierId""#.to_owned(),
        r#""startDate""#.to_owned(),
    ];
    let mut vals = vec![company.to_string(), tier.to_string(), at(-24 * 300)];
    let mut has_end = false;
    for (c, v) in extra {
        has_end |= *c == "endDate";
        cols.push(format!("\"{c}\""));
        vals.push(v.clone());
    }
    if !has_end {
        cols.push(r#""endDate""#.into());
        vals.push(at(24 * 300));
    }
    app.scalar(&format!(
        "INSERT INTO subscriptions ({}) VALUES ({}) RETURNING id",
        cols.join(", "),
        vals.join(", ")
    ))
    .await
}

fn s(v: &str) -> String {
    format!("'{v}'")
}

fn to(mails: &[OutMail], template: &str) -> Vec<String> {
    mails
        .iter()
        .filter(|m| m.template == template)
        .map(|m| m.to_email.clone())
        .collect()
}

async fn text(app: &TestApp, sql: &str) -> String {
    app.scalar::<String>(sql).await
}

// ───────────────────────────── payment failure escalation ─────────────────────────────

#[tokio::test]
async fn payment_failure_escalation_d1_d7_idempotent() {
    let _g = SERIAL.lock().await;
    let Some(app) = app(&[]).await else { return };
    let mock = paypal_mock().await;
    let tr = tier(&app, 5.0, "MONTH", "FULL").await;

    let c1 = company(&app, false, false).await;
    let (_, _, owner1) = member(&app, c1, &at(-24 * 100), true).await;
    let s1 = sub(
        &app,
        c1,
        tr,
        &[
            ("status", s("ACTIVE")),
            ("paypalStatus", s("PAYMENT_FAILED")),
            ("updatedAt", at(-30)),
        ],
    )
    .await;

    let c7 = company(&app, false, false).await;
    let (_, _, owner7) = member(&app, c7, &at(-24 * 100), true).await;
    let s7 = sub(
        &app,
        c7,
        tr,
        &[
            ("status", s("ACTIVE")),
            ("paypalStatus", s("PAYMENT_FAILED_D1")),
            ("updatedAt", at(-24 * 7)),
        ],
    )
    .await;

    // MANUAL tidak pernah ditagih PayPal: tidak disentuh.
    let cm = company(&app, false, false).await;
    let sm = sub(
        &app,
        cm,
        tr,
        &[
            ("billingSource", s("MANUAL")),
            ("paypalStatus", s("PAYMENT_FAILED_D1")),
            ("updatedAt", at(-24 * 7)),
        ],
    )
    .await;
    // Belum 1 hari: dilewati.
    let cf = company(&app, false, false).await;
    let sf = sub(
        &app,
        cf,
        tr,
        &[("paypalStatus", s("PAYMENT_FAILED")), ("updatedAt", at(-2))],
    )
    .await;

    let cx = ctx(&app, &mock, t0(), escalation::ACTOR);
    escalation::run(&cx).await;

    assert_eq!(
        text(
            &app,
            &format!(r#"SELECT "paypalStatus" FROM subscriptions WHERE id = {s1}"#)
        )
        .await,
        "PAYMENT_FAILED_D1"
    );
    let upd: NaiveDateTime = app
        .scalar(&format!(
            r#"SELECT "updatedAt" FROM subscriptions WHERE id = {s1}"#
        ))
        .await;
    assert_eq!(upd, t0());
    assert_eq!(
        text(
            &app,
            &format!(r#"SELECT "paypalStatus" FROM subscriptions WHERE id = {s7}"#)
        )
        .await,
        "ACCESS_BLOCKED"
    );
    let st: i32 = app
        .scalar(&format!("SELECT status FROM company WHERE id = {c7}"))
        .await;
    assert_eq!(st, 0);
    assert_eq!(
        text(
            &app,
            &format!(r#"SELECT "paypalStatus" FROM subscriptions WHERE id = {sm}"#)
        )
        .await,
        "PAYMENT_FAILED_D1"
    );
    let stm: i32 = app
        .scalar(&format!("SELECT status FROM company WHERE id = {cm}"))
        .await;
    assert_eq!(stm, 1);
    assert_eq!(
        text(
            &app,
            &format!(r#"SELECT "paypalStatus" FROM subscriptions WHERE id = {sf}"#)
        )
        .await,
        "PAYMENT_FAILED"
    );

    let mails = cx.mail.sent();
    assert_eq!(
        to(&mails, "d+1-payment-failed"),
        std::slice::from_ref(&owner1)
    );
    assert_eq!(
        to(&mails, "goodbye-access-blocked"),
        std::slice::from_ref(&owner7)
    );
    let d1 = mails
        .iter()
        .find(|m| m.template == "d+1-payment-failed")
        .unwrap();
    assert_eq!(
        d1.context,
        json!({"name": "Budi", "url": "https://pay.test"})
    );
    assert_eq!(d1.category, "Payment Failed D+1");

    // Audit company.update dengan pelaku cron.
    let n: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "auditLog" WHERE "entityType" = 'company' AND "entityId" = {c7}
                 AND "actorType" = 'system' AND "actorName" = 'payment-failure-escalation'
                 AND 'status' = ANY("changedFields")"#
        ))
        .await;
    assert_eq!(n, 1);

    // Jalan ulang di jam yang sama: tidak ada email / tulisan baru.
    let again = ctx(&app, &mock, t0(), escalation::ACTOR);
    escalation::run(&again).await;
    assert!(again.mail.sent().is_empty());
}

// ───────────────────────────── reminders ─────────────────────────────

#[tokio::test]
async fn download_and_trial_reminders() {
    let _g = SERIAL.lock().await;
    let Some(app) = app(&[]).await else { return };
    let mock = paypal_mock().await;
    let c = company(&app, true, false).await;
    // 36 jam lalu, belum tracking → dikirimi.
    let (_, _, fresh) = member(&app, c, &at(-36), true).await;
    // 36 jam lalu tapi sudah tracking → tidak.
    let (tracked_id, _, tracked) = member(&app, c, &at(-30), false).await;
    app.exec(&format!(
        r#"INSERT INTO tracking ("takenDate", "userId", status) VALUES ('2001-06-14', {tracked_id}, 1)"#
    ))
    .await;
    // 3 hari lalu → di luar jendela.
    let (_, _, old) = member(&app, c, &at(-72), false).await;

    let cx = ctx(&app, &mock, t0(), "download-reminder");
    reminders::download_reminder(&cx).await;
    let sent = to(&cx.mail.sent(), "download-reminder");
    assert!(sent.contains(&fresh));
    assert!(!sent.contains(&tracked));
    assert!(!sent.contains(&old));

    // Trial ending: endDate di hari kalender ke-4 (2001-06-19), owner = userCompany isDefault.
    let sales = tier(&app, 0.0, "MONTH", "SALES").await;
    let s_end = sub(
        &app,
        c,
        sales,
        &[("status", s("TRIAL")), ("endDate", at(24 * 4 + 15))],
    )
    .await;
    // Hari ke-5 → tidak.
    let c2 = company(&app, true, false).await;
    let (_, _, other_owner) = member(&app, c2, &at(-24 * 30), true).await;
    sub(
        &app,
        c2,
        sales,
        &[("status", s("TRIAL")), ("endDate", at(24 * 5))],
    )
    .await;

    let cx = ctx(&app, &mock, t0(), "trial-ending-reminder");
    reminders::trial_ending_reminder(&cx).await;
    let mails = cx.mail.sent();
    assert_eq!(
        to(&mails, "trial-ending-reminder"),
        std::slice::from_ref(&fresh),
        "sub {s_end}"
    );
    assert_eq!(
        mails[0].context["url"],
        json!("https://timemoz.com/pricing/sales-tracker")
    );
    assert!(!to(&mails, "trial-ending-reminder").contains(&other_owner));

    // Trial ended: TIME berbayar PayPal INACTIVE, endDate hari ini, tanpa invoice PAID.
    let time = tier(&app, 4.0, "MONTH", "TIME").await;
    let c3 = company(&app, false, false).await;
    let (_, _, ended_owner) = member(&app, c3, &at(-24 * 30), true).await;
    sub(
        &app,
        c3,
        time,
        &[
            ("status", s("INACTIVE")),
            ("pricePerUser", "4".into()),
            ("endDate", at(-1)),
        ],
    )
    .await;
    // Sudah pernah bayar → tidak.
    let c4 = company(&app, false, false).await;
    let (_, _, paid_owner) = member(&app, c4, &at(-24 * 30), true).await;
    let s4 = sub(
        &app,
        c4,
        time,
        &[
            ("status", s("INACTIVE")),
            ("pricePerUser", "4".into()),
            ("endDate", at(-1)),
        ],
    )
    .await;
    app.exec(&format!(
        r#"INSERT INTO invoices ("subscriptionId", "invoiceNumber", "paymentUrl", status)
           VALUES ({s4}, 'INV-X', '', 'PAID')"#
    ))
    .await;
    let cx = ctx(&app, &mock, t0(), "trial-ended-reminder");
    reminders::trial_ended_reminder(&cx).await;
    let sent = to(&cx.mail.sent(), "trial-ended-reminder");
    assert_eq!(sent, [ended_owner]);
    assert!(!sent.contains(&paid_owner));
}

#[tokio::test]
async fn manual_renewal_reminder_once_per_end_date() {
    let _g = SERIAL.lock().await;
    let Some(app) = app(&[
        ("MANUAL_BILLING_NOTIFY_EMAIL", " finance@x.test "),
        ("ADMIN_DASHBOARD_URL", "https://admin.test/"),
    ])
    .await
    else {
        return;
    };
    let mock = paypal_mock().await;
    let tr = tier(&app, 5.0, "MONTH", "FULL").await;
    let c = company(&app, false, false).await;
    app.exec(&format!(
        "UPDATE company SET name = 'PT Maju\nJaya' WHERE id = {c}"
    ))
    .await;
    let sid = sub(
        &app,
        c,
        tr,
        &[
            ("billingSource", s("MANUAL")),
            ("status", s("ACTIVE")),
            ("endDate", at(24 * 7 + 3)),
        ],
    )
    .await;

    let cx = ctx(&app, &mock, t0(), "manual-renewal-reminder");
    reminders::manual_renewal_reminder(&cx).await;
    let mails = cx.mail.sent();
    assert_eq!(mails.len(), 1);
    let m = &mails[0];
    assert_eq!(m.to_email, "finance@x.test");
    assert_eq!(m.to_name, "Finance");
    assert_eq!(
        m.subject,
        "Manual subscription ends in 7 days: PT Maju Jaya"
    );
    assert_eq!(m.context["endDate"], json!("2001-06-22"));
    let uuid = text(
        &app,
        &format!("SELECT uuid::text FROM company WHERE id = {c}"),
    )
    .await;
    assert_eq!(
        m.context["adminUrl"],
        json!(format!(
            "https://admin.test/dashboard/management-company/{uuid}"
        ))
    );
    let marked: Option<NaiveDateTime> = app
        .scalar(&format!(
            r#"SELECT "manualReminderForEndDate" FROM subscriptions WHERE id = {sid}"#
        ))
        .await;
    assert_eq!(marked, Some(t0() + Duration::hours(24 * 7 + 3)));

    // Jalan ulang: klaim kalah → tidak ada email kedua.
    let again = ctx(&app, &mock, t0(), "manual-renewal-reminder");
    reminders::manual_renewal_reminder(&again).await;
    assert!(again.mail.sent().is_empty());
}

// ───────────────────────────── invoice scheduler ─────────────────────────────

#[tokio::test]
async fn invoice_scheduler_monthly_defers_new_users_and_is_idempotent() {
    let _g = SERIAL.lock().await;
    let Some(app) = app(&[]).await else { return };
    let mock = paypal_mock().await;
    let tr = tier(&app, 9.0, "MONTH", "FULL").await;
    let c = company(&app, false, false).await;
    let (_, _, owner) = member(&app, c, &at(-24 * 60), true).await;
    member(&app, c, &at(-24 * 40), false).await;
    // Bergabung 2 hari sebelum endDate → ditunda.
    member(&app, c, &at(24), false).await;
    let sid = sub(
        &app,
        c,
        tr,
        &[
            ("status", s("ACTIVE")),
            ("paypalSubscriptionId", s("I-MONTH")),
            ("pricePerUser", "5".into()),
            ("nextInvoiceDate", at(-1)),
            ("endDate", at(24 * 3)),
        ],
    )
    .await;

    let cx = ctx(&app, &mock, t0(), "invoice-scheduler");
    scheduler::invoice_scheduler(&cx).await;

    let inv: (String, f64, f64, String, String, NaiveDateTime, NaiveDateTime) = sqlx::query_as(&format!(
        r#"SELECT "invoiceNumber", "subPrice", "totalPrice", status::text, "invoiceType"::text, "expiredAt", "createdAt"
             FROM invoices WHERE "subscriptionId" = {sid}"#
    ))
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!(
        inv.0,
        format!("INV-15062001-{}", t0().and_utc().timestamp())
    );
    assert_eq!((inv.1, inv.2), (10.0, 10.0));
    assert_eq!((inv.3.as_str(), inv.4.as_str()), ("UNPAID", "SUBSCRIPTION"));
    assert_eq!(inv.5, t0() + Duration::days(15));
    assert_eq!(inv.6, t0());
    let item: (String, f64, f64, f64) = sqlx::query_as(&format!(
        r#"SELECT description, quantity, "unitPrice", "totalPrice" FROM "invoiceItems"
            WHERE "invoiceId" = (SELECT id FROM invoices WHERE "subscriptionId" = {sid})"#
    ))
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!(
        item,
        (
            "Monthly Subscription - 2 user(s) @ $5/user (1 user(s) deferred to next month)"
                .to_owned(),
            2.0,
            5.0,
            10.0
        )
    );
    let q: (i32, i32) = sqlx::query_as(&format!(
        r#"SELECT quantity, "lastBilledQuantity" FROM subscriptions WHERE id = {sid}"#
    ))
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!(q, (3, 3));
    let patch = mock.requests_to("PATCH", "/v1/billing/subscriptions/I-MONTH");
    assert_eq!(patch.len(), 1);
    assert_eq!(
        patch[0].json(),
        json!([{"op": "replace", "path": "/quantity", "value": "2"}])
    );
    let mails = cx.mail.sent();
    let due = mails.iter().find(|m| m.template == "duedate").unwrap();
    assert_eq!(due.to_email, owner);
    assert_eq!(due.subject, "Timemoz Billing Notice – Due Date 18-06-2001");
    assert_eq!(due.context["totalPrice"], json!(10));

    // Jalan ulang (cron bisa jalan dua kali): invoice < 24 jam → dilewati.
    let again = ctx(&app, &mock, t0() + Duration::hours(1), "invoice-scheduler");
    scheduler::invoice_scheduler(&again).await;
    let n: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM invoices WHERE "subscriptionId" = {sid}"#
        ))
        .await;
    assert_eq!(n, 1);

    // Force melewati cek 24 jam (khusus company ini).
    let forced = ctx(&app, &mock, t0() + Duration::hours(2), "x");
    let res = scheduler::invoice_scheduler_force(&forced, Some(c))
        .await
        .unwrap();
    assert_eq!(
        res,
        json!({"success": true, "message": "Processed 1 subscription(s)"})
    );
    let n: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM invoices WHERE "subscriptionId" = {sid}"#
        ))
        .await;
    assert_eq!(n, 2);

    // dontChargeInvite → tidak ditagih.
    app.exec(&format!(
        r#"UPDATE company SET "dontChargeInvite" = true WHERE id = {c}"#
    ))
    .await;
    scheduler::invoice_scheduler_force(&forced, Some(c))
        .await
        .unwrap();
    let n: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM invoices WHERE "subscriptionId" = {sid}"#
        ))
        .await;
    assert_eq!(n, 2);
}

#[tokio::test]
async fn invoice_scheduler_annual_first_then_proration() {
    let _g = SERIAL.lock().await;
    let Some(app) = app(&[("FRONTEND_URL", "https://app.test")]).await else {
        return;
    };
    let mock = paypal_mock().await;
    let tr = tier(&app, 4.0, "YEAR", "FULL").await;
    let c = company(&app, false, false).await;
    let (_, _, owner) = member(&app, c, &at(-24 * 60), true).await;
    member(&app, c, &at(-24 * 50), false).await;
    let end = 24 * 365;
    let sid = sub(
        &app,
        c,
        tr,
        &[
            ("status", s("ACTIVE")),
            ("billingInterval", s("YEAR")),
            ("paypalSubscriptionId", s("I-YEAR")),
            ("pricePerUser", "0".into()),
            ("nextInvoiceDate", at(-1)),
            ("endDate", at(end)),
        ],
    )
    .await;

    // Pembayaran pertama: 2 user × $4 × 12 (harga dari tier karena pricePerUser 0).
    let cx = ctx(&app, &mock, t0(), "invoice-scheduler");
    scheduler::invoice_scheduler(&cx).await;
    let first: (f64, String) = sqlx::query_as(&format!(
        r#"SELECT i."totalPrice", it.description FROM invoices i JOIN "invoiceItems" it ON it."invoiceId" = i.id
            WHERE i."subscriptionId" = {sid}"#
    ))
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!(
        first,
        (
            96.0,
            "Annual Subscription - 2 user(s) @ $4/user/month (billed yearly)".to_owned()
        )
    );
    // Bukan renewal → tanpa email duedate.
    assert!(to(&cx.mail.sent(), "duedate").is_empty());

    // Dua user baru bergabung di tanggal berbeda; cron hari berikutnya → proration.
    member(&app, c, &at(24 * 2), false).await;
    member(&app, c, &at(24 * 3 + 5), false).await;
    let now2 = t0() + Duration::days(5);
    app.exec(&format!(
        r#"UPDATE subscriptions SET "nextInvoiceDate" = '{}' WHERE id = {sid}"#,
        (now2 - Duration::hours(1)).format("%Y-%m-%d %H:%M:%S")
    ))
    .await;
    let cx2 = ctx(&app, &mock, now2, "invoice-scheduler");
    scheduler::invoice_scheduler(&cx2).await;

    let pr: (i32, String, String, Option<String>, f64, NaiveDateTime) = sqlx::query_as(&format!(
        r#"SELECT id, "invoiceNumber", "paymentUrl", "paypalPaymentId", "totalPrice", "createdAt"
             FROM invoices WHERE "subscriptionId" = {sid} AND "invoiceType" = 'PRORATION'"#
    ))
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!(pr.2, "https://paypal.test/approve/ORDER-1");
    assert_eq!(pr.3.as_deref(), Some("ORDER-1"));
    // 2001-06-17 → 2002-06-15: 363 hari → 13 bulan; 2001-06-18: 362 hari → 13 bulan.
    assert_eq!(pr.4, 2.0 * 4.0 * 13.0);
    assert_eq!(pr.5, now2);
    let items: Vec<String> = sqlx::query_scalar(&format!(
        r#"SELECT description FROM "invoiceItems" WHERE "invoiceId" = {} ORDER BY id"#,
        pr.0
    ))
    .fetch_all(app.db())
    .await
    .unwrap();
    assert_eq!(
        items,
        [
            "Add-on: 1 user(s) × $4/user × 13 month(s) (2001-06-17 – 2002-06-15)",
            "Add-on: 1 user(s) × $4/user × 13 month(s) (2001-06-18 – 2002-06-15)",
        ]
    );
    let q: (i32, i32, NaiveDateTime) = sqlx::query_as(&format!(
        r#"SELECT quantity, "lastBilledQuantity", "nextInvoiceDate" FROM subscriptions WHERE id = {sid}"#
    ))
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!((q.0, q.1), (4, 4));
    assert_eq!(q.2.format("%Y-%m-%d %H:%M").to_string(), "2001-07-01 00:00");

    let order = mock.requests_to("POST", "/v2/checkout/orders");
    assert_eq!(order.len(), 1);
    let ob = order[0].json();
    assert_eq!(ob["purchase_units"][0]["amount"]["value"], json!("104.00"));
    assert_eq!(
        ob["purchase_units"][0]["description"],
        json!("Timemoz Proration — 2 add-on user(s)")
    );
    assert_eq!(
        ob["application_context"]["return_url"],
        json!("http://be.test/paypal/proration/return")
    );
    assert_eq!(
        ob["application_context"]["cancel_url"],
        json!(format!(
            "https://app.test/billing?proration=cancelled&invoice={}",
            pr.1
        ))
    );
    let mails = cx2.mail.sent();
    let inv = mails.iter().find(|m| m.template == "invoice").unwrap();
    assert_eq!(inv.to_email, owner);
    assert_eq!(
        inv.context["tierName"],
        json!("Add-on: 2 user(s) (prorated)")
    );
    assert_eq!(inv.context["period"], json!("2001-06-20 – 2002-06-15"));
    assert_eq!(inv.context["paymentDueDate"], json!("2001-06-26"));
    assert_eq!(inv.context["totalPrice"], json!(104));

    // Jalan ulang: tidak ada user baru → tidak ada invoice/order baru.
    let cx3 = ctx(&app, &mock, now2 + Duration::days(30), "invoice-scheduler");
    app.exec(&format!(
        r#"UPDATE subscriptions SET "nextInvoiceDate" = '2001-07-01' WHERE id = {sid}"#
    ))
    .await;
    scheduler::invoice_scheduler(&cx3).await;
    let n: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM invoices WHERE "subscriptionId" = {sid}"#
        ))
        .await;
    assert_eq!(n, 2);
    assert_eq!(mock.requests_to("POST", "/v2/checkout/orders").len(), 1);
}

#[tokio::test]
async fn proration_force_paypal_failure_writes_nothing() {
    let _g = SERIAL.lock().await;
    let Some(app) = app(&[]).await else { return };
    let mock = paypal_mock().await;
    mock.on(
        "POST",
        "/v2/checkout/orders",
        500,
        json!({"name": "INTERNAL_SERVER_ERROR"}),
    );
    let tr = tier(&app, 3.0, "YEAR", "FULL").await;
    let c = company(&app, false, false).await;
    member(&app, c, &at(-24 * 60), true).await;
    member(&app, c, &at(-24), false).await;
    let sid = sub(
        &app,
        c,
        tr,
        &[
            ("status", s("ACTIVE")),
            ("billingInterval", s("YEAR")),
            ("pricePerUser", "3".into()),
            ("lastBilledQuantity", "1".into()),
            ("endDate", at(24 * 200)),
        ],
    )
    .await;
    let cx = ctx(&app, &mock, t0(), "x");
    let res = scheduler::annual_proration_force(&cx, Some(c))
        .await
        .unwrap();
    assert_eq!(
        res,
        json!({"success": true, "processed": 0, "skipped": 0, "details": [{
            "subscriptionId": sid, "companyId": c, "status": "error",
            "message": "PayPal order failed — nothing written"
        }]})
    );
    let n: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM invoices WHERE "subscriptionId" = {sid}"#
        ))
        .await;
    assert_eq!(n, 0);
    let lb: i32 = app
        .scalar(&format!(
            r#"SELECT "lastBilledQuantity" FROM subscriptions WHERE id = {sid}"#
        ))
        .await;
    assert_eq!(lb, 1);

    // Sukses sesudah PayPal pulih; force kedua → "No new users".
    mock.on(
        "POST",
        "/v2/checkout/orders",
        201,
        json!({"id": "ORDER-2", "links": [{"rel": "approve", "href": "https://paypal.test/a/2"}]}),
    );
    let res = scheduler::annual_proration_force(&cx, Some(c))
        .await
        .unwrap();
    assert_eq!(res["processed"], json!(1));
    assert_eq!(
        res["details"][0]["message"],
        json!("Proration processed — 1 new user(s) billed")
    );
    let res = scheduler::annual_proration_force(&cx, Some(c))
        .await
        .unwrap();
    assert_eq!(
        res["details"][0]["message"],
        json!("No new users (active: 2, lastBilled: 2) — skipped")
    );
}

// ───────────────────────────── proration expiration ─────────────────────────────

#[tokio::test]
async fn proration_expiration_deactivates_added_users_once() {
    let _g = SERIAL.lock().await;
    let Some(app) = app(&[]).await else { return };
    let mock = paypal_mock().await;
    let tr = tier(&app, 3.0, "YEAR", "FULL").await;
    let c = company(&app, false, false).await;
    let (_, uc0, _) = member(&app, c, &at(-24 * 60), true).await;
    let (u1, uc1, _) = member(&app, c, &at(-24 * 20), false).await;
    let (u2, uc2, _) = member(&app, c, &at(-24 * 19), false).await;
    let div: i32 = app
        .scalar(&format!(
            "INSERT INTO division (\"companyId\", name) VALUES ({c}, 'Div') RETURNING id"
        ))
        .await;
    app.exec(&format!(
        r#"INSERT INTO "divisionMember" ("companyId", "divisionId", "userId", role)
           VALUES ({c}, {div}, {u1}, 'MEMBER'), ({c}, {div}, {u2}, 'LEADER')"#
    ))
    .await;
    let sid = sub(
        &app,
        c,
        tr,
        &[
            ("status", s("ACTIVE")),
            ("billingInterval", s("YEAR")),
            ("quantity", "3".into()),
            ("lastBilledQuantity", "3".into()),
        ],
    )
    .await;
    let inv: i32 = app
        .scalar(&format!(
            r#"INSERT INTO invoices ("subscriptionId", "invoiceNumber", "paymentUrl", status, "invoiceType", "createdAt")
               VALUES ({sid}, 'INV-PR-{}', '', 'UNPAID', 'PRORATION', {}) RETURNING id"#,
            uniq(),
            at(-24 * 8)
        ))
        .await;
    app.exec(&format!(
        r#"INSERT INTO "invoiceItems" ("invoiceId", description, quantity) VALUES ({inv}, 'a', 1), ({inv}, 'b', 1)"#
    ))
    .await;
    // Baru 3 hari → tidak tersentuh cron (force tetap memprosesnya).
    let young: i32 = app
        .scalar(&format!(
            r#"INSERT INTO invoices ("subscriptionId", "invoiceNumber", "paymentUrl", status, "invoiceType", "createdAt")
               VALUES ({sid}, 'INV-Y', '', 'UNPAID', 'PRORATION', {}) RETURNING id"#,
            at(-24 * 3)
        ))
        .await;

    let cx = ctx(&app, &mock, t0(), proration_expiry::ACTOR);
    let res = proration_expiry::run(&cx, Some(c), false).await.unwrap();
    assert_eq!(res["expired"], json!(1));
    assert_eq!(
        res["details"][0]["message"],
        json!("2 user(s) deactivated, lastBilledQuantity reset to 1")
    );
    let st: Vec<(i32, i32)> = sqlx::query_as(&format!(
        r#"SELECT id, status FROM "userCompany" WHERE id IN ({uc0}, {uc1}, {uc2}) ORDER BY id"#
    ))
    .fetch_all(app.db())
    .await
    .unwrap();
    assert_eq!(st, [(uc0, 1), (uc1, 0), (uc2, 0)]);
    let ids: Vec<i32> = app
        .scalar(&format!(
            r#"SELECT "prorationDeactivatedUserCompanyIds" FROM invoices WHERE id = {inv}"#
        ))
        .await;
    assert_eq!(ids, [uc1, uc2]);
    assert_eq!(
        text(
            &app,
            &format!("SELECT status::text FROM invoices WHERE id = {inv}")
        )
        .await,
        "EXPIRED"
    );
    assert_eq!(
        text(
            &app,
            &format!("SELECT status::text FROM invoices WHERE id = {young}")
        )
        .await,
        "UNPAID"
    );
    let live: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "divisionMember" WHERE "divisionId" = {div} AND "deletedAt" IS NULL"#
        ))
        .await;
    assert_eq!(live, 0);
    let q: (i32, i32) = sqlx::query_as(&format!(
        r#"SELECT quantity, "lastBilledQuantity" FROM subscriptions WHERE id = {sid}"#
    ))
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!(q, (1, 1));
    let audited: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "auditLog" WHERE "companyId" = {c}
                 AND "actorName" = 'unpaid-prorated-users-expiration'"#
        ))
        .await;
    assert!(audited >= 1, "audit userCompany/divisionMember");

    // Jalan ulang: invoice sudah EXPIRED → tidak ada yang berubah.
    let res = proration_expiry::run(&cx, Some(c), false).await.unwrap();
    assert_eq!(res, json!({"success": true, "expired": 0, "details": []}));
}

// ───────────────────────────── subscription expiration ─────────────────────────────

#[tokio::test]
async fn subscription_expiration_stages() {
    let _g = SERIAL.lock().await;
    let Some(app) = app(&[]).await else { return };
    let mock = paypal_mock().await;
    ensure_free_tier(&app).await;
    let tr = tier(&app, 6.0, "MONTH", "FULL").await;

    // D-1: autoRenew=false, berakhir 12 jam lagi → batal PayPal + INACTIVE + turun ke Free.
    let c1 = company(&app, false, false).await;
    let s1 = sub(
        &app,
        c1,
        tr,
        &[
            ("status", s("ACTIVE")),
            ("autoRenew", "false".into()),
            ("pricePerUser", "6".into()),
            ("paypalSubscriptionId", s("I-D1")),
            ("paypalStatus", s("ACTIVE")),
            ("endDate", at(12)),
        ],
    )
    .await;
    let cx = ctx(&app, &mock, t0(), expiration::ACTOR);
    let res = expiration::run(&cx, Some(c1)).await.unwrap();
    assert_eq!(res["d1Expired"], json!(1));
    assert_eq!(res["d0Expired"], json!(0));
    assert_eq!(res["downgradedToFree"], json!(1));
    assert_eq!(
        res["details"][0],
        json!({"subscriptionId": s1, "companyId": c1, "stage": "D-1",
               "message": "Marked INACTIVE at D-1 (endDate: Fri Jun 15 2001 14:00:00 GMT+0000 (Coordinated Universal Time))"})
    );
    assert_eq!(res["details"][1]["stage"], json!("AUTO-DOWNGRADE-FREE"));
    let cancel = mock.requests_to("POST", "/v1/billing/subscriptions/I-D1/cancel");
    assert_eq!(
        cancel[0].json(),
        json!({"reason": "Subscription expiring — auto-renewal disabled"})
    );
    let tier_name = text(
        &app,
        &format!(r#"SELECT pt.name FROM subscriptions s JOIN "pricingTiers" pt ON pt.id = s."pricingTierId" WHERE s.id = {s1}"#),
    )
    .await;
    assert!(tier_name.eq_ignore_ascii_case("free"));

    // D-0 (autoRenew=true, sudah lewat) + MANUAL D-0 + TRIAL di company lain.
    let c2 = company(&app, true, false).await;
    let s0 = sub(
        &app,
        c2,
        tr,
        &[
            ("status", s("ACTIVE")),
            ("pricePerUser", "6".into()),
            ("paypalSubscriptionId", s("I-D0")),
            ("paypalStatus", s("CANCELLED")),
            ("endDate", at(-1)),
        ],
    )
    .await;
    let sm = sub(
        &app,
        c2,
        tr,
        &[
            ("billingSource", s("MANUAL")),
            ("status", s("ACTIVE")),
            ("pricePerUser", "6".into()),
            ("endDate", at(-1)),
        ],
    )
    .await;
    let st = sub(&app, c2, tr, &[("status", s("TRIAL")), ("endDate", at(-3))]).await;
    // Pending plan change → dikecualikan dari D-0.
    let sp = sub(
        &app,
        c2,
        tr,
        &[
            ("status", s("ACTIVE")),
            ("pricePerUser", "6".into()),
            ("pendingPaypalSubscriptionId", s("I-PEND")),
            ("endDate", at(-1)),
        ],
    )
    .await;
    mock.clear_log();
    let cx = ctx(&app, &mock, t0(), expiration::ACTOR);
    let res = expiration::run(&cx, Some(c2)).await.unwrap();
    assert_eq!(res["d0Expired"], json!(1));
    assert_eq!(res["manualExpired"], json!(1));
    let stages: Vec<(i64, String)> = res["details"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| {
            (
                d["subscriptionId"].as_i64().unwrap(),
                d["stage"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert!(stages.contains(&(i64::from(s0), "D-0".into())));
    assert!(stages.contains(&(i64::from(st), "TRIAL-EXPIRED".into())));
    // PayPal CANCELLED → tidak dibatalkan di tahap D-0 (auto-downgrade tetap membatalkan
    // `paypalSubscriptionId` dengan alasan "Switched to free plan", seperti activateFreeTier Node).
    let reasons: Vec<Value> = mock
        .requests_to("POST", "/v1/billing/subscriptions/I-D0/cancel")
        .iter()
        .map(|r| r.json()["reason"].clone())
        .collect();
    assert_eq!(reasons, [json!("Switched to free plan")]);
    let status = |id: i32| format!("SELECT status::text FROM subscriptions WHERE id = {id}");
    assert_eq!(text(&app, &status(sm)).await, "INACTIVE");
    assert_eq!(text(&app, &status(sp)).await, "ACTIVE");
    // MANUAL INACTIVE tidak diturunkan ke Free.
    assert!(!stages.contains(&(i64::from(sm), "AUTO-DOWNGRADE-FREE".into())));
    let is_trial: bool = app
        .scalar(&format!(r#"SELECT "isTrial" FROM company WHERE id = {c2}"#))
        .await;
    assert!(!is_trial);
    let audited: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "auditLog" WHERE "entityType" = 'company' AND "entityId" = {c2}
                 AND "actorName" = 'subscription-expiration' AND 'isTrial' = ANY("changedFields")"#
        ))
        .await;
    assert!(audited >= 1);

    // Jalan ulang: tidak ada lagi yang kedaluwarsa / diturunkan.
    let res = expiration::run(&cx, Some(c2)).await.unwrap();
    assert_eq!(
        (
            res["d1Expired"].clone(),
            res["d0Expired"].clone(),
            res["manualExpired"].clone(),
            res["downgradedToFree"].clone()
        ),
        (json!(0), json!(0), json!(0), json!(0))
    );
}

#[tokio::test]
async fn free_plan_expiry_when_configured() {
    let _g = SERIAL.lock().await;
    let Some(app) = app(&[]).await else { return };
    let mock = paypal_mock().await;
    ensure_free_tier(&app).await;
    let free: i32 = app
        .scalar(r#"SELECT id FROM "pricingTiers" WHERE name ILIKE 'free' AND "productType" = 'FULL' AND status = 1 ORDER BY id LIMIT 1"#)
        .await;
    let c = company(&app, false, false).await;
    let sid = sub(
        &app,
        c,
        free,
        &[
            ("status", s("ACTIVE")),
            ("pricePerUser", "0".into()),
            ("endDate", at(-24 * 4)),
        ],
    )
    .await;

    // Tanpa FREE_SUBSCRIPTION_DAYS: Free berlaku selamanya.
    let cx = ctx(&app, &mock, t0(), expiration::ACTOR);
    expiration::run(&cx, Some(c)).await.unwrap();
    assert_eq!(
        text(
            &app,
            &format!("SELECT status::text FROM subscriptions WHERE id = {sid}")
        )
        .await,
        "ACTIVE"
    );

    let mut cx = ctx(&app, &mock, t0(), expiration::ACTOR);
    let mut cfg = (*cx.state.config).clone();
    cfg.free_subscription_days = Some(14);
    cfg.free_subscription_grace_days = 3;
    cx.state.config = std::sync::Arc::new(cfg);
    let res = expiration::run(&cx, Some(c)).await.unwrap();
    assert_eq!(res["details"][0]["stage"], json!("FREE-EXPIRED"));
    assert_eq!(
        res["details"][0]["message"],
        json!(
            "Free plan expired — marked INACTIVE (endDate: Mon Jun 11 2001 02:00:00 GMT+0000 (Coordinated Universal Time))"
        )
    );
    // Sudah di tier Free → tidak ada auto-downgrade.
    assert_eq!(res["downgradedToFree"], json!(0));
}

// ───────────────────────────── pending plan change (force) ─────────────────────────────

#[tokio::test]
async fn pending_plan_change_force() {
    let _g = SERIAL.lock().await;
    let Some(app) = app(&[]).await else { return };
    let mock = paypal_mock().await;
    let old = tier(&app, 5.0, "MONTH", "FULL").await;
    let new = tier(&app, 8.0, "YEAR", "FULL").await;
    let new_name = text(
        &app,
        &format!(r#"SELECT name FROM "pricingTiers" WHERE id = {new}"#),
    )
    .await;
    let c = company(&app, false, false).await;
    let s1 = sub(
        &app,
        c,
        old,
        &[
            ("status", s("ACTIVE")),
            ("quantity", "2".into()),
            ("pendingPricingTierId", new.to_string()),
            ("pendingPaypalSubscriptionId", s("I-NEW")),
            ("pendingPaypalPlanId", s("P-NEW")),
        ],
    )
    .await;
    let unpaid: i32 = app
        .scalar(&format!(
            r#"INSERT INTO invoices ("subscriptionId", "invoiceNumber", "paymentUrl", status)
               VALUES ({s1}, 'INV-U', '', 'UNPAID') RETURNING id"#
        ))
        .await;

    let cx = ctx(&app, &mock, t0(), plan_change::ACTOR);
    let res = plan_change::run(&cx, Some(c)).await.unwrap();
    assert_eq!(
        res,
        json!({"success": true, "processed": 1, "skipped": 0, "details": [{
            "subscriptionId": s1, "companyId": c, "status": "processed",
            "message": format!("Plan change applied — new tier: {new_name}, endDate: Sat Jun 15 2002 02:00:00 GMT+0000 (Coordinated Universal Time)")
        }]})
    );
    let row: (String, String, i32, f64, String, NaiveDateTime, NaiveDateTime, i32, Option<i32>) = sqlx::query_as(&format!(
        r#"SELECT "paypalSubscriptionId", "paypalPlanId", "pricingTierId", "pricePerUser", "billingInterval",
                  "endDate", "nextInvoiceDate", "lastBilledQuantity", "pendingPricingTierId"
             FROM subscriptions WHERE id = {s1}"#
    ))
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!(
        (row.0.as_str(), row.1.as_str(), row.2, row.3, row.4.as_str()),
        ("I-NEW", "P-NEW", new, 8.0, "YEAR")
    );
    assert_eq!(row.5, t0() + Duration::days(365));
    assert_eq!(row.6.format("%Y-%m-%d").to_string(), "2001-07-01");
    assert_eq!((row.7, row.8), (2, None));
    let inv: (String, Option<String>) = sqlx::query_as(&format!(
        r#"SELECT status::text, "paypalPaymentId" FROM invoices WHERE id = {unpaid}"#
    ))
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!(inv.0, "PAID");
    assert_eq!(
        inv.1,
        Some(format!("FORCE-ADMIN-{}", t0().and_utc().timestamp_millis()))
    );
    let max_users: i32 = app
        .scalar(&format!(r#"SELECT "maxUsers" FROM company WHERE id = {c}"#))
        .await;
    assert_eq!(max_users, 50);

    // Tanpa invoice UNPAID → invoice PAID pengganti; tier tertunda yang hilang → dilewati.
    let s2 = sub(
        &app,
        c,
        old,
        &[
            ("status", s("ACTIVE")),
            ("quantity", "0".into()),
            ("pendingPricingTierId", new.to_string()),
            ("pendingPaypalSubscriptionId", s("I-NEW2")),
        ],
    )
    .await;
    let s3 = sub(
        &app,
        c,
        old,
        &[
            ("status", s("ACTIVE")),
            ("pendingPricingTierId", "2147483000".into()),
            ("pendingPaypalSubscriptionId", s("I-NEW3")),
        ],
    )
    .await;
    let res = plan_change::run(&cx, Some(c)).await.unwrap();
    assert_eq!(
        (res["processed"].clone(), res["skipped"].clone()),
        (json!(1), json!(1))
    );
    let skipped = res["details"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["subscriptionId"] == json!(s3))
        .unwrap();
    assert_eq!(
        skipped["message"],
        json!("Pending tier 2147483000 not found — skipped")
    );
    let fb: (String, f64, String) = sqlx::query_as(&format!(
        r#"SELECT i.status::text, i."totalPrice", it.description FROM invoices i
             JOIN "invoiceItems" it ON it."invoiceId" = i.id WHERE i."subscriptionId" = {s2}"#
    ))
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!(
        fb,
        (
            "PAID".to_owned(),
            96.0,
            format!(
                "Yearly Subscription (plan change to {new_name}) - 1 user(s) @ $8/user/month (billed yearly)"
            )
        )
    );
}
