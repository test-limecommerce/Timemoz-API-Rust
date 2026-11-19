//! Uji integrasi varian scope divisi (leader / manager / staff) untuk screenshots,
//! activity-logs dan reports. Binary sendiri = database sendiri, jadi mengaktifkan
//! divisi tidak mengganggu test lain. Leader = `staff`, anggota = `staff2`; `admin` di luar divisi.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use common::{D1, D2, assert_gcs_failure, gcs_env_present, seed, shot_of};
use mz_testkit::{Company, TestApp};
use serde_json::json;

async fn setup(app: &TestApp, fx: &Company) {
    seed(app, fx).await;
    app.exec(&format!(
        r#"
SELECT pg_advisory_xact_lock(7770002);
UPDATE company SET "divisionsEnabled" = true WHERE id = {cid};
INSERT INTO "companyPlanOverride" ("companyId", divisions) VALUES ({cid}, true)
  ON CONFLICT ("companyId") DO UPDATE SET divisions = true;
INSERT INTO division (name, "companyId")
  SELECT 'it-div-main', {cid} WHERE NOT EXISTS (SELECT 1 FROM division WHERE "companyId" = {cid} AND name = 'it-div-main' AND "deletedAt" IS NULL);
INSERT INTO "divisionMember" ("companyId", "divisionId", "userId", role)
  SELECT {cid}, d.id, {leader}, 'LEADER' FROM division d
  WHERE d."companyId" = {cid} AND d.name = 'it-div-main' AND d."deletedAt" IS NULL
    AND NOT EXISTS (SELECT 1 FROM "divisionMember" m WHERE m."divisionId" = d.id AND m."userId" = {leader} AND m."deletedAt" IS NULL);
INSERT INTO "divisionMember" ("companyId", "divisionId", "userId", role)
  SELECT {cid}, d.id, {member}, 'MEMBER' FROM division d
  WHERE d."companyId" = {cid} AND d.name = 'it-div-main' AND d."deletedAt" IS NULL
    AND NOT EXISTS (SELECT 1 FROM "divisionMember" m WHERE m."divisionId" = d.id AND m."userId" = {member} AND m."deletedAt" IS NULL);
"#,
        cid = fx.id,
        leader = fx.staff.id,
        member = fx.staff2.id,
    ))
    .await;
}

