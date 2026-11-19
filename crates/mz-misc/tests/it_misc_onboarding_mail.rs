//! Uji integrasi dua cron email onboarding terhadap Postgres nyata:
//! `quickstart_reminder::run_sweep` (port `quickstart-reminder.service.spec.ts`) dan
//! `sales_onboarding::run_sweep` (port `sales-onboarding-email.service.spec.ts`).
//!
//! Mailtrap TIDAK dihubungi: pengirim diganti tiruan (`FakeMail`) yang mencatat panggilan dan
//! mengembalikan hasil yang diskenariokan.
//! Jalankan: `MZ_TEST_DATABASE_URL=... MZ_TEST_REQUIRE_DB=1 cargo test -p mz-misc --test it_misc_onboarding_mail`.
//!
//! Kedua sweep memindai SELURUH database, jadi test berjalan berurutan (`SERIAL`) dan setiap
//! test menonaktifkan kandidat lain (`reminderEligible` / `salesOnboardingEmailEligible`) di
//! database test-nya sendiri sebelum menjalankan sweep.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::VecDeque;
use std::sync::Mutex as StdMutex;

use axum::Router;
use chrono::{Duration, NaiveDateTime};
use mz_audit::AuditHandle;
use mz_core::AppState;
use mz_core::time::now_naive;
use mz_misc::quickstart_reminder::{
    self, QUICKSTART_REMINDER_MAX_ATTEMPTS, Recipient, ReminderContent, ReminderMailer,
};
use mz_misc::sales_onboarding::{self, OnboardingMailer};
use mz_testkit::TestApp;
use serde_json::Value;
use tokio::sync::Mutex;

static SERIAL: Mutex<()> = Mutex::const_new(());

fn no_routes(_: &AppState) -> Router {
    Router::new()
}

fn uniq() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..10].to_owned()
}

// ───────────────────────── pengirim tiruan ─────────────────────────

/// Satu panggilan kirim: `(email, name, subject, paragraphs)`.
type Call = (String, String, String, Vec<String>);

/// Mencatat setiap panggilan; hasil diambil dari `script` (urut), lalu `default_ok`.
struct FakeMail {
    calls: StdMutex<Vec<Call>>,
    script: StdMutex<VecDeque<bool>>,
    default_ok: bool,
}

impl FakeMail {
    fn ok() -> Self {
        Self::with(&[], true)
    }

    fn failing() -> Self {
        Self::with(&[], false)
    }

    fn with(script: &[bool], default_ok: bool) -> Self {
        Self {
            calls: StdMutex::new(Vec::new()),
            script: StdMutex::new(script.iter().copied().collect()),
            default_ok,
        }
    }

