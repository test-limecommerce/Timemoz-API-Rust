//! Uji integrasi jalur DB analytics (sweeper, klaim, cron milestone) terhadap Postgres nyata.
//! GA4 TIDAK dihubungi: setiap test menonaktifkan GA (`GA_MEASUREMENT_ID` kosong) sehingga
//! baris berakhir `SKIPPED`, atau hanya menguji klaim/seleksi yang tidak memanggil HTTP.
//! Jalankan: `MZ_TEST_DATABASE_URL=... cargo test -p mz-auth --test it_jobs`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use axum::Router;
use mz_audit::AuditHandle;
use mz_auth::analytics::{DAY_30_RETAINED, FIRST_TRACKED_HOUR, claim_event};
use mz_auth::analytics_jobs::{retry_candidates, run_milestones};
use mz_auth::ga::send_event;
use mz_core::AppState;
use mz_core::time::now_naive;
use mz_testkit::TestApp;
use serde_json::json;

fn no_routes(_: &AppState) -> Router {
    Router::new()
}

/// GA dimatikan lewat override kosong (nilai kosong dibaca sebagai tidak diset).
const GA_OFF: &[(&str, &str)] = &[("GA_MEASUREMENT_ID", ""), ("GA_API_SECRET", "")];

fn uniq() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..10].to_owned()
}

/// Buat user baru (status aktif, tanpa gaClientId). `created` = ekspresi SQL timestamp.
async fn new_user(app: &TestApp, created: &str) -> i32 {
    let u = uniq();
    app.scalar(&format!(
        r#"INSERT INTO users (username, password, name, status, "createdAt")
           VALUES ('it_auth_{u}', 'x', 'IT {u}', 1, {created}) RETURNING id"#
    ))
    .await
}

async fn event_row(app: &TestApp, user_id: i32, event: &str) -> (String, i32, Option<String>) {
    let status: String = app
        .scalar(&format!(
            r#"SELECT status FROM "analyticsEvent" WHERE "userId" = {user_id} AND "eventName" = '{event}'"#
        ))
        .await;
    let attempts: i32 = app
        .scalar(&format!(
            r#"SELECT attempts FROM "analyticsEvent" WHERE "userId" = {user_id} AND "eventName" = '{event}'"#
        ))
        .await;
    let last_error: Option<String> = app
        .scalar(&format!(
            r#"SELECT "lastError" FROM "analyticsEvent" WHERE "userId" = {user_id} AND "eventName" = '{event}'"#
        ))
        .await;
    (status, attempts, last_error)
}

#[tokio::test]
async fn retry_candidates_pick_only_stale_pending_rows() {
    let Some(app) = TestApp::with_vars(no_routes, GA_OFF).await else {
        return;
    };
    let fx = app.company().await;
    let tag = uniq();
    // Nama event unik per baris (unique (userId, eventName)); seleksi sweep tidak memeriksa nama.
    let insert = |user: i32,
                  name: String,
                  status: &str,
                  attempts: i32,
                  sending: &str,
                  created: &str| {
        format!(
            r#"INSERT INTO "analyticsEvent" ("userId", "eventName", status, attempts, "occurredAt", "sendingAt", "createdAt")
               VALUES ({user}, '{name}', '{status}', {attempts}, now(), {sending}, {created}) RETURNING id"#
        )
    };
    let old = "now() - interval '1 hour'";
    let fresh = "now()";
    let a: i32 = app
        .scalar(&insert(
            fx.super_admin.id,
            format!("a_{tag}"),
            "PENDING",
            0,
            "NULL",
            old,
        ))
        .await;
    let b: i32 = app
        .scalar(&insert(
            fx.admin.id,
            format!("b_{tag}"),
            "PENDING",
            1,
            old,
            fresh,
        ))
        .await;
    let sent: i32 = app
        .scalar(&insert(
            fx.manager.id,
            format!("c_{tag}"),
            "SENT",
            1,
            "NULL",
            old,
        ))
        .await;
    let exhausted: i32 = app
        .scalar(&insert(
            fx.staff.id,
            format!("d_{tag}"),
            "PENDING",
            5,
            "NULL",
            old,
        ))
        .await;
    let recent: i32 = app
        .scalar(&insert(
            fx.staff2.id,
            format!("e_{tag}"),
            "PENDING",
            0,
            "NULL",
            fresh,
        ))
        .await;

    let stale_before = now_naive() - chrono::Duration::minutes(10);
    let ids = retry_candidates(&app.state, stale_before)
        .await
        .expect("retry candidates");
    assert!(
        ids.contains(&a),
        "baris PENDING tua tanpa sendingAt harus terpilih"
    );
    assert!(
        ids.contains(&b),
        "baris PENDING dengan sendingAt tua harus terpilih"
    );
    assert!(!ids.contains(&sent), "SENT tidak boleh di-retry");
    assert!(
        !ids.contains(&exhausted),
        "attempts >= 5 tidak boleh di-retry"
    );
    assert!(!ids.contains(&recent), "baris baru belum layak di-retry");
}