#[tokio::test]
async fn reports_division_scope() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    setup(&app, &fx).await;
    let base = format!("startAt={D1}&endAt={D2}");

    // leader tanpa filter: seluruh anggota divisi (staff + staff2), bukan admin
    let r = app
        .get(&format!("/reports?{base}"))
        .token(&fx.staff.token)
        .send()
        .await;
    r.assert_ok();
    let emp = r.data()["trackingEmployees"].as_array().unwrap();
    let ids: Vec<i64> = emp
        .iter()
        .map(|e| e["user"]["id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids.len(), 2, "{ids:?}");
    assert!(ids.contains(&i64::from(fx.staff.id)) && ids.contains(&i64::from(fx.staff2.id)));
    assert_eq!(r.data()["totalTakenInSecond"], json!(21_600));
    // leader meminta anggota divisi -> boleh; di luar divisi -> 403 DIVISION_SCOPE_FORBIDDEN
    let r = app
        .get(&format!("/reports?{base}&user={}", fx.staff2.id))
        .token(&fx.staff.token)
        .send()
        .await;
    r.assert_ok();
    assert_eq!(r.data()["totalTakenInSecond"], json!(10_800));
    let r = app
        .get(&format!("/reports?{base}&user={}", fx.admin.id))
        .token(&fx.staff.token)
        .send()
        .await;
    r.assert_status(403);
    assert_eq!(
        r.json["code"],
        json!("DIVISION_SCOPE_FORBIDDEN"),
        "{}",
        r.json
    );
    // anggota biasa: hanya dirinya
    let r = app
        .get(&format!("/reports?{base}&user={}", fx.staff.id))
        .token(&fx.staff2.token)
        .send()
        .await;
    r.assert_ok();
    assert_eq!(r.data()["trackingEmployees"].as_array().unwrap().len(), 1);
    assert_eq!(
        r.data()["trackingEmployees"][0]["user"]["id"],
        json!(fx.staff2.id)
    );
    // manager: company-wide
    let r = app
        .get(&format!(
            "/reports?{base}&user={}%20{}",
            fx.admin.id, fx.staff.id
        ))
        .token(&fx.manager.token)
        .send()
        .await;
    r.assert_ok();
    assert_eq!(r.data()["trackingEmployees"].as_array().unwrap().len(), 2);

    // export: leader (scope divisi) & di luar divisi
    for kind in ["excel", "pdf"] {
        let r = app
            .get(&format!("/reports/export/{kind}?{base}"))
            .token(&fx.staff.token)
            .send()
            .await;
        r.assert_status(200);
        assert!(!r.bytes.is_empty());
        let r = app
            .get(&format!(
                "/reports/export/{kind}?{base}&user={}",
                fx.admin.id
            ))
            .token(&fx.staff.token)
            .send()
            .await;
        r.assert_status(403);
        assert_eq!(r.json["code"], json!("DIVISION_SCOPE_FORBIDDEN"));
        app.get(&format!(
            "/reports/export/{kind}?{base}&user={}",
            fx.staff2.id
        ))
        .token(&fx.staff.token)
        .send()
        .await
        .assert_status(200);
    }
}

#[tokio::test]
async fn screenshots_division_scope() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    setup(&app, &fx).await;
    let member_shot = shot_of(&app, fx.staff2.id).await;
    let leader_shot = shot_of(&app, fx.staff.id).await;
    let admin_shot = shot_of(&app, fx.admin.id).await;

    // leader melihat milik anggota divisi
    let r = app
        .get(&format!("/screenshots/{member_shot}"))
        .token(&fx.staff.token)
        .send()
        .await;
    r.assert_ok();
    assert_eq!(r.data()["uuid"], json!(member_shot));
    // leader terhadap admin (luar divisi) -> 403 kode scope
    let r = app
        .get(&format!("/screenshots/{admin_shot}"))
        .token(&fx.staff.token)
        .send()
        .await;
    r.assert_status(403);
    assert_eq!(
        r.json["code"],
        json!("DIVISION_SCOPE_FORBIDDEN"),
        "{}",
        r.json
    );
    // anggota biasa terhadap leader: scope self -> not found
    let r = app
        .get(&format!("/screenshots/{leader_shot}"))
        .token(&fx.staff2.token)
        .send()
        .await;
    assert_eq!(r.json["success"], json!(false));
    // manager melihat semua
    app.get(&format!("/screenshots/{admin_shot}"))
        .token(&fx.manager.token)
        .send()
        .await
        .assert_ok();

    // open-url
    let r = app
        .get(&format!("/screenshots/{member_shot}/open-url"))
        .token(&fx.staff.token)
        .send()
        .await;
    r.assert_ok();
    let r = app
        .get(&format!("/screenshots/{admin_shot}/open-url"))
        .token(&fx.staff.token)
        .send()
        .await;
    r.assert_status(403);
    assert_eq!(r.json["code"], json!("DIVISION_SCOPE_FORBIDDEN"));
    let r = app
        .get(&format!("/screenshots/{leader_shot}/open-url"))
        .token(&fx.staff2.token)
        .send()
        .await;
    assert_eq!(r.json["success"], json!(false));

    // image & image/url
    if !gcs_env_present() {
        for p in ["image", "image/url"] {
            let r = app
                .get(&format!("/screenshots/{member_shot}/{p}"))
                .token(&fx.staff.token)
                .send()
                .await;
            assert_gcs_failure(&r);
            assert!(!matches!(r.status.as_u16(), 403 | 404), "{p}: {}", r.status);
            let r = app
                .get(&format!("/screenshots/{admin_shot}/{p}"))
                .token(&fx.staff.token)
                .send()
                .await;
            r.assert_status(403);
            assert_eq!(r.json["code"], json!("DIVISION_SCOPE_FORBIDDEN"));
            app.get(&format!("/screenshots/{leader_shot}/{p}"))
                .token(&fx.staff2.token)
                .send()
                .await
                .assert_status(404);
        }
    }
    // hapus/blur tidak memakai scope divisi: leader bukan pemilik -> 404 / not found
    app.delete(&format!("/screenshots/{member_shot}"))
        .token(&fx.staff.token)
        .send()
        .await
        .assert_status(404);
    let r = app
        .put(&format!("/screenshots/blur/{member_shot}"))
        .token(&fx.staff.token)
        .send()
        .await;
    assert_eq!(r.json["message"], json!("Screenshot not found"));
}

