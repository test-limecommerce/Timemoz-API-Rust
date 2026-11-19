//! Uji integrasi `emit_upgrade_to_pro` (Node `AnalyticsService.emitUpgradeToPro`) terhadap
//! Postgres nyata. GA4 TIDAK dihubungi (`GA_MEASUREMENT_ID` kosong, `MZ_JOBS` kosong → tidak
//! ada kirim langsung); yang diuji hanya resolusi owner + klaim baris `analyticsEvent`.
//! Jalankan: `MZ_TEST_DATABASE_URL=... cargo test -p mz-auth --test it_upgrade_to_pro`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use axum::Router;
use mz_audit::AuditHandle;
use mz_auth::analytics::{
    UPGRADE_TO_PRO, UpgradeToPro, emit_upgrade_to_pro, resolve_company_owner_user_id,
};
use mz_core::AppState;
use mz_testkit::TestApp;
use tokio::sync::Mutex;

/// Semua test memakai company fixture yang sama: dijalankan berurutan.
static SERIAL: Mutex<()> = Mutex::const_new(());

fn no_routes(_: &AppState) -> Router {
    Router::new()
}

const ENV: &[(&str, &str)] = &[
    ("GA_MEASUREMENT_ID", ""),
    ("GA_API_SECRET", ""),
    ("MZ_JOBS", ""),
    ("GA_ENV", "staging"),
];

fn input(company_id: i32, first: bool) -> UpgradeToPro {
    UpgradeToPro {
        company_id,
        is_first_paid_transition: first,
        tier_name: Some("  Pro  ".into()),
        billing_interval: Some("MONTHLY".into()),
        payment_provider: "paypal",
        value: Some(10.0),
        currency: Some("USD".into()),
    }
}

async fn clear(app: &TestApp, company_id: i32) {
    app.exec(&format!(
        r#"DELETE FROM "analyticsEvent" WHERE "eventName" = '{UPGRADE_TO_PRO}'
             AND "userId" IN (SELECT "userId" FROM "userCompany" WHERE "companyId" = {company_id})"#
    ))
    .await;
}

#[tokio::test]
async fn claims_for_owner_once_with_node_params() {
    let _g = SERIAL.lock().await;
    let Some(app) = TestApp::with_vars(no_routes, ENV).await else {
        return;
    };
    let c = app.company().await;
    clear(&app, c.id).await;
    app.exec(&format!(
        r#"UPDATE company SET "ownerUserId" = {} WHERE id = {}"#,
        c.admin.id, c.id
    ))
    .await;
    let audit = AuditHandle::detached(app.state.db.clone());

    // Renewal (bukan transisi pertama) → tidak ada klaim.
    assert!(
        emit_upgrade_to_pro(&app.state, &audit, input(c.id, false))
            .await
            .is_none()
    );

    // `ownerUserId` menang; user lama tetap diklaim (upgrade_to_pro bebas cutoff user baru).
    let id = emit_upgrade_to_pro(&app.state, &audit, input(c.id, true))
        .await
        .expect("klaim pertama harus menang");
    let (user_id, status, params): (i32, String, String) = sqlx::query_as(
        r#"SELECT "userId", status::text, params::text FROM "analyticsEvent" WHERE id = $1"#,
    )
    .bind(id)
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!(user_id, c.admin.id);
    assert_eq!(status, "PENDING");
    // Urutan kunci jsonb ditentukan Postgres; isi = sanitizeParams Node (trim, angka JS `10`).
    let params: serde_json::Value = serde_json::from_str(&params).unwrap();
    assert_eq!(
        params,
        serde_json::json!({
            "env": "staging",
            "company_id": c.id,
            "tier_name": "Pro",
            "billing_interval": "MONTHLY",
            "payment_provider": "paypal",
            "value": 10,
            "currency": "USD"
        })
    );
    assert!(params["value"].is_i64(), "value harus 10, bukan 10.0");

    // Retry webhook → klaim kalah (unique userId+eventName).
    assert!(
        emit_upgrade_to_pro(&app.state, &audit, input(c.id, true))
            .await
            .is_none()
    );
}

#[tokio::test]
async fn owner_fallbacks_follow_node() {
    let _g = SERIAL.lock().await;
    let Some(app) = TestApp::with_vars(no_routes, ENV).await else {
        return;
    };
    let c = app.company().await;
    let db = app.db();

    // 1) Tanpa ownerUserId → SUPER_ADMIN bawaan aktif dengan userCompany terlama.
    app.exec(&format!(
        r#"UPDATE company SET "ownerUserId" = NULL WHERE id = {}"#,
        c.id
    ))
    .await;
    let expected_sa: i32 = app
        .scalar(&format!(
            r#"SELECT uc."userId" FROM "userCompany" uc
                WHERE uc."companyId" = {c} AND uc.status = 1 AND EXISTS (
                  SELECT 1 FROM "userAccessLevel" ual JOIN "accessLevel" al ON al.id = ual."accessLevelId"
                   WHERE ual."userId" = uc."userId" AND ual."companyId" = {c} AND ual.status = 1
                     AND al."isSystem" AND al.code = 'SUPER_ADMIN')
                ORDER BY uc."createdAt", uc.id LIMIT 1"#,
            c = c.id
        ))
        .await;
    assert_eq!(
        resolve_company_owner_user_id(db, c.id).await.unwrap(),
        Some(expected_sa)
    );

    // 2) Tanpa SUPER_ADMIN aktif → userCompany aktif terlama.
    // Dipulihkan di akhir: fixture company test lain membutuhkan SUPER_ADMIN aktif.
    let disabled: Vec<i32> = sqlx::query_scalar(
        r#"UPDATE "userAccessLevel" ual SET status = 0 FROM "accessLevel" al
            WHERE al.id = ual."accessLevelId" AND al.code = 'SUPER_ADMIN' AND ual."companyId" = $1
              AND ual.status = 1
            RETURNING ual.id"#,
    )
    .bind(c.id)
    .fetch_all(db)
    .await
    .unwrap();
    let oldest: i32 = app
        .scalar(&format!(
            r#"SELECT "userId" FROM "userCompany" WHERE "companyId" = {} AND status = 1
                ORDER BY "createdAt", id LIMIT 1"#,
            c.id
        ))
        .await;
    assert_eq!(
        resolve_company_owner_user_id(db, c.id).await.unwrap(),
        Some(oldest)
    );

    // 3) Company tanpa anggota aktif → None, emit dilewati tanpa galat.
    let empty: i32 = app
        .scalar(
            r#"INSERT INTO company (name, status, "createdAt", "updatedAt")
               VALUES ('it upgrade_to_pro kosong', 1, now(), now()) RETURNING id"#,
        )
        .await;
    assert_eq!(
        resolve_company_owner_user_id(db, empty).await.unwrap(),
        None
    );
    let audit = AuditHandle::detached(app.state.db.clone());
    assert!(
        emit_upgrade_to_pro(&app.state, &audit, input(empty, true))
            .await
            .is_none()
    );
    assert!(
        emit_upgrade_to_pro(&app.state, &audit, input(0, true))
            .await
            .is_none()
    );

    sqlx::query(r#"UPDATE "userAccessLevel" SET status = 1 WHERE id = ANY($1)"#)
        .bind(&disabled)
        .execute(db)
        .await
        .unwrap();
}
