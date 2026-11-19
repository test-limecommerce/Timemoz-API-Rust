//! Uji integrasi pustaka billing (free tier, auto-downgrade, `assertNotManualBilling`,
//! `cleanupCompressedScreenshots`, `generateInvoicePdf`) terhadap Postgres nyata + emulator GCS
//! di proses uji. PayPal diganti pencatat tiruan (tidak ada panggilan jaringan). Jalankan:
//! `MZ_TEST_DATABASE_URL=... MZ_TEST_REQUIRE_DB=1 cargo test -p mz-billing --test it_billing_core`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::future::Future;
use std::sync::Mutex;

use axum::Router;
use common::{BUCKET, start_emulator};
use mz_audit::{ActorType, AuditHandle};
use mz_billing::{
    FreeTier, FreeTierGuard, FreeTierOutcome, FreeTierSubscription, PaypalSubscriptionCanceller,
    activate_free_tier, assert_not_manual_billing, auto_downgrade_to_free,
    cleanup_compressed_screenshots, generate_invoice_pdf_response, resolve_free_tier_for,
};
use mz_core::AppState;
use mz_core::enums::SubscriptionStatus;
use mz_testkit::TestApp;
use serde_json::json;

fn build(_state: &AppState) -> Router {
    Router::new()
}

fn uniq() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// Pembatal PayPal tiruan: mencatat panggilan; `fail` → galat (harus hanya di-log).
#[derive(Default)]
struct Rec {
    calls: Mutex<Vec<(String, String)>>,
    fail: bool,
}

impl PaypalSubscriptionCanceller for Rec {
    fn cancel_subscription(
        &self,
        id: &str,
        reason: &str,
    ) -> impl Future<Output = Result<(), String>> + Send {
        self.calls
            .lock()
            .unwrap()
            .push((id.to_owned(), reason.to_owned()));
        let fail = self.fail;
        async move {
            if fail {
                Err("PayPal down".to_owned())
            } else {
                Ok(())
            }
        }
    }
}

fn audit(app: &TestApp) -> AuditHandle {
    AuditHandle::run_as(app.db().clone(), ActorType::System, Some("it-billing"))
}

/// Tier Free ber-`productType` FULL (dibuat bila template belum punya).
async fn free_tier(app: &TestApp) -> FreeTier {
    if let Some(t) = resolve_free_tier_for(app.db(), "FULL").await.unwrap() {
        return t;
    }
    app.exec(
        r#"INSERT INTO "pricingTiers" (name, status, "productType", "maxUsers", "maxProject",
               "screenshotResolution", "screenshotRetentionDays", "reportExportsPerMonth", "dataHistoryMonths")
           VALUES ('Free', 1, 'FULL', 3, 2, '640x360', 7, 3, 3)"#,
    )
    .await;
    resolve_free_tier_for(app.db(), "FULL")
        .await
        .unwrap()
        .expect("tier Free")
}

async fn paid_tier(app: &TestApp) -> i32 {
    app.scalar(&format!(
        r#"INSERT INTO "pricingTiers" (name, status, "productType", "maxUsers", "maxProject",
               "screenshotResolution", "screenshotRetentionDays", "reportExportsPerMonth", "dataHistoryMonths")
           VALUES ('Paid-{}', 1, 'FULL', 99, 50, '1920x1080', 90, 100, 24) RETURNING id"#,
        uniq()
    ))
    .await
}

async fn company(app: &TestApp) -> i32 {
    app.scalar(&format!(
        r#"INSERT INTO company (name, status, "isTrial", "maxUsers", "maxProject", "screenshotResolution",
               "screenshotRetentionDays", "reportExportsPerMonth", "reportExportsUsedThisMonth", "dataHistoryMonths")
           VALUES ('it-billing-{}', 1, true, 99, 50, '1920x1080', 90, 100, 7, 24) RETURNING id"#,
        uniq()
    ))
    .await
}

struct Sub {
    id: i32,
    company_id: i32,
    invoice_id: i32,
    paid_tier: i32,
}