#[tokio::test]
async fn activity_logs_division_scope() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    setup(&app, &fx).await;
    // leader memfilter anggota divisi
    let r = app
        .get(&format!(
            "/activity-logs?takenDate={D1}&user_uuid={}",
            fx.staff2.uuid
        ))
        .token(&fx.staff.token)
        .send()
        .await;
    r.assert_ok();
    assert!(r.json["total"].as_i64().unwrap() > 0);
    let r = app
        .get(&format!(
            "/activity-logs/export/csv?takenDate={D1}&user_uuid={}",
            fx.staff2.uuid
        ))
        .token(&fx.staff.token)
        .send()
        .await;
    r.assert_status(200);
    assert!(r.text().contains("ItSeedApp"));
    // di luar divisi -> 403 DIVISION_SCOPE_FORBIDDEN
    let r = app
        .get(&format!(
            "/activity-logs?takenDate={D1}&user_uuid={}",
            fx.admin.uuid
        ))
        .token(&fx.staff.token)
        .send()
        .await;
    r.assert_status(403);
    assert_eq!(
        r.json["code"],
        json!("DIVISION_SCOPE_FORBIDDEN"),
        "{}",
        r.json
    );
    // user_uuid sendiri tetap ditolak (parity Node)
    app.get(&format!(
        "/activity-logs?takenDate={D1}&user_uuid={}",
        fx.staff.uuid
    ))
    .token(&fx.staff.token)
    .send()
    .await
    .assert_status(403);
    // anggota biasa tidak boleh memfilter user
    let r = app
        .get(&format!(
            "/activity-logs?takenDate={D1}&user_uuid={}",
            fx.staff.uuid
        ))
        .token(&fx.staff2.token)
        .send()
        .await;
    r.assert_status(403);
    // manager bebas
    app.get(&format!(
        "/activity-logs?takenDate={D1}&user_uuid={}",
        fx.admin.uuid
    ))
    .token(&fx.manager.token)
    .send()
    .await
    .assert_ok();
}

#[tokio::test]
async fn share_report_division_creator() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    setup(&app, &fx).await;
    app.exec(&format!(
        r#"INSERT INTO "rolePermission" ("accessLevelId", "featureKey", action)
           SELECT DISTINCT ual."accessLevelId", 'reports', 'WRITE'::"PermissionAction" FROM "userAccessLevel" ual
           WHERE ual."userId" = {} AND ual.status = 1 AND ual."companyId" = {}
           ON CONFLICT DO NOTHING"#,
        fx.staff.id, fx.id
    ))
    .await;
    let app = TestApp::new(mz_server::app).await.unwrap();
    // leader membagikan data anggota divisi: share-report tidak memakai scope divisi -> ditolak
    let r = app
        .post("/reports/create/share")
        .token(&fx.staff.token)
        .json(&json!({"name": "it-div-share", "startAt": D1, "endAt": D2, "projects": [], "employees": [fx.staff2.id]}))
        .send()
        .await;
    r.assert_status(403);
    assert_eq!(
        r.json["code"],
        json!("SHARE_OTHERS_FORBIDDEN"),
        "{}",
        r.json
    );
    // data sendiri
    let r = app
        .post("/reports/create/share")
        .token(&fx.staff.token)
        .json(&json!({"name": "it-div-share", "startAt": D1, "endAt": D2, "projects": [], "employees": [fx.staff.id]}))
        .send()
        .await;
    r.assert_ok();
}
