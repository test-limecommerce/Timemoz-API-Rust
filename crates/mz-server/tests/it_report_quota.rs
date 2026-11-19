//! Uji integrasi kuota export laporan (`claimExportQuota`) — satu test berurutan karena
//! mengubah baris `company` yang sama.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use common::{D1, D2, seed};
use mz_testkit::TestApp;
use serde_json::json;

#[tokio::test]
async fn export_quota_claim_release_and_reset() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    seed(&app, &fx).await;
    let id = fx.id;
    let url = format!("/reports/export/excel?startAt={D1}&endAt={D2}");
    let used = || async {
        app.scalar::<i32>(&format!(
            r#"SELECT "reportExportsUsedThisMonth" FROM company WHERE id = {id}"#
        ))
        .await
    };

    // kuota 2/bulan, reset di masa depan
    app.exec(&format!(
        r#"UPDATE company SET "reportExportsPerMonth" = 2, "reportExportsUsedThisMonth" = 0,
                  "reportExportsResetAt" = NOW() + interval '10 days' WHERE id = {id}"#
    ))
    .await;
    app.get(&url)
        .token(&fx.admin.token)
        .send()
        .await
        .assert_status(200);
    assert_eq!(used().await, 1);
    let r = app
        .get(&format!("/reports/export/pdf?startAt={D1}&endAt={D2}"))
        .token(&fx.admin.token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(used().await, 2);
    // habis -> 402 dan tidak bertambah
    let r = app.get(&url).token(&fx.admin.token).send().await;
    r.assert_status(402);
    let msg = r.json["message"].as_str().unwrap().to_owned();
    assert!(
        msg.starts_with("Export quota exceeded. You have 2 exports per month. Resets on "),
        "{msg}"
    );
    assert_eq!(used().await, 2);
    app.get(&format!("/reports/export/pdf?startAt={D1}&endAt={D2}"))
        .token(&fx.staff.token)
        .send()
        .await
        .assert_status(402);
    assert_eq!(used().await, 2);

    // pembuatan file gagal (rentang > 366 hari lolos DTO, gagal di service) -> kuota dikembalikan
    app.exec(&format!(
        r#"UPDATE company SET "reportExportsUsedThisMonth" = 0 WHERE id = {id}"#
    ))
    .await;
    app.get("/reports/export/excel?startAt=2024-01-01&endAt=2026-03-10")
        .token(&fx.admin.token)
        .send()
        .await
        .assert_status(400);
    assert_eq!(used().await, 0);

    // reset terlewat -> penghitung di-reset & reset berikutnya +1 bulan
    app.exec(&format!(
        r#"UPDATE company SET "reportExportsUsedThisMonth" = 2, "reportExportsResetAt" = NOW() - interval '1 day' WHERE id = {id}"#
    ))
    .await;
    app.get(&url)
        .token(&fx.admin.token)
        .send()
        .await
        .assert_status(200);
    assert_eq!(used().await, 1);
    let days: i32 = app
        .scalar(&format!(
            r#"SELECT ROUND(EXTRACT(EPOCH FROM ("reportExportsResetAt" - NOW())) / 86400)::int FROM company WHERE id = {id}"#
        ))
        .await;
    assert!((27..=32).contains(&days), "{days}");
    // resetAt NULL diperlakukan kedaluwarsa
    app.exec(&format!(
        r#"UPDATE company SET "reportExportsUsedThisMonth" = 2, "reportExportsResetAt" = NULL WHERE id = {id}"#
    ))
    .await;
    app.get(&url)
        .token(&fx.manager.token)
        .send()
        .await
        .assert_status(200);
    assert_eq!(used().await, 1);

    // tak terbatas (-1)
    app.exec(&format!(
        r#"UPDATE company SET "reportExportsPerMonth" = -1, "reportExportsUsedThisMonth" = 9999 WHERE id = {id}"#
    ))
    .await;
    app.get(&url)
        .token(&fx.admin.token)
        .send()
        .await
        .assert_status(200);
    assert_eq!(used().await, 10_000);
    // kuota 0 -> selalu 402
    app.exec(&format!(r#"UPDATE company SET "reportExportsPerMonth" = 0, "reportExportsUsedThisMonth" = 0 WHERE id = {id}"#)).await;
    app.get(&url)
        .token(&fx.admin.token)
        .send()
        .await
        .assert_status(402);
    // tanpa token
    app.get(&url).send().await.assert_status(401);
    let _ = json!(null);
}