    fn record(&self, call: Call) -> bool {
        self.calls.lock().unwrap().push(call);
        self.script
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(self.default_ok)
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    fn emails(&self) -> Vec<String> {
        self.calls().into_iter().map(|c| c.0).collect()
    }
}

impl ReminderMailer for FakeMail {
    async fn send_reminder(&self, to: &Recipient, content: &ReminderContent) -> bool {
        self.record((
            to.email.clone(),
            to.name.clone(),
            content.subject.to_owned(),
            content.paragraphs.clone(),
        ))
    }
}

impl OnboardingMailer for FakeMail {
    async fn send_onboarding(&self, email: &str, name: &str) -> bool {
        self.record((email.to_owned(), name.to_owned(), String::new(), Vec::new()))
    }
}

// ───────────────────────── data uji ─────────────────────────

async fn new_company(app: &TestApp, sales_tracker: bool, has_create_org: bool) -> i32 {
    let tag = uniq();
    app.scalar(&format!(
        r#"INSERT INTO company (name, status, "moduleSalesTrackerEnabled", "hasCreateOrgnanization")
           VALUES ('IT onboarding {tag}', 1, {sales_tracker}, {has_create_org}) RETURNING id"#
    ))
    .await
}

/// User baru; `(id, username)`.
async fn new_user(app: &TestApp, status: i32) -> (i32, String) {
    let tag = uniq();
    let username = format!("it_onb_{tag}@mailinator.com");
    let id: i32 = app
        .scalar(&format!(
            r#"INSERT INTO users (username, password, name, status)
               VALUES ('{username}', 'x', 'IT {tag} Onboarding', {status}) RETURNING id"#
        ))
        .await;
    (id, username)
}

/// Keanggotaan aktif + role bawaan (`accessLevel` global ber-`code`).
async fn member(app: &TestApp, user_id: i32, company_id: i32, role: &str, is_default: bool) {
    app.exec(&format!(
        r#"INSERT INTO "userCompany" ("userId", "companyId", status, "idleTimeInSecond", "isDefault")
           VALUES ({user_id}, {company_id}, 1, 300, {is_default});
           INSERT INTO "userAccessLevel" ("userId", "accessLevelId", "companyId", status)
           VALUES ({user_id}, (SELECT id FROM "accessLevel" WHERE code = '{role}' AND "companyId" IS NULL
                               ORDER BY id LIMIT 1), {company_id}, 1);"#
    ))
    .await;
}

/// Langganan baru dengan paket `productType` tertentu (paket dibuat sendiri).
async fn subscribe(app: &TestApp, company_id: i32, product_type: &str) {
    let tag = uniq();
    app.exec(&format!(
        r#"INSERT INTO "pricingTiers" (name, status, "productType")
           VALUES ('IT {tag}', 1, '{product_type}'::"PricingProductType");
           INSERT INTO subscriptions ("companyId", "pricingTierId", "startDate", "endDate", status, "maxUsers")
           VALUES ({company_id}, (SELECT id FROM "pricingTiers" WHERE name = 'IT {tag}'),
                   NOW() - interval '1 day', NOW() + interval '1 year', 'ACTIVE'::"SubscriptionStatus", 10);"#
    ))
    .await;
}

/// Baris `companyQuickstart` layak pengingat. `created` & `stamps` = ekspresi SQL.
async fn quickstart(app: &TestApp, company_id: i32, created: &str, stamps: &[(&str, &str)]) {
    let mut cols = vec![
        r#""companyId""#.to_owned(),
        r#""reminderEligible""#.to_owned(),
        r#""createdAt""#.to_owned(),
        r#""updatedAt""#.to_owned(),
    ];
    let mut vals = vec![
        company_id.to_string(),
        "true".to_owned(),
        created.to_owned(),
        "NOW()".to_owned(),
    ];
    for (c, v) in stamps {
        cols.push(format!(r#""{c}""#));
        vals.push((*v).to_owned());
    }
    app.exec(&format!(
        r#"INSERT INTO "companyQuickstart" ({}) VALUES ({})"#,
        cols.join(", "),
        vals.join(", ")
    ))
    .await;
}

/// Company Sales Tracker berpaket SALES dengan owner, admin, dan staff; `(company, [owner, admin], staff)`.
async fn sales_company(app: &TestApp) -> (i32, Vec<String>, String) {
    let company = new_company(app, true, false).await;
    subscribe(app, company, "SALES").await;
    let owner = new_user(app, 1).await;
    let admin = new_user(app, 1).await;
    let staff = new_user(app, 1).await;
    member(app, owner.0, company, "SUPER_ADMIN", true).await;
    member(app, admin.0, company, "ADMIN", true).await;
    member(app, staff.0, company, "STAFF", true).await;
    (company, vec![owner.1, admin.1], staff.1)
}

/// Hanya company `keep` yang layak pengingat di database test ini.
async fn only_quickstart(app: &TestApp, keep: &[i32]) {
    let ids = keep
        .iter()
        .map(i32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    app.exec(&format!(
        r#"UPDATE "companyQuickstart" SET "reminderEligible" = false
           WHERE "reminderEligible" AND "companyId" <> ALL(ARRAY[{ids}]::int[])"#
    ))
    .await;
}

/// `(status, attempts, sentAt IS NOT NULL, lastError, recipients)` jejak satu jenis.
async fn email_row(
    app: &TestApp,
    company_id: i32,
    kind: &str,
) -> Option<(String, i32, bool, Option<String>, Option<Value>)> {
    sqlx::query_as(
        r#"SELECT status, attempts, "sentAt" IS NOT NULL, "lastError", recipients
           FROM "companyQuickstartEmail"
           WHERE "companyId" = $1 AND type = $2::"QuickstartReminderType""#,
    )
    .bind(company_id)
    .bind(kind)
    .fetch_optional(app.db())
    .await
    .unwrap()
}

async fn email_rows(app: &TestApp, company_id: i32) -> i64 {
    app.scalar(&format!(
        r#"SELECT COUNT(*) FROM "companyQuickstartEmail" WHERE "companyId" = {company_id}"#
    ))
    .await
}

fn successes(recipients: &Option<Value>) -> Vec<bool> {
    recipients
        .as_ref()
        .and_then(Value::as_array)
        .map(|a| a.iter().map(|r| r["success"] == true).collect())
        .unwrap_or_default()
}

// ───────────────────────── quickstart reminder ─────────────────────────

/// spec: "mengevaluasi stempel dulu, lalu mengirim ke owner & admin dan menandai SENT" +
/// "klaim kosong (sudah terkirim): tidak mengirim".
#[tokio::test]
async fn reminder_sent_to_owner_and_admin_once() {
    let Some(app) = TestApp::new(no_routes).await else {
        return;
    };
    let _g = SERIAL.lock().await;
    let (company, admins, staff) = sales_company(&app).await;
    quickstart(&app, company, "NOW() - interval '25 hours'", &[]).await;
    only_quickstart(&app, &[company]).await;

    let mail = FakeMail::ok();
    let r = quickstart_reminder::run_sweep(app.db(), &mail)
        .await
        .unwrap();
    assert_eq!((r.sent, r.failed), (1, 0));
    assert_eq!(mail.emails(), admins, "owner & admin saja, urut id");
    assert!(!mail.emails().contains(&staff));
    assert!(
        mail.calls()
            .iter()
            .all(|c| c.2 == "Company Setting Reminder")
    );

    let (status, attempts, sent_at, last_error, recipients) =
        email_row(&app, company, "COMPANY_SETTING").await.unwrap();
    assert_eq!((status.as_str(), attempts, sent_at), ("SENT", 1, true));
    assert_eq!(last_error, None);
    assert_eq!(successes(&recipients), [true, true]);
    assert_eq!(recipients.unwrap()[0]["email"], admins[0].as_str());

    // `evaluate` jalan dulu: 3 anggota → INVITE_TEAM ter-stempel.
    let invited: bool = app
        .scalar(&format!(
            r#"SELECT "inviteTeamAt" IS NOT NULL FROM "companyQuickstart" WHERE "companyId" = {company}"#
        ))
        .await;
    assert!(invited);

    // Tick berikutnya: jenis yang sama sudah SENT → klaim kosong, tidak mengirim.
    let mail = FakeMail::ok();
    let r = quickstart_reminder::run_sweep(app.db(), &mail)
        .await
        .unwrap();
    assert_eq!((r.sent, r.failed), (0, 0));
    assert!(mail.calls().is_empty());
    assert_eq!(email_rows(&app, company).await, 1);
}

/// spec: "belum jatuh tempo: tidak mencoba klaim".
#[tokio::test]
async fn reminder_not_due_is_not_claimed() {
    let Some(app) = TestApp::new(no_routes).await else {
        return;
    };
    let _g = SERIAL.lock().await;
    let (company, _, _) = sales_company(&app).await;
    quickstart(&app, company, "NOW() - interval '23 hours'", &[]).await;
    only_quickstart(&app, &[company]).await;

    let mail = FakeMail::ok();
    let r = quickstart_reminder::run_sweep(app.db(), &mail)
        .await
        .unwrap();
    assert_eq!((r.sent, r.failed), (0, 0));
    assert!(mail.calls().is_empty());
    assert_eq!(email_rows(&app, company).await, 0);
}

/// spec: "semua penerima gagal: FAILED supaya dicoba lagi tick berikutnya" — sampai
/// `QUICKSTART_REMINDER_MAX_ATTEMPTS`.
#[tokio::test]
async fn reminder_all_failed_retries_until_max_attempts() {
    let Some(app) = TestApp::new(no_routes).await else {
        return;
    };
    let _g = SERIAL.lock().await;
    let (company, _, _) = sales_company(&app).await;
    quickstart(&app, company, "NOW() - interval '25 hours'", &[]).await;
    only_quickstart(&app, &[company]).await;

    for attempt in 1..=QUICKSTART_REMINDER_MAX_ATTEMPTS {
        let mail = FakeMail::failing();
        let r = quickstart_reminder::run_sweep(app.db(), &mail)
            .await
            .unwrap();
        assert_eq!((r.sent, r.failed), (0, 1), "percobaan {attempt}");
        assert_eq!(mail.calls().len(), 2);
        let (status, attempts, sent_at, last_error, recipients) =
            email_row(&app, company, "COMPANY_SETTING").await.unwrap();
        assert_eq!(
            (status.as_str(), attempts, sent_at),
            ("FAILED", attempt, false)
        );
        assert_eq!(last_error.as_deref(), Some("all recipients failed"));
        assert_eq!(successes(&recipients), [false, false]);
    }

    // Batas percobaan tercapai: tidak diklaim lagi.
    let mail = FakeMail::ok();
    let r = quickstart_reminder::run_sweep(app.db(), &mail)
        .await
        .unwrap();
    assert_eq!((r.sent, r.failed), (0, 0));
    assert!(mail.calls().is_empty());
    let (status, attempts, ..) = email_row(&app, company, "COMPANY_SETTING").await.unwrap();
    assert_eq!(
        (status.as_str(), attempts),
        ("FAILED", QUICKSTART_REMINDER_MAX_ATTEMPTS)
    );
}

/// spec: "satu penerima berhasil sudah cukup: SENT, tidak diulang".
#[tokio::test]
async fn reminder_one_success_is_enough() {
    let Some(app) = TestApp::new(no_routes).await else {
        return;
    };
    let _g = SERIAL.lock().await;
    let (company, _, _) = sales_company(&app).await;
    quickstart(&app, company, "NOW() - interval '25 hours'", &[]).await;
    only_quickstart(&app, &[company]).await;

    let mail = FakeMail::with(&[false, true], true);
    let r = quickstart_reminder::run_sweep(app.db(), &mail)
        .await
        .unwrap();
    assert_eq!((r.sent, r.failed), (1, 0));
    let (status, _, sent_at, last_error, recipients) =
        email_row(&app, company, "COMPANY_SETTING").await.unwrap();
    assert_eq!((status.as_str(), sent_at), ("SENT", true));
    assert_eq!(last_error, None);
    assert_eq!(successes(&recipients), [false, true]);
}

/// spec: "VISIT_PLAN menyebut persentase progres di badan email".
#[tokio::test]
async fn reminder_visit_plan_mentions_progress() {
    let Some(app) = TestApp::new(no_routes).await else {
        return;
    };
    let _g = SERIAL.lock().await;
    let (company, _, _) = sales_company(&app).await;
    let past = "NOW() - interval '25 hours'";
    quickstart(
        &app,
        company,
        "NOW() - interval '30 hours'",
        &[
            ("inviteTeamAt", past),
            ("companySettingAt", past),
            ("manageRegionAt", past),
            ("manageCustomerAt", past),
            ("manageStoreAt", past),
        ],
    )
    .await;
    only_quickstart(&app, &[company]).await;

    let mail = FakeMail::ok();
    quickstart_reminder::run_sweep(app.db(), &mail)
        .await
        .unwrap();
    let calls = mail.calls();
    let first = calls.first().unwrap();
    assert_eq!(first.2, "Visit Plan Reminder");
    assert!(first.3[0].contains("71%"), "{:?}", first.3);
    assert!(email_row(&app, company, "VISIT_PLAN").await.is_some());
}

/// Quickstart selesai (100%): APP_INSTALL tanpa jeda.
#[tokio::test]
async fn reminder_completed_sends_app_install() {
    let Some(app) = TestApp::new(no_routes).await else {
        return;
    };
    let _g = SERIAL.lock().await;
    let (company, admins, _) = sales_company(&app).await;
    quickstart(&app, company, "NOW()", &[("completedAt", "NOW()")]).await;
    only_quickstart(&app, &[company]).await;

    let mail = FakeMail::ok();
    let r = quickstart_reminder::run_sweep(app.db(), &mail)
        .await
        .unwrap();
    assert_eq!((r.sent, r.failed), (1, 0));
    assert_eq!(mail.emails(), admins);
    assert_eq!(
        mail.calls()[0].2,
        "Tim Anda belum bisa di-tracking — 1 langkah yang sering terlewat"
    );
    let (status, ..) = email_row(&app, company, "APP_INSTALL").await.unwrap();
    assert_eq!(status, "SENT");

    // APP_INSTALL SENT → company keluar dari kandidat.
    let mail = FakeMail::ok();
    let r = quickstart_reminder::run_sweep(app.db(), &mail)
        .await
        .unwrap();
    assert_eq!((r.sent, r.failed), (0, 0));
    assert!(mail.calls().is_empty());
}

/// Tanpa owner/admin aktif: FAILED `no active owner/admin recipient`.
#[tokio::test]
async fn reminder_without_recipient_marks_failed() {
    let Some(app) = TestApp::new(no_routes).await else {
        return;
    };
    let _g = SERIAL.lock().await;
    let company = new_company(&app, true, false).await;
    subscribe(&app, company, "SALES").await;
    let staff = new_user(&app, 1).await;
    member(&app, staff.0, company, "STAFF", true).await;
    // Owner nonaktif bukan penerima.
    let disabled = new_user(&app, 3).await;
    member(&app, disabled.0, company, "SUPER_ADMIN", true).await;
    quickstart(&app, company, "NOW() - interval '25 hours'", &[]).await;
    only_quickstart(&app, &[company]).await;

    let mail = FakeMail::ok();
    let r = quickstart_reminder::run_sweep(app.db(), &mail)
        .await
        .unwrap();
    assert_eq!((r.sent, r.failed), (0, 1));
    assert!(mail.calls().is_empty());
    let (status, attempts, _, last_error, recipients) =
        email_row(&app, company, "COMPANY_SETTING").await.unwrap();
    assert_eq!((status.as_str(), attempts), ("FAILED", 1));
    assert_eq!(
        last_error.as_deref(),
        Some("no active owner/admin recipient")
    );
    assert_eq!(recipients, Some(serde_json::json!([])));
}

/// Syarat kandidat: paket subscription TERAKHIR = SALES, `reminderEligible`, Sales Tracker aktif.
#[tokio::test]
async fn reminder_candidate_filters() {
    let Some(app) = TestApp::new(no_routes).await else {
        return;
    };
    let _g = SERIAL.lock().await;
    let due = "NOW() - interval '25 hours'";

    // Langganan terakhir TIME (yang SALES lebih lama).
    let (time_tier, _, _) = sales_company(&app).await;
    subscribe(&app, time_tier, "TIME").await;
    quickstart(&app, time_tier, due, &[]).await;

    // Sales Tracker mati.
    let (tracker_off, _, _) = sales_company(&app).await;
    app.exec(&format!(
        r#"UPDATE company SET "moduleSalesTrackerEnabled" = false WHERE id = {tracker_off}"#
    ))
    .await;
    quickstart(&app, tracker_off, due, &[]).await;

    // `reminderEligible = false` (company lama).
    let (not_eligible, _, _) = sales_company(&app).await;
    quickstart(&app, not_eligible, due, &[]).await;
    app.exec(&format!(
        r#"UPDATE "companyQuickstart" SET "reminderEligible" = false WHERE "companyId" = {not_eligible}"#
    ))
    .await;

    only_quickstart(&app, &[time_tier, tracker_off]).await;

    let mail = FakeMail::ok();
    let r = quickstart_reminder::run_sweep(app.db(), &mail)
        .await
        .unwrap();
    assert_eq!((r.sent, r.failed), (0, 0));
    assert!(mail.calls().is_empty());
    for c in [time_tier, tracker_off, not_eligible] {
        assert_eq!(email_rows(&app, c).await, 0, "company {c}");
    }
}

// ───────────────────────── sales onboarding ─────────────────────────

/// User onboarding: `(id, username)`. `verified` = waktu `emailVerifiedAt`.
async fn onboarding_user(
    app: &TestApp,
    status: i32,
    eligible: bool,
    verified: Option<NaiveDateTime>,
    sent_at: Option<NaiveDateTime>,
) -> (i32, String) {
    let (id, username) = new_user(app, status).await;
    sqlx::query(
        r#"UPDATE users SET "salesOnboardingEmailEligible" = $2, "emailVerifiedAt" = $3,
                  "salesOnboardingEmailSentAt" = $4
           WHERE id = $1"#,
    )
    .bind(id)
    .bind(eligible)
    .bind(verified)
    .bind(sent_at)
    .execute(app.db())
    .await
    .unwrap();
    (id, username)
}

/// Hanya user `keep` yang layak email onboarding di database test ini.
async fn only_onboarding(app: &TestApp, keep: &[i32]) {
    let ids = keep
        .iter()
        .map(i32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    app.exec(&format!(
        r#"UPDATE users SET "salesOnboardingEmailEligible" = false
           WHERE "salesOnboardingEmailEligible" AND id <> ALL(ARRAY[{ids}]::int[])"#
    ))
    .await;
}

/// `(salesOnboardingEmailEligible, salesOnboardingEmailSentAt)`.
async fn onboarding_state(app: &TestApp, user_id: i32) -> (bool, Option<NaiveDateTime>) {
    sqlx::query_as(
        r#"SELECT "salesOnboardingEmailEligible", "salesOnboardingEmailSentAt" FROM users WHERE id = $1"#,
    )
    .bind(user_id)
    .fetch_one(app.db())
    .await
    .unwrap()
}

/// spec: "hanya memindai user SALES yang belum dikirimi, aktif, dan terverifikasi 24 jam–7 hari
/// lalu" + "mengklaim lalu mengirim email ke user yang belum create organization".
#[tokio::test]
async fn onboarding_window_claim_and_send() {
    let Some(app) = TestApp::new(no_routes).await else {
        return;
    };
    let _g = SERIAL.lock().await;
    let now = now_naive();
    let h = Duration::hours;
    let company = new_company(&app, true, false).await;

    let due = onboarding_user(&app, 1, true, Some(now - h(25)), None).await;
    let edge_24h = onboarding_user(&app, 1, true, Some(now - h(24)), None).await;
    let edge_7d = onboarding_user(&app, 1, true, Some(now - h(7 * 24)), None).await;
    let too_new = onboarding_user(&app, 1, true, Some(now - h(23)), None).await;
    let too_old = onboarding_user(&app, 1, true, Some(now - h(8 * 24)), None).await;
    let invited = onboarding_user(&app, 2, true, Some(now - h(25)), None).await;
    let unverified = onboarding_user(&app, 1, true, None, None).await;
    let not_sales = onboarding_user(&app, 1, false, Some(now - h(25)), None).await;
    let already = onboarding_user(&app, 1, true, Some(now - h(25)), Some(now - h(1))).await;
    let all = [
        &due,
        &edge_24h,
        &edge_7d,
        &too_new,
        &too_old,
        &invited,
        &unverified,
        &not_sales,
        &already,
    ];
    for u in all {
        member(&app, u.0, company, "SUPER_ADMIN", true).await;
    }
    only_onboarding(&app, &all.iter().map(|u| u.0).collect::<Vec<_>>()).await;

    let mail = FakeMail::ok();
    let r = sales_onboarding::run_sweep(app.db(), now, &mail)
        .await
        .unwrap();
    assert_eq!((r.sent, r.skipped_onboarded, r.failed), (3, 0, 0));
    let mut sent = mail.emails();
    sent.sort();
    let mut expected = vec![due.1.clone(), edge_24h.1.clone(), edge_7d.1.clone()];
    expected.sort();
    assert_eq!(sent, expected, "batas jendela inklusif (lte/gte)");
    // `{ email: username, name }`.
    let call = mail.calls().into_iter().find(|c| c.0 == due.1).unwrap();
    assert!(call.1.starts_with("IT "), "{call:?}");

    // Klaim = `salesOnboardingEmailSentAt: now` (presisi ms), kelayakan tetap.
    assert_eq!(onboarding_state(&app, due.0).await, (true, Some(now)));
    for u in [&too_new, &too_old, &invited, &unverified] {
        assert_eq!(onboarding_state(&app, u.0).await, (true, None), "{}", u.1);
    }
    assert_eq!(onboarding_state(&app, not_sales.0).await, (false, None));
    assert_eq!(
        onboarding_state(&app, already.0).await,
        (true, Some(now - h(1)))
    );

    // Tick berikutnya (+1 jam): yang sudah diklaim tidak dikirim ulang; `too_new` (verifikasi
    // now-23j) kini tepat 24 jam → masuk jendela (batas inklusif), satu-satunya yang dikirim.
    let mail = FakeMail::ok();
    let r = sales_onboarding::run_sweep(app.db(), now + h(1), &mail)
        .await
        .unwrap();
    assert_eq!(r.sent, 1);
    assert_eq!(mail.emails(), vec![too_new.1.clone()]);
}

/// spec: "tidak mengirim dan mencabut kelayakan kalau sudah create organization" — company
/// yang dicek = keanggotaan aktif default.
#[tokio::test]
async fn onboarding_revoked_when_organization_created() {
    let Some(app) = TestApp::new(no_routes).await else {
        return;
    };
    let _g = SERIAL.lock().await;
    let now = now_naive();
    let onboarded = new_company(&app, true, true).await;
    let fresh = new_company(&app, true, false).await;
    let user = onboarding_user(&app, 1, true, Some(now - Duration::hours(30)), None).await;
    member(&app, user.0, fresh, "STAFF", false).await;
    member(&app, user.0, onboarded, "SUPER_ADMIN", true).await;
    only_onboarding(&app, &[user.0]).await;

    let mail = FakeMail::ok();
    let r = sales_onboarding::run_sweep(app.db(), now, &mail)
        .await
        .unwrap();
    assert_eq!((r.sent, r.skipped_onboarded, r.failed), (0, 1, 0));
    assert!(mail.calls().is_empty());
    assert_eq!(onboarding_state(&app, user.0).await, (false, None));
}

/// spec: "melepas klaim miliknya sendiri kalau pengiriman gagal"; tick berikutnya mencoba lagi.
#[tokio::test]
async fn onboarding_failed_send_releases_claim() {
    let Some(app) = TestApp::new(no_routes).await else {
        return;
    };
    let _g = SERIAL.lock().await;
    let now = now_naive();
    let company = new_company(&app, true, false).await;
    let user = onboarding_user(&app, 1, true, Some(now - Duration::hours(30)), None).await;
    member(&app, user.0, company, "SUPER_ADMIN", true).await;
    only_onboarding(&app, &[user.0]).await;

    let mail = FakeMail::failing();
    let r = sales_onboarding::run_sweep(app.db(), now, &mail)
        .await
        .unwrap();
    assert_eq!((r.sent, r.skipped_onboarded, r.failed), (0, 0, 1));
    assert_eq!(mail.emails(), std::slice::from_ref(&user.1));
    assert_eq!(onboarding_state(&app, user.0).await, (true, None));

    let later = now + Duration::hours(1);
    let mail = FakeMail::ok();
    let r = sales_onboarding::run_sweep(app.db(), later, &mail)
        .await
        .unwrap();
    assert_eq!(r.sent, 1);
    assert_eq!(onboarding_state(&app, user.0).await, (true, Some(later)));
}

/// spec: "tidak mengirim kalau klaimnya sudah diambil replika lain" — klaim bersyarat.
#[tokio::test]
async fn onboarding_claim_is_exclusive() {
    let Some(app) = TestApp::new(no_routes).await else {
        return;
    };
    let _g = SERIAL.lock().await;
    let now = now_naive();
    let user = onboarding_user(&app, 1, true, Some(now - Duration::hours(30)), None).await;
    let audit = AuditHandle::detached(app.db().clone());

    assert!(
        sales_onboarding::claim(app.db(), &audit, user.0, now)
            .await
            .unwrap()
    );
    let other = now + Duration::milliseconds(5);
    assert!(
        !sales_onboarding::claim(app.db(), &audit, user.0, other)
            .await
            .unwrap()
    );
    assert_eq!(onboarding_state(&app, user.0).await, (true, Some(now)));

    // Kelayakan yang sudah dicabut juga tidak bisa diklaim.
    let revoked = onboarding_user(&app, 1, false, Some(now - Duration::hours(30)), None).await;
    assert!(
        !sales_onboarding::claim(app.db(), &audit, revoked.0, now)
            .await
            .unwrap()
    );
}

/// Tanpa keanggotaan aktif: `membership?.company?.hasCreateOrgnanization` falsy → tetap dikirim
/// (perilaku Node).
#[tokio::test]
async fn onboarding_without_membership_still_sends() {
    let Some(app) = TestApp::new(no_routes).await else {
        return;
    };
    let _g = SERIAL.lock().await;
    let now = now_naive();
    let user = onboarding_user(&app, 1, true, Some(now - Duration::hours(30)), None).await;
    only_onboarding(&app, &[user.0]).await;

    let mail = FakeMail::ok();
    let r = sales_onboarding::run_sweep(app.db(), now, &mail)
        .await
        .unwrap();
    assert_eq!((r.sent, r.skipped_onboarded, r.failed), (1, 0, 0));
    assert_eq!(mail.emails(), [user.1]);
}
