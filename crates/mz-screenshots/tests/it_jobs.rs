//! Uji integrasi Postgres nyata untuk kerja latar screenshot. GCS tidak tersedia: jalur
//! jaringan harus gagal aman (galat dilog/dikembalikan, baris tidak rusak).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use axum::Router;
use mz_core::AppState;
use mz_core::queue::{Job, Outcome};
use mz_screenshots::jobs::*;
use mz_testkit::TestApp;

fn queue_job(kind: &str, id: i32) -> Job {
    Job {
        id: format!("screenshot.{kind}:{id}"),
        data: serde_json::json!({ "kind": kind, "screenshotId": id }),
        attempt: 1,
        max_attempts: 1,
        backoff_ms: 0,
        enqueued_at: 0,
    }
}

async fn app() -> Option<TestApp> {
    TestApp::new(|_| Router::new()).await
}

fn uniq() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

async fn one_i32(state: &AppState, sql: &str) -> i32 {
    sqlx::query_scalar::<_, i32>(sql)
        .fetch_one(&state.db)
        .await
        .unwrap()
}

/// company (retensi `days`) + project tertaut; mengembalikan (company_id, project_id).
async fn project_with_retention(state: &AppState, days: i32) -> (i32, i32) {
    let c = sqlx::query_scalar::<_, i32>(
        r#"INSERT INTO company (name, status, "screenshotRetentionDays") VALUES ($1, 1, $2) RETURNING id"#,
    )
    .bind(format!("it-shot-{}", uniq()))
    .bind(days)
    .fetch_one(&state.db)
    .await
    .unwrap();
    let p = project_for_company(state, c).await;
    (c, p)
}