/// Subscription `PAYPAL`/`status` di tier berbayar + invoice UNPAID.
async fn subscription(app: &TestApp, source: &str, status: &str) -> Sub {
    let company_id = company(app).await;
    let paid_tier = paid_tier(app).await;
    let id: i32 = app
        .scalar(&format!(
            r#"INSERT INTO subscriptions ("companyId", "pricingTierId", "startDate", "endDate", status,
                   "billingSource", "pricePerUser", "paypalSubscriptionId", "pendingPaypalSubscriptionId",
                   "paypalPlanId", "autoRenew", "nextInvoiceDate", "billingInterval")
               VALUES ({company_id}, {paid_tier}, '2026-10-01', '2026-11-01', '{status}'::"SubscriptionStatus",
                   '{source}'::"SubscriptionBillingSource", 5, 'I-ACTIVE', 'I-PENDING', 'P-1', true,
                   '2026-11-01', 'YEAR') RETURNING id"#
        ))
        .await;
    let invoice_id: i32 = app
        .scalar(&format!(
            r#"INSERT INTO invoices ("subscriptionId", "invoiceNumber", "paymentUrl", status)
               VALUES ({id}, 'INV-IT-{}', '', 'UNPAID') RETURNING id"#,
            uniq()
        ))
        .await;
    Sub {
        id,
        company_id,
        invoice_id,
        paid_tier,
    }
}

#[derive(Debug, PartialEq, sqlx::FromRow)]
struct SubRow {
    status: String,
    #[sqlx(rename = "billingSource")]
    billing_source: String,
    #[sqlx(rename = "pricingTierId")]
    tier: i32,
    #[sqlx(rename = "pricePerUser")]
    price: f64,
    #[sqlx(rename = "paypalSubscriptionId")]
    paypal: Option<String>,
    #[sqlx(rename = "pendingPaypalSubscriptionId")]
    pending: Option<String>,
    #[sqlx(rename = "paypalPlanId")]
    plan: Option<String>,
    #[sqlx(rename = "autoRenew")]
    auto_renew: bool,
    #[sqlx(rename = "nextInvoiceDate")]
    next: Option<chrono::NaiveDateTime>,
    #[sqlx(rename = "billingInterval")]
    interval: String,
    #[sqlx(rename = "endDate")]
    end: chrono::NaiveDateTime,
}

async fn sub_row(app: &TestApp, id: i32) -> SubRow {
    sqlx::query_as(
        r#"SELECT status::text AS status, "billingSource"::text AS "billingSource", "pricingTierId", "pricePerUser",
                  "paypalSubscriptionId", "pendingPaypalSubscriptionId", "paypalPlanId", "autoRenew",
                  "nextInvoiceDate", "billingInterval", "endDate"
             FROM subscriptions WHERE id = $1"#,
    )
    .bind(id)
    .fetch_one(app.db())
    .await
    .unwrap()
}

#[derive(Debug, PartialEq, sqlx::FromRow)]
struct CompanyRow {
    #[sqlx(rename = "isTrial")]
    trial: bool,
    #[sqlx(rename = "maxUsers")]
    max_users: i32,
    #[sqlx(rename = "maxProject")]
    max_project: i32,
    #[sqlx(rename = "screenshotResolution")]
    resolution: String,
    #[sqlx(rename = "screenshotRetentionDays")]
    retention: i32,
    #[sqlx(rename = "reportExportsPerMonth")]
    exports: i32,
    #[sqlx(rename = "reportExportsUsedThisMonth")]
    used: i32,
    #[sqlx(rename = "dataHistoryMonths")]
    history: i32,
}

async fn company_row(app: &TestApp, id: i32) -> CompanyRow {
    sqlx::query_as(
        r#"SELECT "isTrial", "maxUsers", "maxProject", "screenshotResolution", "screenshotRetentionDays",
                  "reportExportsPerMonth", "reportExportsUsedThisMonth", "dataHistoryMonths"
             FROM company WHERE id = $1"#,
    )
    .bind(id)
    .fetch_one(app.db())
    .await
    .unwrap()
}

