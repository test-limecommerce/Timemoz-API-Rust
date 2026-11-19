//! Uji integrasi `GET /v1/quickstart` dan `POST /v1/quickstart/finish` terhadap Postgres nyata.
//! Jalankan: `MZ_TEST_DATABASE_URL=... MZ_TEST_REQUIRE_DB=1 cargo test -p mz-misc --test it_misc_quickstart`.
//!
//! Company fixture (`app.company()`) dipakai bersama, jadi test berjalan berurutan dan setiap
//! test men-reset `companyQuickstart` serta `moduleSalesTrackerEnabled` ke keadaan yang ia butuhkan.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use axum::Router;
use mz_core::AppState;
use mz_testkit::{Company, TestApp};
use serde_json::{Value, json};
use tokio::sync::Mutex;

/// Semua test memakai company fixture yang sama: dijalankan berurutan.
static SERIAL: Mutex<()> = Mutex::const_new(());

const ALL_STAMPS: &[&str] = &[
    "inviteTeamAt",
    "companySettingAt",
    "manageRegionAt",
    "manageCustomerAt",
    "manageStoreAt",
    "visitPlanAt",
];

fn build(state: &AppState) -> Router {
    mz_misc::router(state).with_state(state.clone())
}

/// Atur `companyQuickstart` company fixture: baris dengan stempel `stamps` (atau tanpa baris bila
/// `None`), dan `moduleSalesTrackerEnabled`.
async fn reset(app: &TestApp, company: &Company, stamps: Option<&[&str]>, tracker: bool) {
    let company_id = company.id;
    app.exec(&format!(
        r#"DELETE FROM "companyQuickstart" WHERE "companyId" = {company_id};
           UPDATE company SET "moduleSalesTrackerEnabled" = {tracker} WHERE id = {company_id};"#
    ))
    .await;
    if let Some(stamps) = stamps {
        let mut sets = vec![r#""updatedAt" = NOW()"#.to_owned()];
        sets.extend(stamps.iter().map(|c| format!(r#""{c}" = NOW()"#)));
        app.exec(&format!(
            r#"INSERT INTO "companyQuickstart" ("companyId", "updatedAt") VALUES ({company_id}, NOW());
               UPDATE "companyQuickstart" SET {} WHERE "companyId" = {company_id};"#,
            sets.join(", ")
        ))
        .await;
    }
}

fn step<'a>(data: &'a Value, key: &str) -> &'a Value {
    data["steps"]
        .as_array()
        .and_then(|a| a.iter().find(|s| s["key"] == key))
        .unwrap_or_else(|| panic!("langkah {key} tidak ada: {data}"))
}

#[tokio::test]
async fn status_tanpa_token_401() {
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let _g = SERIAL.lock().await;
    app.get("/v1/quickstart").send().await.assert_status(401);
}

#[tokio::test]
async fn status_company_tanpa_baris_tidak_berlaku() {
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let _g = SERIAL.lock().await;
    let c = app.company().await;
    reset(&app, &c, None, true).await;

    let res = app.get("/v1/quickstart").token(&c.staff.token).send().await;
    res.assert_ok().assert_no_sql_error();
    assert_eq!(res.json["statusCode"], 200);
    assert_eq!(res.json["message"], "OK");
    assert_eq!(
        res.data(),
        &json!({
            "applicable": false,
            "completed": true,
            "completedAt": null,
            "percent": 100,
            "steps": []
        })
    );
}

#[tokio::test]
async fn status_sales_tracker_mati_tidak_berlaku_dan_finish_404() {
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let _g = SERIAL.lock().await;
    let c = app.company().await;
    reset(&app, &c, Some(ALL_STAMPS), false).await;

    let res = app.get("/v1/quickstart").token(&c.admin.token).send().await;
    res.assert_ok();
    assert_eq!(res.data()["applicable"], false);

    let res = app
        .post("/v1/quickstart/finish")
        .token(&c.admin.token)
        .send()
        .await;
    res.assert_status(404);
    assert_eq!(
        res.json,
        json!({
            "code": "QUICKSTART_NOT_APPLICABLE",
            "message": "Quickstart is not applicable to this company"
        })
    );
}

#[tokio::test]
async fn status_menstempel_langkah_yang_terpenuhi_dan_tidak_mundur() {
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let _g = SERIAL.lock().await;
    let c = app.company().await;
    reset(&app, &c, Some(&["manageStoreAt"]), true).await;

    let first = app.get("/v1/quickstart").token(&c.staff.token).send().await;
    first.assert_ok().assert_no_sql_error();
    let data = first.data();
    assert_eq!(data["applicable"], true);
    assert_eq!(data["steps"].as_array().map(Vec::len), Some(7));
    // Langkah yang sudah ber-stempel tetap bertanggal yang sama.
    assert_eq!(step(data, "MANAGE_STORE")["completed"], true);
    let stored = step(data, "MANAGE_STORE")["completedAt"].clone();
    assert!(
        stored.as_str().is_some_and(|s| s.ends_with('Z')),
        "{stored}"
    );

    // INVITE_TEAM: stempel terisi hanya kalau company punya lebih dari satu anggota.
    let multi: bool = app
        .scalar(&format!(
            r#"SELECT COUNT(*) > 1 FROM "userCompany" WHERE "companyId" = {}"#,
            c.id
        ))
        .await;
    assert_eq!(step(data, "INVITE_TEAM")["completed"], multi);

    // Pemanggilan kedua tidak mengubah stempel yang sudah ada.
    let second = app.get("/v1/quickstart").token(&c.staff.token).send().await;
    second.assert_ok();
    assert_eq!(step(second.data(), "MANAGE_STORE")["completedAt"], stored);
    assert_eq!(second.data()["completed"], false);
}

#[tokio::test]
async fn finish_401_untuk_staff_karena_role_tidak_cocok() {
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let _g = SERIAL.lock().await;
    let c = app.company().await;
    reset(&app, &c, Some(ALL_STAMPS), true).await;
    // RolesGuard Node membalas 401 (bukan 403) untuk role yang tidak diizinkan.
    let res = app
        .post("/v1/quickstart/finish")
        .token(&c.staff.token)
        .send()
        .await;
    res.assert_status(401);
    assert_eq!(
        res.json["message"],
        "Access denied: insufficient permissions"
    );
}

#[tokio::test]
async fn finish_409_menyebut_langkah_yang_kurang() {
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let _g = SERIAL.lock().await;
    let c = app.company().await;
    // Semua enam stempel kecuali VISIT_PLAN, dan tanpa storeVisitPlan agar pengecekannya false.
    let stamps: Vec<&str> = ALL_STAMPS
        .iter()
        .copied()
        .filter(|s| *s != "visitPlanAt")
        .collect();
    reset(&app, &c, Some(&stamps), true).await;
    app.exec(&format!(
        r#"DELETE FROM "storeVisitPlan" WHERE "companyId" = {}"#,
        c.id
    ))
    .await;

    let res = app
        .post("/v1/quickstart/finish")
        .token(&c.admin.token)
        .send()
        .await;
    res.assert_status(409).assert_no_sql_error();
    assert_eq!(
        res.json,
        json!({
            "code": "QUICKSTART_STEPS_INCOMPLETE",
            "message": "Quickstart steps are not complete",
            "missing": ["VISIT_PLAN"]
        })
    );
    let finished: bool = app
        .scalar(&format!(
            r#"SELECT "finishAt" IS NOT NULL FROM "companyQuickstart" WHERE "companyId" = {}"#,
            c.id
        ))
        .await;
    assert!(!finished, "finishAt tidak boleh terstempel saat 409");
}

#[tokio::test]
async fn finish_sukses_201_lalu_idempoten() {
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let _g = SERIAL.lock().await;
    let c = app.company().await;
    reset(&app, &c, Some(ALL_STAMPS), true).await;

    // Bawaan Nest untuk POST tanpa @HttpCode: 201. `statusCode` di body tetap 200.
    let res = app
        .post("/v1/quickstart/finish")
        .token(&c.admin.token)
        .send()
        .await;
    res.assert_status(201).assert_no_sql_error();
    assert_eq!(res.json["statusCode"], 200);
    assert_eq!(res.data()["completed"], true);
    assert_eq!(res.data()["percent"], 100);
    assert_eq!(step(res.data(), "FINISH")["completed"], true);
    let first_at = res.data()["completedAt"].clone();
    assert!(first_at.is_string(), "{first_at}");

    // Pemanggilan kedua: 201, tanpa mengubah completedAt.
    let again = app
        .post("/v1/quickstart/finish")
        .token(&c.admin.token)
        .send()
        .await;
    again.assert_status(201);
    assert_eq!(again.data()["completedAt"], first_at);

    // GET setelah selesai juga memakai status yang sama (tanpa evaluasi ulang).
    let status = app.get("/v1/quickstart").token(&c.staff.token).send().await;
    status.assert_ok();
    assert_eq!(status.data()["completed"], true);
    assert_eq!(status.data()["completedAt"], first_at);
}