async fn project_for_company(state: &AppState, company: i32) -> i32 {
    let pt = one_i32(state, r#"SELECT id FROM "projectType" ORDER BY id LIMIT 1"#).await;
    let p = sqlx::query_scalar::<_, i32>(
        r#"INSERT INTO project (code, name, "screenshotPerHour", "projectTypeId", "startDate", status)
           VALUES ($1, $1, 3, $2, CURRENT_DATE, 1) RETURNING id"#,
    )
    .bind(format!("it{}", uniq()))
    .bind(pt)
    .fetch_one(&state.db)
    .await
    .unwrap();
    sqlx::query(
        r#"INSERT INTO "projectCompany" ("projectId", "companyId", status) VALUES ($1, $2, 1)"#,
    )
    .bind(p)
    .bind(company)
    .execute(&state.db)
    .await
    .unwrap();
    p
}

/// tracking + timeline milik `user_id` pada `project`; mengembalikan timeline id.
async fn timeline(state: &AppState, user_id: i32, project: i32) -> i32 {
    let t = sqlx::query_scalar::<_, i32>(
        r#"INSERT INTO tracking ("takenDate", "userId", status) VALUES (CURRENT_DATE, $1, 1)
           ON CONFLICT ("userId", "takenDate") DO UPDATE SET status = 1 RETURNING id"#,
    )
    .bind(user_id)
    .fetch_one(&state.db)
    .await
    .unwrap();
    sqlx::query_scalar::<_, i32>(
        r#"INSERT INTO "trackingTimeline" (note, "startAt", "endAt", "trackingId", "projectId", status)
           VALUES ('it', NOW(), NOW(), $1, $2, 1) RETURNING id"#,
    )
    .bind(t)
    .bind(project)
    .fetch_one(&state.db)
    .await
    .unwrap()
}

async fn shot(
    state: &AppState,
    tl: i32,
    project: i32,
    full: &str,
    status: i32,
    age_days: i32,
) -> i32 {
    sqlx::query_scalar::<_, i32>(
        r#"INSERT INTO screenshot ("fileFullpath", "takenAt", "trackingTimelineId", "projectId", status, "createdAt")
           VALUES ($1, NOW(), $2, $3, $4, NOW() - make_interval(days => $5)) RETURNING id"#,
    )
    .bind(full)
    .bind(tl)
    .bind(project)
    .bind(status)
    .bind(age_days)
    .fetch_one(&state.db)
    .await
    .unwrap()
}

async fn status_of(state: &AppState, id: i32) -> Option<i32> {
    sqlx::query_scalar::<_, i32>("SELECT status FROM screenshot WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.db)
        .await
        .unwrap()
}

#[tokio::test]
async fn retention_purge_marks_expired_rows_and_survives_gcs_failure() {
    let Some(app) = app().await else { return };
    let s = &app.state;
    let fx = app.company().await;
    let (c30, p30) = project_with_retention(s, 30).await;
    let (_cu, p_unl) = project_with_retention(s, 36500).await;
    // project dengan dua company (30 & 90): retensi efektif 90.
    let c90 = sqlx::query_scalar::<_, i32>(
        r#"INSERT INTO company (name, status, "screenshotRetentionDays") VALUES ($1, 1, 90) RETURNING id"#,
    )
    .bind(format!("it-shot-{}", uniq()))
    .fetch_one(&s.db)
    .await
    .unwrap();
    sqlx::query(
        r#"INSERT INTO "projectCompany" ("projectId", "companyId", status) VALUES ($1, $2, 1)"#,
    )
    .bind(p30)
    .bind(c90)
    .execute(&s.db)
    .await
    .unwrap();
    let _ = c30;
    let tl30 = timeline(s, fx.staff.id, p30).await;
    let tl_unl = timeline(s, fx.staff.id, p_unl).await;

    let key = "companies/x/p/u/screenshots/a.jpg";
    let old_120 = shot(s, tl30, p30, key, 3, 120).await; // > 90: kedaluwarsa
    let mid_60 = shot(s, tl30, p30, key, 3, 60).await; // < 90 (maks): aman
    let young = shot(s, tl30, p30, key, 3, 1).await;
    let unl_old = shot(s, tl_unl, p_unl, key, 3, 1000).await; // project unlimited: aman
    let already = shot(s, tl30, p30, key, 0, 200).await; // sudah DELETED: tidak dihitung

    let n = purge_expired_screenshots(s).await.unwrap();
    assert!(n >= 1);
    assert_eq!(status_of(s, old_120).await, Some(0));
    assert_eq!(status_of(s, mid_60).await, Some(3));
    assert_eq!(status_of(s, young).await, Some(3));
    assert_eq!(status_of(s, unl_old).await, Some(3));
    assert_eq!(status_of(s, already).await, Some(0));
    // Fungsi cron membungkus galat dan tidak panik.
    run_retention_purge(s.clone()).await.unwrap();
}

#[tokio::test]
async fn purge_deleted_rows_select_and_consumer() {
    let Some(app) = app().await else { return };
    let s = &app.state;
    let fx = app.company().await;
    let (_c, p) = project_with_retention(s, 30).await;
    let tl = timeline(s, fx.staff.id, p).await;
    let key = "companies/x/p/u/screenshots/b.jpg";

    let del_old = shot(s, tl, p, key, 0, 45).await; // DELETED & lewat retensi
    let del_young = shot(s, tl, p, key, 0, 5).await; // DELETED tapi belum lewat
    let live_old = shot(s, tl, p, key, 3, 45).await; // hidup: bukan kandidat
    let no_file = shot(s, tl, p, "", 0, 45).await; // tanpa file: tak perlu GCS
    let invalid = shot(s, tl, p, "https://example.com/x.jpg", 0, 45).await; // key tak sah -> dilewati

    let ids = select_purge_candidates(s, mz_core::time::now_naive())
        .await
        .unwrap();
    assert!(ids.contains(&del_old) && ids.contains(&no_file) && ids.contains(&invalid));
    assert!(!ids.contains(&del_young) && !ids.contains(&live_old));

    // Tanpa GCS: baris dengan file sah GAGAL dan tetap ada (aman, bisa dicoba lagi besok).
    assert!(purge_screenshot(s, del_old).await.is_err());
    assert_eq!(status_of(s, del_old).await, Some(0));
    // Tanpa object yang bisa dihapus: baris dihapus dari DB.
    purge_screenshot(s, no_file).await.unwrap();
    purge_screenshot(s, invalid).await.unwrap();
    assert_eq!(status_of(s, no_file).await, None);
    assert_eq!(status_of(s, invalid).await, None);
    // Hidup + belum lewat retensi user (baru): dibiarkan.
    let live_new = shot(s, tl, p, "", 3, 0).await;
    purge_screenshot(s, live_new).await.unwrap();
    assert_eq!(status_of(s, live_new).await, Some(3));
    // Id tak ada: no-op.
    purge_screenshot(s, i32::MAX).await.unwrap();

    // Handler antrean: sukses -> Done; galat GCS -> Retry (attempts 1 = dibuang + log).
    let limits = Limits::default();
    assert_eq!(
        handle_job(s, &limits, queue_job("purge", live_new)).await,
        Outcome::Done
    );
    assert!(matches!(
        handle_job(s, &limits, queue_job("purge", del_old)).await,
        Outcome::Retry(_)
    ));
    // Payload tidak valid: dibuang tanpa menyentuh DB.
    let mut bad = queue_job("purge", 1);
    bad.data = serde_json::json!({ "kind": "nope" });
    assert!(matches!(
        handle_job(s, &limits, bad).await,
        Outcome::Drop(_)
    ));
}

#[tokio::test]
async fn compress_selection_and_consumer_db_paths() {
    let Some(app) = app().await else { return };
    let s = &app.state;
    let fx = app.company().await;
    let (_c, p) = project_with_retention(s, 30).await;
    let (_cu, p_unl) = project_with_retention(s, 36500).await;
    let tl = timeline(s, fx.staff.id, p).await;
    let tl_unl = timeline(s, fx.staff.id, p_unl).await;
    let key = "companies/x/p/u/screenshots/c.jpg";

    let cand = shot(s, tl, p, key, 3, 10).await;
    let too_young = shot(s, tl, p, key, 3, 3).await;
    let uploading = shot(s, tl, p, key, 1, 10).await;
    let empty_path = shot(s, tl, p, "", 3, 10).await;
    let unl = shot(s, tl_unl, p_unl, key, 3, 10).await;
    let done = shot(s, tl, p, key, 3, 10).await;
    sqlx::query(r#"UPDATE screenshot SET "compressedAt" = NOW() WHERE id = $1"#)
        .bind(done)
        .execute(&s.db)
        .await
        .unwrap();

    let ids = select_compress_candidates(s, mz_core::time::now_naive())
        .await
        .unwrap();
    assert!(ids.contains(&cand));
    for x in [too_young, uploading, empty_path, unl, done] {
        assert!(!ids.contains(&x), "{x} tidak boleh terpilih");
    }
    assert!(ids.len() <= 200 && ids.windows(2).all(|w| w[0] < w[1]));

    // Atur retensi seluruh company milik user fixture agar hasil deterministik.
    let set_retention = |days: i32| {
        sqlx::query(
            r#"UPDATE company SET "screenshotRetentionDays" = $1
               WHERE id IN (SELECT "companyId" FROM "userCompany" WHERE "userId" = $2)"#,
        )
        .bind(days)
        .bind(fx.staff.id)
        .execute(&s.db)
    };
    let compressed_at = |id: i32| {
        sqlx::query_scalar::<_, Option<chrono::NaiveDateTime>>(
            r#"SELECT "compressedAt" FROM screenshot WHERE id = $1"#,
        )
        .bind(id)
        .fetch_one(&s.db)
    };

    // Bukan free-tier menurut company user: ditandai, tidak ada GCS.
    set_retention(36500).await.unwrap();
    compress_screenshot(s, cand).await.unwrap();
    assert!(compressed_at(cand).await.unwrap().is_some());

    // Free-tier tetapi key tak sah (fileFullpath kosong): ditandai permanen.
    set_retention(30).await.unwrap();
    let blank = shot(s, tl, p, "", 3, 10).await;
    compress_screenshot(s, blank).await.unwrap();
    assert!(compressed_at(blank).await.unwrap().is_some());

    // Free-tier + key sah + GCS tidak tersedia: job gagal, baris tidak diubah.
    let real = shot(s, tl, p, key, 3, 10).await;
    assert!(compress_screenshot(s, real).await.is_err());
    assert!(compressed_at(real).await.unwrap().is_none());
    let flags: (bool, Option<String>) = sqlx::query_as(
        r#"SELECT "isCompressed", "compressedFileKey" FROM screenshot WHERE id = $1"#,
    )
    .bind(real)
    .fetch_one(&s.db)
    .await
    .unwrap();
    assert_eq!(flags, (false, None));

    // Sudah ditandai: tidak ditimpa. Id tak ada: no-op.
    let before = compressed_at(blank).await.unwrap();
    compress_screenshot(s, blank).await.unwrap();
    assert_eq!(compressed_at(blank).await.unwrap(), before);
    compress_screenshot(s, i32::MAX).await.unwrap();

    let limits = Limits::default();
    assert_eq!(
        handle_job(s, &limits, queue_job("compress", blank)).await,
        Outcome::Done
    );
    assert!(matches!(
        handle_job(s, &limits, queue_job("compress", real)).await,
        Outcome::Retry(_)
    ));
}