async fn invoice_status(app: &TestApp, id: i32) -> String {
    app.scalar(&format!(
        "SELECT status::text FROM invoices WHERE id = {id}"
    ))
    .await
}

#[tokio::test]
async fn auto_downgrade_menurunkan_inactive_paypal_ke_free() {
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let free = free_tier(&app).await;
    let s = subscription(&app, "PAYPAL", "INACTIVE").await;
    let rec = Rec {
        fail: true, // galat PayPal tidak boleh menghentikan downgrade
        ..Rec::default()
    };

    let done = auto_downgrade_to_free(app.state(), &audit(&app), &rec, s.id)
        .await
        .unwrap();
    assert!(done);
    assert_eq!(
        rec.calls.lock().unwrap().clone(),
        vec![
            ("I-ACTIVE".to_owned(), "Switched to free plan".to_owned()),
            ("I-PENDING".to_owned(), "Switched to free plan".to_owned()),
        ]
    );

    let row = sub_row(&app, s.id).await;
    assert_eq!(row.status, "ACTIVE");
    assert_eq!(row.billing_source, "PAYPAL");
    assert_eq!(row.tier, free.id);
    assert_eq!(row.price, 0.0);
    assert_eq!((row.paypal, row.pending, row.plan), (None, None, None));
    assert!(!row.auto_renew);
    assert_eq!(row.next, None);
    assert_eq!(row.interval, "MONTH");
    // FREE_SUBSCRIPTION_DAYS tidak diset di uji → Free tanpa batas (2099-12-31).
    assert_eq!(row.end.to_string(), "2099-12-31 00:00:00");
    assert_eq!(invoice_status(&app, s.invoice_id).await, "CANCELLED");

    let c = company_row(&app, s.company_id).await;
    assert_eq!(
        c,
        CompanyRow {
            trial: false,
            max_users: free.max_users,
            max_project: free.max_project,
            resolution: free.screenshot_resolution.clone(),
            retention: free.screenshot_retention_days,
            exports: free.report_exports_per_month,
            used: 0,
            history: free.data_history_months,
        }
    );

    // Sudah di tier Free → tidak ada yang diturunkan.
    let again = auto_downgrade_to_free(app.state(), &audit(&app), &rec, s.id)
        .await
        .unwrap();
    assert!(!again);
}