#[tokio::test]
async fn send_with_ga_disabled_marks_row_skipped() {
    let Some(app) = TestApp::with_vars(no_routes, GA_OFF).await else {
        return;
    };
    let fx = app.company().await;
    let name = format!("skip_{}", uniq());
    let id: i32 = app
        .scalar(&format!(
            r#"INSERT INTO "analyticsEvent" ("userId", "eventName", status, "occurredAt")
               VALUES ({}, '{name}', 'PENDING', now()) RETURNING id"#,
            fx.super_admin.id
        ))
        .await;
    send_event(&app.state, id).await;
    let (status, attempts, last_error) = event_row(&app, fx.super_admin.id, &name).await;
    assert_eq!(status, "SKIPPED");
    assert_eq!(attempts, 1);
    assert_eq!(last_error.as_deref(), Some("GA disabled"));
    let sent_at_null: bool = app
        .scalar(&format!(
            r#"SELECT "sentAt" IS NULL FROM "analyticsEvent" WHERE id = {id}"#
        ))
        .await;
    assert!(sent_at_null);

    // Baris yang sudah tidak PENDING tidak diklaim ulang.
    send_event(&app.state, id).await;
    let (_, attempts_again, _) = event_row(&app, fx.super_admin.id, &name).await;
    assert_eq!(attempts_again, 1);
}

#[tokio::test]
async fn claim_is_idempotent_and_assigns_ga_client_id() {
    let Some(app) =
        TestApp::with_vars(no_routes, &[("GA_NEW_USER_SINCE", "2000-01-01T00:00:00Z")]).await
    else {
        return;
    };
    let user = new_user(&app, "now()").await;
    let audit = AuditHandle::detached(app.state.db.clone());
    let params = vec![("platform", json!("ios")), ("company_id", json!(null))];

    let first = claim_event(
        &app.state,
        &audit,
        user,
        FIRST_TRACKED_HOUR,
        params.clone(),
        now_naive(),
    )
    .await
    .expect("claim");
    assert!(first.is_some(), "klaim pertama harus menang");
    let second = claim_event(
        &app.state,
        &audit,
        user,
        FIRST_TRACKED_HOUR,
        params,
        now_naive(),
    )
    .await
    .expect("claim ulang");
    assert!(
        second.is_none(),
        "klaim kedua harus kalah (unique userId+eventName)"
    );

    let client_id: Option<String> = app
        .scalar(&format!(
            r#"SELECT "gaClientId" FROM users WHERE id = {user}"#
        ))
        .await;
    let client_id = client_id.unwrap_or_default();
    assert!(
        client_id.contains('.'),
        "format GA client id `{{random}}.{{unix}}`: {client_id}"
    );

    let params_json: serde_json::Value = app
        .scalar(&format!(
            r#"SELECT params FROM "analyticsEvent" WHERE "userId" = {user} AND "eventName" = '{FIRST_TRACKED_HOUR}'"#
        ))
        .await;
    assert_eq!(params_json["platform"], json!("ios"));
    assert_eq!(params_json["env"], json!("development"));
    assert!(
        params_json.get("company_id").is_none(),
        "null dibuang seperti sanitizeParams"
    );
}

#[tokio::test]
async fn claim_respects_new_user_cutoff() {
    let Some(app) = TestApp::with_vars(no_routes, &[]).await else {
        return;
    };
    // Cutoff default 2026-08-01; user lama tidak boleh mendapat event kohort baru.
    let old_user = new_user(&app, "'2020-01-01 00:00:00'").await;
    let audit = AuditHandle::detached(app.state.db.clone());
    let res = claim_event(
        &app.state,
        &audit,
        old_user,
        DAY_30_RETAINED,
        vec![],
        now_naive(),
    )
    .await
    .expect("claim");
    assert!(res.is_none());
}

#[tokio::test]
async fn milestone_run_claims_first_tracked_hour_from_timeline() {
    let Some(app) = TestApp::with_vars(
        no_routes,
        &[
            ("GA_NEW_USER_SINCE", "2000-01-01T00:00:00Z"),
            ("GA_MEASUREMENT_ID", ""),
            ("GA_API_SECRET", ""),
        ],
    )
    .await
    else {
        return;
    };
    let user = new_user(&app, "now()").await;
    let c = uniq();
    app.exec(&format!(
        r#"INSERT INTO "projectType" (code, name, status) VALUES ('PT{c}', 'PT {c}', 1)"#
    ))
    .await;
    let project: i32 = app
        .scalar(&format!(
            r#"INSERT INTO project (code, name, "screenshotPerHour", "totalHour", "projectTypeId", "startDate", status, billable)
               VALUES ('P{c}', 'P {c}', 0, 0, (SELECT id FROM "projectType" WHERE code = 'PT{c}'), '2026-01-01', 1, true)
               RETURNING id"#
        ))
        .await;
    let tracking: i32 = app
        .scalar(&format!(
            r#"INSERT INTO tracking ("takenDate", "userId", status) VALUES (CURRENT_DATE, {user}, 1) RETURNING id"#
        ))
        .await;
    // Dua jam terbakar (status 1) -> 7200 detik >= ambang 3600.
    app.exec(&format!(
        r#"INSERT INTO "trackingTimeline" (note, "startAt", "endAt", "trackingId", "projectId", status)
           VALUES ('it', now() - interval '2 hours', now(), {tracking}, {project}, 1)"#
    ))
    .await;

    run_milestones(app.state.clone())
        .await
        .expect("milestone run");

    let (status, attempts, _) = event_row(&app, user, FIRST_TRACKED_HOUR).await;
    assert_eq!(
        status, "SKIPPED",
        "GA mati: baris diklaim lalu ditandai SKIPPED"
    );
    assert_eq!(attempts, 1);
    let total: String = app
        .scalar(&format!(
            r#"SELECT params->>'total_seconds' FROM "analyticsEvent" WHERE "userId" = {user} AND "eventName" = '{FIRST_TRACKED_HOUR}'"#
        ))
        .await;
    assert_eq!(total, "7200");
    // 2 jam hanya satu hari: retensi belum terpenuhi.
    let retained: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "analyticsEvent" WHERE "userId" = {user} AND "eventName" = '{DAY_30_RETAINED}'"#
        ))
        .await;
    assert_eq!(retained, 0);
}
