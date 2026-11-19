//! Uji integrasi `monitoring::count_activity` (query `countActivity` Node) terhadap Postgres
//! nyata. Hanya query DB; Discord, DigitalOcean, dan penjadwal TIDAK disentuh (tidak ada
//! `monitoring::spawn` di sini, dan mz-testkit menolak jalan bila `DISCORD_MONITORING_WEBHOOK`
//! / `DO_TOKEN` ada di env).
//!
//! Jalankan: `MZ_TEST_DATABASE_URL=... MZ_TEST_REQUIRE_DB=1 cargo test -p mz-misc --test it_misc_monitoring`.
//!
//! Data dibuat di tanggal 2099 supaya hitungan tracking tidak tercampur data template.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use axum::Router;
use chrono::{DateTime, Utc};
use mz_core::AppState;
use mz_misc::monitoring::count_activity;
use mz_testkit::TestApp;

fn no_routes(_: &AppState) -> Router {
    Router::new()
}

fn at(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

#[tokio::test]
async fn counts_active_tracking_and_visits() {
    let Some(app) = TestApp::new(no_routes).await else {
        return;
    };
    let company = app.company().await;
    let project: i32 = app
        .scalar("SELECT id FROM project ORDER BY id LIMIT 1")
        .await;

    // now = 2099-01-01T00:00Z (07:00 WIB): takenDate 2098-12-31 … 2099-01-02 terjangkau.
    let now = at("2099-01-01T00:00:00Z");
    let before = count_activity(app.db(), now).await.unwrap();
    assert_eq!(before.active_user_tracking, 0, "tanggal 2099 harus kosong");

    let tracking = |user: i32, date: &'static str| {
        let app = &app;
        async move {
            app.scalar::<i32>(&format!(
                r#"INSERT INTO tracking ("takenDate", "userId", status)
                   VALUES ('{date}', {user}, 1) RETURNING id"#
            ))
            .await
        }
    };
    let timeline = |tracking: i32, start: &str, end: &str, status: i32, manual: &str| {
        format!(
            r#"INSERT INTO "trackingTimeline"
                 (note, "startAt", "endAt", "startAtUtc", "endAtUtc", "trackingId", "projectId", status, "manualTimeStatus")
               VALUES ('it-monitoring', '{start}', '{end}', '{start}', '{end}', {tracking}, {project}, {status}, {manual})"#
        )
    };

    // staff: dua segmen aktif (dihitung SATU user).
    let t_staff = tracking(company.staff.id, "2099-01-01").await;
    app.exec(&timeline(
        t_staff,
        "2098-12-31 23:50:00",
        "2099-01-01 00:00:00",
        1,
        "NULL",
    ))
    .await;
    app.exec(&timeline(
        t_staff,
        "2098-12-31 23:55:00",
        "2098-12-31 23:58:00",
        1,
        "NULL",
    ))
    .await;
    // staff2: takenDate kemarin (zona lain), segmen berakhir tepat di batas 10 menit → dihitung.
    let t_staff2 = tracking(company.staff2.id, "2098-12-31").await;
    app.exec(&timeline(
        t_staff2,
        "2098-12-31 23:00:00",
        "2098-12-31 23:50:00",
        1,
        "NULL",
    ))
    .await;
    // manager: segmen lama (> 10 menit), manual time, status 0, dan segmen masa depan → tidak.
    let t_manager = tracking(company.manager.id, "2099-01-01").await;
    app.exec(&timeline(
        t_manager,
        "2098-12-31 23:00:00",
        "2098-12-31 23:49:59",
        1,
        "NULL",
    ))
    .await;
    app.exec(&timeline(
        t_manager,
        "2098-12-31 23:55:00",
        "2099-01-01 00:00:00",
        1,
        "'APPROVED'",
    ))
    .await;
    app.exec(&timeline(
        t_manager,
        "2098-12-31 23:55:00",
        "2099-01-01 00:00:00",
        0,
        "NULL",
    ))
    .await;
    app.exec(&timeline(
        t_manager,
        "2099-01-01 00:00:01",
        "2099-01-01 00:05:00",
        1,
        "NULL",
    ))
    .await;
    // admin: takenDate di luar ±1 hari → tidak.
    let t_admin = tracking(company.admin.id, "2099-01-03").await;
    app.exec(&timeline(
        t_admin,
        "2098-12-31 23:55:00",
        "2099-01-01 00:00:00",
        1,
        "NULL",
    ))
    .await;

    let counts = count_activity(app.db(), now).await.unwrap();
    assert_eq!(counts.active_user_tracking, 2);

    let visits: i64 = app
        .scalar(r#"SELECT COUNT(*) FROM "storeVisit" WHERE state = 'active'"#)
        .await;
    assert_eq!(counts.active_sales_on_visit, visits);
}