#[tokio::test]
async fn auto_downgrade_dilewati_untuk_manual_dan_bukan_inactive() {
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let _ = free_tier(&app).await;
    for (source, status) in [("MANUAL", "INACTIVE"), ("PAYPAL", "ACTIVE")] {
        let s = subscription(&app, source, status).await;
        let rec = Rec::default();
        let done = auto_downgrade_to_free(app.state(), &audit(&app), &rec, s.id)
            .await
            .unwrap();
        assert!(!done, "{source}/{status}");
        assert!(rec.calls.lock().unwrap().is_empty());
        let row = sub_row(&app, s.id).await;
        assert_eq!((row.status.as_str(), row.tier), (status, s.paid_tier));
        assert_eq!(invoice_status(&app, s.invoice_id).await, "UNPAID");
        assert!(company_row(&app, s.company_id).await.trial);
    }
    // Subscription tak ada.
    let rec = Rec::default();
    assert!(
        !auto_downgrade_to_free(app.state(), &audit(&app), &rec, i32::MAX)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn activate_free_tier_guard_tak_cocok_dilewati_tanpa_menyentuh_data() {
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let free = free_tier(&app).await;
    // Baris sudah dialihkan ke MANUAL di sela pembacaan & tulisan (TOCTOU).
    let s = subscription(&app, "MANUAL", "INACTIVE").await;
    let rec = Rec::default();
    let sub = FreeTierSubscription {
        id: s.id,
        paypal_subscription_id: Some("I-ACTIVE".into()),
        pending_paypal_subscription_id: None,
        status: "INACTIVE".into(),
        price_per_user: 5.0,
        end_date: None,
    };
    let out = activate_free_tier(
        app.state(),
        &audit(&app),
        &rec,
        &sub,
        &free,
        s.company_id,
        Some(FreeTierGuard {
            billing_source: "PAYPAL",
            status: SubscriptionStatus::INACTIVE,
        }),
    )
    .await
    .unwrap();
    assert_eq!(out, FreeTierOutcome::Skipped);
    assert_eq!(out.to_json()["skipped"], true);
    // Seperti Node: PayPal dibatalkan SEBELUM guard dicek.
    assert_eq!(rec.calls.lock().unwrap().len(), 1);
    let row = sub_row(&app, s.id).await;
    assert_eq!(
        (row.billing_source.as_str(), row.tier),
        ("MANUAL", s.paid_tier)
    );
    assert_eq!(invoice_status(&app, s.invoice_id).await, "UNPAID");
    assert!(company_row(&app, s.company_id).await.trial);
}

#[tokio::test]
async fn activate_free_tier_tanpa_guard_mempertahankan_end_date_yang_belum_berbayar() {
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let free = free_tier(&app).await;
    let s = subscription(&app, "PAYPAL", "PENDING").await;
    // PENDING dengan pricePerUser terisi = belum berbayar; endDate lama (2026-11-01) < 2099 → dipertahankan.
    let sub = FreeTierSubscription {
        id: s.id,
        paypal_subscription_id: None,
        pending_paypal_subscription_id: None,
        status: "PENDING".into(),
        price_per_user: 5.0,
        end_date: Some(
            chrono::NaiveDate::from_ymd_opt(2026, 11, 1)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap(),
        ),
    };
    let rec = Rec::default();
    let out = activate_free_tier(
        app.state(),
        &audit(&app),
        &rec,
        &sub,
        &free,
        s.company_id,
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        out.to_json(),
        json!({"success": true, "message": "Free plan activated successfully", "data": {"newPlan": free.name}})
    );
    assert!(rec.calls.lock().unwrap().is_empty());
    let row = sub_row(&app, s.id).await;
    assert_eq!(row.status, "ACTIVE");
    assert_eq!(row.end.to_string(), "2026-11-01 00:00:00");
    assert_eq!(invoice_status(&app, s.invoice_id).await, "CANCELLED");
    assert!(!company_row(&app, s.company_id).await.trial);
}

#[tokio::test]
async fn assert_not_manual_billing_menolak_manual_yang_berjalan() {
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let tier = paid_tier(&app).await;
    let mk = |company: i32, source: &'static str, status: &'static str, end: &'static str| {
        format!(
            r#"INSERT INTO subscriptions ("companyId", "pricingTierId", "startDate", "endDate", status, "billingSource")
               VALUES ({company}, {tier}, '2026-01-01', '{end}', '{status}'::"SubscriptionStatus",
                       '{source}'::"SubscriptionBillingSource")"#
        )
    };
    // Tanpa subscription → lolos.
    let c0 = company(&app).await;
    assert_not_manual_billing(app.db(), c0).await.unwrap();

    // MANUAL + ACTIVE + belum berakhir → 409 MANUAL_BILLING.
    let c1 = company(&app).await;
    app.exec(&mk(c1, "MANUAL", "ACTIVE", "2099-01-01")).await;
    let err = assert_not_manual_billing(app.db(), c1).await.unwrap_err();
    let resp = axum::response::IntoResponse::into_response(err);
    assert_eq!(resp.status(), 409);
    let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        body,
        json!({"code": "MANUAL_BILLING", "message": "Your subscription is managed by the Timemoz team."})
    );

    // Juga lewat transaksi.
    let mut tx = app.db().begin().await.unwrap();
    assert!(assert_not_manual_billing(&mut *tx, c1).await.is_err());
    tx.rollback().await.unwrap();

    // MANUAL sudah berakhir / PAYPAL aktif → lolos.
    let c2 = company(&app).await;
    app.exec(&mk(c2, "MANUAL", "ACTIVE", "2020-01-01")).await;
    assert_not_manual_billing(app.db(), c2).await.unwrap();
    let c3 = company(&app).await;
    app.exec(&mk(c3, "PAYPAL", "ACTIVE", "2099-01-01")).await;
    assert_not_manual_billing(app.db(), c3).await.unwrap();

    // Baris terkini (startDate desc) yang menentukan: MANUAL lama + PAYPAL baru → lolos.
    let c4 = company(&app).await;
    app.exec(&mk(c4, "MANUAL", "ACTIVE", "2099-01-01")).await;
    app.exec(&format!(
        r#"INSERT INTO subscriptions ("companyId", "pricingTierId", "startDate", "endDate", status, "billingSource")
           VALUES ({c4}, {tier}, '2026-06-01', '2099-01-01', 'ACTIVE'::"SubscriptionStatus", 'PAYPAL'::"SubscriptionBillingSource")"#
    ))
    .await;
    assert_not_manual_billing(app.db(), c4).await.unwrap();
}

/// project + timeline milik staff fixture (pola `mz-timeconfig` it_admin_screenshots).
async fn timeline_of(app: &TestApp, staff: i32, company_id: i32) -> (i32, i32) {
    let u = uniq();
    let p: i32 = app
        .scalar(&format!(
            r#"INSERT INTO project (code, name, "screenshotPerHour", "projectTypeId", "startDate", status)
               VALUES ('it{u}', 'it{u}', 3, (SELECT id FROM "projectType" ORDER BY id LIMIT 1), CURRENT_DATE, 1)
               RETURNING id"#
        ))
        .await;
    app.exec(&format!(
        r#"INSERT INTO "projectCompany" ("projectId", "companyId", status) VALUES ({p}, {company_id}, 1)"#
    ))
    .await;
    let t: i32 = app
        .scalar(&format!(
            r#"INSERT INTO tracking ("takenDate", "userId", status) VALUES (CURRENT_DATE - 400 - {}, {staff}, 1)
               RETURNING id"#,
            u.len()
        ))
        .await;
    let tl: i32 = app
        .scalar(&format!(
            r#"INSERT INTO "trackingTimeline" (note, "startAt", "endAt", "trackingId", "projectId", status)
               VALUES ('it', NOW(), NOW(), {t}, {p}, 1) RETURNING id"#
        ))
        .await;
    (p, tl)
}

#[tokio::test]
async fn cleanup_compressed_screenshots_hapus_file_dan_kosongkan_kolom() {
    let em = start_emulator().await;
    let vars = [
        ("STORAGE_EMULATOR_HOST", em.host.as_str()),
        ("GCP_BUCKET_NAME", BUCKET),
    ];
    let Some(app) = TestApp::with_vars(build, &vars).await else {
        return;
    };
    let c = app.company().await;
    let other = app.other_company_admin().await;
    let (p1, tl1) = timeline_of(&app, c.staff.id, c.id).await;
    let (p2, tl2) = timeline_of(&app, other.admin.id, other.id).await;
    let shot = |p: i32, tl: i32, key: &str, compressed: bool| {
        format!(
            r#"INSERT INTO screenshot ("fileFullpath", "takenAt", "trackingTimelineId", "projectId", status,
                   "isCompressed", "compressedFileKey", "compressedAt")
               VALUES ('orig', NOW(), {tl}, {p}, 1, {compressed}, {}, {}) RETURNING id"#,
            if compressed {
                format!("'{key}'")
            } else {
                "NULL".into()
            },
            if compressed { "NOW()" } else { "NULL" }
        )
    };
    let k1 = format!("compressed/{}-1.jpg", uniq());
    let k2 = format!("compressed/{}-2.jpg", uniq());
    let ko = format!("compressed/{}-other.jpg", uniq());
    let s1: i32 = app.scalar(&shot(p1, tl1, &k1, true)).await;
    let s2: i32 = app.scalar(&shot(p1, tl1, &k2, true)).await; // file sudah hilang di bucket
    let s3: i32 = app.scalar(&shot(p1, tl1, "", false)).await; // tidak terkompresi
    let so: i32 = app.scalar(&shot(p2, tl2, &ko, true)).await; // company lain
    em.put(&k1, b"x");
    em.put(&ko, b"y");

    // Fixture company membawa data template (screenshot terkompresi lain): hitung dulu.
    let before: i64 = app
        .scalar(&format!(
            r#"SELECT count(*) FROM screenshot s WHERE s."isCompressed" = true AND s."compressedFileKey" IS NOT NULL
                 AND EXISTS (SELECT 1 FROM "trackingTimeline" tt JOIN tracking t ON t.id = tt."trackingId"
                             JOIN "userCompany" uc ON uc."userId" = t."userId"
                             WHERE tt.id = s."trackingTimelineId" AND uc."companyId" = {})"#,
            c.id
        ))
        .await;
    assert!(before >= 2);
    let n = cleanup_compressed_screenshots(app.state(), c.id).await;
    assert_eq!(n as i64, before);
    // Hanya file company ini yang dihapus; kegagalan hapus k2 (404) tidak menghentikan apa pun.
    assert_eq!(em.keys(), vec![ko.clone()]);
    assert_eq!(em.deleted.lock().unwrap().clone(), vec![k1]);
    type R = (bool, Option<String>, Option<chrono::NaiveDateTime>);
    let get = |id: i32| {
        let db = app.db().clone();
        async move {
            sqlx::query_as::<_, R>(
                r#"SELECT "isCompressed", "compressedFileKey", "compressedAt" FROM screenshot WHERE id = $1"#,
            )
            .bind(id)
            .fetch_one(&db)
            .await
            .unwrap()
        }
    };
    assert_eq!(get(s1).await, (false, None, None));
    assert_eq!(get(s2).await, (false, None, None));
    assert_eq!(get(s3).await, (false, None, None));
    let other_row = get(so).await;
    assert!(other_row.0 && other_row.1.as_deref() == Some(ko.as_str()));

    // Tidak ada yang tersisa → 0, tanpa galat.
    assert_eq!(cleanup_compressed_screenshots(app.state(), c.id).await, 0);
}

#[tokio::test]
async fn generate_invoice_pdf_response_mengikuti_akses_company() {
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let c = app.company().await;
    let other = app.other_company_admin().await;
    let sub: i32 = app
        .scalar(&format!(
            r#"INSERT INTO subscriptions ("companyId", "pricingTierId", "startDate", "endDate", status)
               VALUES ({}, (SELECT MIN(id) FROM "pricingTiers"), '2026-11-01', '2026-12-01', 'ACTIVE') RETURNING id"#,
            c.id
        ))
        .await;
    let inv: String = app
        .scalar(&format!(
            r#"INSERT INTO invoices ("subscriptionId", "invoiceNumber", "paymentUrl") VALUES ({sub}, 'INV-IT-{}', '')
               RETURNING uuid::text"#,
            uniq()
        ))
        .await;
    let ok = generate_invoice_pdf_response(app.state(), &inv, c.admin.id).await;
    assert_eq!(
        ok,
        json!({"success": true, "message": "Success get invoice PDF url", "data": {"url": format!("/invoices/{inv}/pdf")}})
    );
    // SUPER_ADMIN company yang sama juga boleh; STAFF dan admin company lain dijawab "tidak ada".
    let nf = json!({"success": false, "message": "Invoice not found"});
    assert_eq!(
        generate_invoice_pdf_response(app.state(), &inv, c.staff.id).await,
        nf
    );
    assert_eq!(
        generate_invoice_pdf_response(app.state(), &inv, other.admin.id).await,
        nf
    );
    let missing = uuid::Uuid::new_v4().to_string();
    assert_eq!(
        generate_invoice_pdf_response(app.state(), &missing, c.admin.id).await,
        nf
    );
    // uuid rusak → galat Prisma ditangkap catch Node.
    let bad = generate_invoice_pdf_response(app.state(), "nope", c.admin.id).await;
    assert_eq!(bad["success"], false);
}
