//! Uji integrasi DB nyata: reports (9 route) — laporan JSON, export PDF/Excel, share-report
//! (termasuk route publik) dan validasi `POST /reports/send`.
//! Tidak tercakup: pengiriman email `POST /reports/send` (Mailtrap = jaringan luar) dan
//! streaming gambar share (GCS) selain otorisasi DB + jalur gagal.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use common::{D1, D2, assert_gcs_failure, gcs_env_present, seed, shot_of};
use mz_testkit::{TestApp, TestResponse};
use serde_json::{Value, json};

fn uniq(prefix: &str) -> String {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("{prefix} {n:x}")
}

fn ctype(r: &TestResponse) -> String {
    r.headers
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn report_json_totals_and_filters() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let s = seed(&app, &fx).await;
    let base = format!("/reports?startAt={D1}&endAt={D2}");

    // user tunggal: 2 hari x (A 3600s billable[null -> project] + B 1800s non-billable)
    let q = format!("{base}&user={}", fx.admin.id);
    let r = app.get(&q).token(&fx.admin.token).send().await;
    r.assert_ok();
    let d = r.data();
    assert_eq!(d["totalTakenInSecond"], json!(10_800), "{}", r.json);
    assert_eq!(d["billableSeconds"], json!(7_200));
    assert_eq!(d["nonBillableSeconds"], json!(3_600));
    assert_eq!(d["billablePercentage"], json!(67));
    assert_eq!(d["timelines"].as_array().unwrap().len(), 2);
    assert_eq!(d["trackingEmployees"].as_array().unwrap().len(), 1);
    let tr = d["trackings"].as_array().unwrap();
    assert_eq!(tr.len(), 4);
    assert!(
        tr.iter()
            .any(|t| t["screenshot"].as_array().is_some_and(|a| !a.is_empty()))
    );
    assert!(d["trackingProjects"][0]["project"]["name"].is_string());
    assert!(d["trackingNotes"].is_array() && d["projectSecond"].is_array());

    // filter billable / non-billable / project
    let r = app
        .get(&format!("{q}&billableFilter=billable"))
        .token(&fx.admin.token)
        .send()
        .await;
    r.assert_ok();
    assert_eq!(r.data()["totalTakenInSecond"], json!(7_200));
    let r = app
        .get(&format!("{q}&billableFilter=non-billable"))
        .token(&fx.admin.token)
        .send()
        .await;
    assert_eq!(r.data()["totalTakenInSecond"], json!(3_600));
    app.get(&format!("{q}&billableFilter=all"))
        .token(&fx.admin.token)
        .send()
        .await
        .assert_ok();
    let r = app
        .get(&format!("{q}&project={}", s.project_id))
        .token(&fx.admin.token)
        .send()
        .await;
    assert_eq!(r.data()["totalTakenInSecond"], json!(10_800));
    // dua user (daftar dipisah spasi)
    let r = app
        .get(&format!("{base}&user={}%20{}", fx.staff.id, fx.staff2.id))
        .token(&fx.manager.token)
        .send()
        .await;
    assert_eq!(r.data()["totalTakenInSecond"], json!(21_600));
    assert_eq!(r.data()["trackingEmployees"].as_array().unwrap().len(), 2);
    // tanpa filter user: company-wide, minimal data seed seluruh fixture
    let r = app.get(&base).token(&fx.super_admin.token).send().await;
    r.assert_ok();
    assert!(r.data()["totalTakenInSecond"].as_i64().unwrap() >= 54_000);

    // staff (scope self): meminta user lain tetap hanya data sendiri
    let r = app
        .get(&format!("{base}&user={}", fx.admin.id))
        .token(&fx.staff.token)
        .send()
        .await;
    r.assert_ok();
    assert_eq!(r.data()["totalTakenInSecond"], json!(10_800));
    let emp = &r.data()["trackingEmployees"];
    assert_eq!(emp.as_array().unwrap().len(), 1);
    assert_eq!(emp[0]["user"]["id"], json!(fx.staff.id));

    // validasi
    app.get("/reports")
        .token(&fx.admin.token)
        .send()
        .await
        .assert_status(400);
    app.get("/reports?startAt=2026-03-12&endAt=2026-03-10")
        .token(&fx.admin.token)
        .send()
        .await
        .assert_status(400);
    app.get("/reports?startAt=2024-01-01&endAt=2026-03-10")
        .token(&fx.admin.token)
        .send()
        .await
        .assert_status(400);
    app.get(&format!("{base}&billableFilter=zzz"))
        .token(&fx.admin.token)
        .send()
        .await
        .assert_status(400);
    app.get(&format!("{base}&userUuid=bad"))
        .token(&fx.admin.token)
        .send()
        .await
        .assert_status(400);
    app.get(&base).send().await.assert_status(401);
}

#[tokio::test]
async fn report_exports_excel_and_pdf() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    seed(&app, &fx).await;
    let base = format!("startAt={D1}&endAt={D2}");

    for u in [&fx.admin, &fx.manager, &fx.staff] {
        let r = app
            .get(&format!("/reports/export/excel?{base}"))
            .token(&u.token)
            .send()
            .await;
        r.assert_status(200);
        assert_eq!(
            ctype(&r),
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
        );
        assert!(
            r.bytes.len() > 100 && r.bytes.starts_with(b"PK"),
            "xlsx len {}",
            r.bytes.len()
        );
        let cd = r
            .headers
            .get("content-disposition")
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        assert!(cd.contains(&format!("Report-{D1}-to-{D2}.xlsx")), "{cd}");

        let r = app
            .get(&format!("/reports/export/pdf?{base}"))
            .token(&u.token)
            .send()
            .await;
        r.assert_status(200);
        assert_eq!(ctype(&r), "application/pdf");
        assert!(
            r.bytes.len() > 500 && r.bytes.starts_with(b"%PDF"),
            "pdf len {}",
            r.bytes.len()
        );
        let cd = r
            .headers
            .get("content-disposition")
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        assert!(cd.contains(&format!("Report-{D1}-to-{D2}.pdf")), "{cd}");
    }
    // filter lengkap
    let q = format!(
        "{base}&user={}&project=1&billableFilter=billable",
        fx.staff.id
    );
    let r = app
        .get(&format!("/reports/export/pdf?{q}"))
        .token(&fx.admin.token)
        .send()
        .await;
    r.assert_status(200);
    assert!(r.bytes.starts_with(b"%PDF"));
    let r = app
        .get(&format!("/reports/export/excel?{q}"))
        .token(&fx.admin.token)
        .send()
        .await;
    r.assert_status(200);
    assert!(r.bytes.starts_with(b"PK"));
    // periode tanpa data tetap menghasilkan file
    let r = app
        .get("/reports/export/pdf?startAt=2020-01-01&endAt=2020-01-02")
        .token(&fx.admin.token)
        .send()
        .await;
    r.assert_status(200);
    assert!(!r.bytes.is_empty());
    // validasi
    app.get("/reports/export/excel")
        .token(&fx.admin.token)
        .send()
        .await
        .assert_status(400);
    app.get("/reports/export/pdf")
        .token(&fx.admin.token)
        .send()
        .await
        .assert_status(400);
    app.get(&format!("/reports/export/pdf?{base}"))
        .send()
        .await
        .assert_status(401);
}

async fn create_share(
    app: &TestApp,
    token: &str,
    name: &str,
    employees: &[i32],
    projects: &[i32],
) -> TestResponse {
    app.post("/reports/create/share")
        .token(token)
        .json(&json!({"name": name, "startAt": D1, "endAt": D2, "employees": employees, "projects": projects,
                      "expiresInDays": 7}))
        .send()
        .await
}

#[tokio::test]
async fn share_report_lifecycle() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let s = seed(&app, &fx).await;
    let name = uniq("it-share");

    let r = create_share(
        &app,
        &fx.admin.token,
        &name,
        &[fx.staff.id, fx.staff2.id],
        &[s.project_id],
    )
    .await;
    r.assert_ok();
    let uuid = r.data()["uuid"].as_str().unwrap().to_owned();
    assert_eq!(r.data()["name"], json!(name));
    assert_eq!(r.data()["userId"], json!(fx.admin.id));
    assert_eq!(r.data()["companyId"], json!(fx.id));
    let n: i64 = app
        .scalar(&format!(
            r#"SELECT (SELECT COUNT(*) FROM "shareReportUsers" WHERE "shareId" = {id}) * 10
                    + (SELECT COUNT(*) FROM "shareReportProjects" WHERE "shareId" = {id})"#,
            id = r.data()["id"]
        ))
        .await;
    assert_eq!(n, 21);

    // daftar milik pembuat
    let r = app
        .get(&format!("/reports/share?startAt={D1}&endAt={D2}"))
        .token(&fx.admin.token)
        .send()
        .await;
    r.assert_ok();
    assert!(
        r.json["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["uuid"] == json!(uuid) && x["name"] == json!(name))
    );
    // user lain tidak melihatnya
    let r = app
        .get(&format!("/reports/share?startAt={D1}&endAt={D2}"))
        .token(&fx.staff.token)
        .send()
        .await;
    r.assert_ok();
    assert!(
        !r.json["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["uuid"] == json!(uuid))
    );
    // query tidak valid
    app.get("/reports/share")
        .token(&fx.admin.token)
        .send()
        .await
        .assert_status(400);
    app.get("/reports/share?startAt=2026/03/10&endAt=2026-03-11")
        .token(&fx.admin.token)
        .send()
        .await
        .assert_status(400);

    // route PUBLIK (tanpa token)
    let r = app.get(&format!("/reports/share/{uuid}")).send().await;
    r.assert_ok();
    let d = r.data();
    assert_eq!(d["uuid"], json!(uuid));
    assert_eq!(d["totalTakenInSecond"], json!(21_600), "{}", r.json);
    assert_eq!(d["users"].as_array().unwrap().len(), 2);
    assert_eq!(d["projects"].as_array().unwrap().len(), 1);
    assert_eq!(d["trackingEmployees"].as_array().unwrap().len(), 2);
    assert!(d["trackingEmployees"][0]["absent"].is_number());
    assert_eq!(d["timelines"].as_array().unwrap().len(), 2);
    let shots = d["trackingProjects"][0]["screenshots"].as_array().unwrap();
    assert!(!shots.is_empty());
    assert!(
        shots[0]["url"]
            .as_str()
            .unwrap()
            .starts_with(&format!("/reports/share/{uuid}/screenshots/"))
    );
    // tidak ada / format salah
    let r = app
        .get("/reports/share/00000000-0000-4000-8000-0000000000aa")
        .send()
        .await;
    assert_eq!(r.json["success"], json!(false));
    app.get("/reports/share/not-uuid")
        .send()
        .await
        .assert_status(400);

    // proxy gambar share (publik): otorisasi DB, lalu GCS gagal
    if !gcs_env_present() {
        let ok_shot = shot_of(&app, fx.staff.id).await;
        let r = app
            .get(&format!("/reports/share/{uuid}/screenshots/{ok_shot}"))
            .send()
            .await;
        assert_gcs_failure(&r);
        assert_ne!(r.status.as_u16(), 404);
        // screenshot admin bukan bagian share -> 404
        let other = shot_of(&app, fx.admin.id).await;
        app.get(&format!("/reports/share/{uuid}/screenshots/{other}"))
            .send()
            .await
            .assert_status(404);
        app.get(&format!(
            "/reports/share/00000000-0000-4000-8000-0000000000aa/screenshots/{ok_shot}"
        ))
        .send()
        .await
        .assert_status(404);
        app.get(&format!("/reports/share/{uuid}/screenshots/bad"))
            .send()
            .await
            .assert_status(400);
    }

    // hapus: user lain (staff) -> 404; manager (company-wide) boleh
    app.delete(&format!("/reports/share/{uuid}"))
        .token(&fx.staff.token)
        .send()
        .await
        .assert_status(404);
    app.delete(&format!("/reports/share/{uuid}"))
        .send()
        .await
        .assert_status(401);
    let r = app
        .delete(&format!("/reports/share/{uuid}"))
        .token(&fx.manager.token)
        .send()
        .await;
    r.assert_ok();
    assert_eq!(r.json["message"], json!("Share report revoked"));
    let c: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "shareReport" WHERE uuid = '{uuid}'"#
        ))
        .await;
    assert_eq!(c, 0);
    app.delete(&format!("/reports/share/{uuid}"))
        .token(&fx.manager.token)
        .send()
        .await
        .assert_status(404);
    let r = app.get(&format!("/reports/share/{uuid}")).send().await;
    assert_eq!(r.json["success"], json!(false));
    app.delete("/reports/share/zzz")
        .token(&fx.admin.token)
        .send()
        .await
        .assert_status(400);
}

#[tokio::test]
async fn share_report_permissions_and_expiry() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let s = seed(&app, &fx).await;

    // staff bawaan tidak punya izin WRITE reports (gate core) -> 403 PERMISSION_DENIED
    let r = create_share(&app, &fx.staff.token, &uniq("it-own"), &[], &[]).await;
    r.assert_status(403);
    assert_eq!(r.json["code"], json!("PERMISSION_DENIED"));
    // beri izin WRITE pada role staff agar cabang "hanya data sendiri" terjangkau
    app.exec(&format!(
        r#"INSERT INTO "rolePermission" ("accessLevelId", "featureKey", action)
           SELECT DISTINCT ual."accessLevelId", 'reports', 'WRITE'::"PermissionAction" FROM "userAccessLevel" ual
           WHERE ual."userId" = {} AND ual.status = 1 AND ual."companyId" = {}
           ON CONFLICT DO NOTHING"#,
        fx.staff.id, fx.id
    ))
    .await;
    let app = TestApp::new(mz_server::app).await.unwrap();
    let r = create_share(
        &app,
        &fx.staff.token,
        &uniq("it-own"),
        &[fx.staff2.id],
        &[s.project_id],
    )
    .await;
    r.assert_status(403);
    assert_eq!(
        r.json["code"],
        json!("SHARE_OTHERS_FORBIDDEN"),
        "{}",
        r.json
    );
    let r = create_share(&app, &fx.staff.token, &uniq("it-own"), &[], &[]).await;
    r.assert_ok();
    let uuid = r.data()["uuid"].as_str().unwrap().to_owned();
    let users: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "shareReportUsers" sru JOIN "shareReport" sr ON sr.id = sru."shareId"
               WHERE sr.uuid = '{uuid}' AND sru."userId" = {}"#,
            fx.staff.id
        ))
        .await;
    assert_eq!(users, 1);
    // staff tersebut melihat data miliknya lewat link publik (hanya dirinya)
    let r = app.get(&format!("/reports/share/{uuid}")).send().await;
    r.assert_ok();
    assert_eq!(r.data()["trackingEmployees"].as_array().unwrap().len(), 1);
    // pemilik boleh hapus
    app.delete(&format!("/reports/share/{uuid}"))
        .token(&fx.staff.token)
        .send()
        .await
        .assert_ok();

    // anggota/proyek tidak valid
    create_share(
        &app,
        &fx.admin.token,
        &uniq("it-bad"),
        &[2_000_000_000],
        &[],
    )
    .await
    .assert_status(400);
    create_share(
        &app,
        &fx.admin.token,
        &uniq("it-bad"),
        &[],
        &[2_000_000_000],
    )
    .await
    .assert_status(400);
    // validasi DTO
    app.post("/reports/create/share")
        .token(&fx.admin.token)
        .json(&json!({}))
        .send()
        .await
        .assert_status(400);
    app.post("/reports/create/share")
        .token(&fx.admin.token)
        .json(&json!({"name": "x", "startAt": "2024-01-01", "endAt": "2026-01-01", "projects": [], "employees": []}))
        .send()
        .await
        .assert_status(400);
    app.post("/reports/create/share")
        .json(&json!({}))
        .send()
        .await
        .assert_status(401);

    // kedaluwarsa & tanpa expiresInDays (default 365 hari)
    let r = app
        .post("/reports/create/share")
        .token(&fx.admin.token)
        .json(&json!({"name": uniq("it-exp"), "startAt": D1, "endAt": D2, "projects": [], "employees": [fx.staff.id]}))
        .send()
        .await;
    r.assert_ok();
    let uuid = r.data()["uuid"].as_str().unwrap().to_owned();
    let days: i32 = app
        .scalar(&format!(
            r#"SELECT ROUND(EXTRACT(EPOCH FROM ("expiresAt" - NOW())) / 86400)::int FROM "shareReport" WHERE uuid = '{uuid}'"#
        ))
        .await;
    assert_eq!(days, 365);
    app.get(&format!("/reports/share/{uuid}"))
        .send()
        .await
        .assert_ok();
    app.exec(&format!(
        r#"UPDATE "shareReport" SET "expiresAt" = NOW() - interval '1 hour' WHERE uuid = '{uuid}'"#
    ))
    .await;
    let r = app.get(&format!("/reports/share/{uuid}")).send().await;
    assert_eq!(r.json["success"], json!(false));
    assert_eq!(r.json["message"], json!("Share report not found"));
    // share tanpa companyId (data lama) dianggap tidak ada
    app.exec(&format!(
        r#"UPDATE "shareReport" SET "expiresAt" = NOW() + interval '1 day', "companyId" = NULL WHERE uuid = '{uuid}'"#
    ))
    .await;
    let r = app.get(&format!("/reports/share/{uuid}")).send().await;
    assert_eq!(r.json["success"], json!(false));
    // super admin pemilik lain bisa mencabutnya? companyId NULL -> tidak ada akses lintas user
    app.delete(&format!("/reports/share/{uuid}"))
        .token(&fx.manager.token)
        .send()
        .await
        .assert_status(404);
    app.delete(&format!("/reports/share/{uuid}"))
        .token(&fx.admin.token)
        .send()
        .await
        .assert_ok();
}

#[tokio::test]
async fn share_public_rate_limit() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let ip = format!("10.250.{}.{}", std::process::id() % 250, 7);
    let mut last = 0;
    for _ in 0..32 {
        last = app
            .get("/reports/share/00000000-0000-4000-8000-0000000000bb")
            .ip(&ip)
            .send()
            .await
            .status
            .as_u16();
    }
    assert_eq!(last, 429);
}

#[tokio::test]
async fn send_report_gates_and_validation() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    seed(&app, &fx).await;
    let body = json!({"emails": ["a@example.com"], "subject": "s", "message": "m", "startDate": D1, "endDate": D2});
    app.post("/reports/send")
        .json(&body)
        .send()
        .await
        .assert_status(401);
    // STAFF tidak diizinkan
    app.post("/reports/send")
        .token(&fx.staff.token)
        .json(&body)
        .send()
        .await
        .assert_status(401);
    // validasi DTO
    app.post("/reports/send")
        .token(&fx.admin.token)
        .json(&json!({}))
        .send()
        .await
        .assert_status(400);
    // plan tanpa clientProofExportAllowed -> 402 (sebelum email apa pun dikirim)
    app.exec(&format!(
        r#"UPDATE company SET "clientProofExportAllowed" = false WHERE id = {}"#,
        fx.id
    ))
    .await;
    let r = app
        .post("/reports/send")
        .token(&fx.admin.token)
        .json(&body)
        .send()
        .await;
    r.assert_status(402);
    assert!(
        r.json["message"]
            .as_str()
            .unwrap()
            .contains("not available on your current plan")
    );
    // SQL pembantu send_report_inner (daftar user/proyek company) divalidasi langsung terhadap skema;
    // pengiriman Mailtrap sendiri tidak dijalankan (jaringan luar).
    let users: Vec<i32> = sqlx::query_scalar(
        r#"SELECT "userId" FROM "userCompany" WHERE "companyId" = $1 AND status = 1"#,
    )
    .bind(fx.id)
    .fetch_all(app.db())
    .await
    .unwrap();
    assert!(!users.is_empty());
    let projects: Vec<i32> = sqlx::query_scalar(
        r#"SELECT "projectId" FROM "projectCompany" WHERE "companyId" = $1 AND status = 1"#,
    )
    .bind(fx.id)
    .fetch_all(app.db())
    .await
    .unwrap();
    assert!(!projects.is_empty());
    let _: Value = json!(null);
}

/// Data produksi teranonimkan (baris asli dengan kolom NULL seperti `startAtUtc`/`timezone`/`billable`).
#[tokio::test]
async fn report_over_real_template_data() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let q = "startAt=2025-11-17&endAt=2026-11-16";
    let r = app
        .get(&format!("/reports?{q}"))
        .token(&fx.super_admin.token)
        .send()
        .await;
    r.assert_ok();
    assert!(r.data()["trackings"].is_array());
    let r = app
        .get(&format!("/reports?{q}&billableFilter=non-billable"))
        .token(&fx.super_admin.token)
        .send()
        .await;
    r.assert_ok();
    let r = app
        .get(&format!("/reports/export/excel?{q}"))
        .token(&fx.super_admin.token)
        .send()
        .await;
    r.assert_status(200);
    assert!(r.bytes.starts_with(b"PK"));
    let r = app
        .get(&format!("/reports/export/pdf?{q}"))
        .token(&fx.super_admin.token)
        .send()
        .await;
    r.assert_status(200);
    assert!(r.bytes.starts_with(b"%PDF"));
    // staff (scope self) atas rentang yang sama
    app.get(&format!("/reports?{q}"))
        .token(&fx.staff.token)
        .send()
        .await
        .assert_ok();
    // activity log pada hari yang punya data app asli
    let day: Option<String> = sqlx::query_scalar(
        r#"SELECT to_char(a."startAt", 'YYYY-MM-DD') FROM app a
           JOIN "trackingTimeline" tt ON tt.id = a."trackingTimelineId"
           JOIN tracking tr ON tr.id = tt."trackingId"
           JOIN "userCompany" uc ON uc."userId" = tr."userId" AND uc."companyId" = $1 AND uc.status = 1
           WHERE tt.note NOT LIKE 'itseed %' GROUP BY 1 ORDER BY COUNT(*) DESC LIMIT 1"#,
    )
    .bind(fx.id)
    .fetch_optional(app.db())
    .await
    .unwrap();
    if let Some(day) = day {
        let uid: i32 = app
            .scalar(&format!(
                r#"SELECT tr."userId" FROM app a JOIN "trackingTimeline" tt ON tt.id = a."trackingTimelineId"
                   JOIN tracking tr ON tr.id = tt."trackingId"
                   WHERE to_char(a."startAt", 'YYYY-MM-DD') = '{day}' AND tt.note NOT LIKE 'itseed %' LIMIT 1"#
            ))
            .await;
        let uuid: String = app
            .scalar(&format!("SELECT uuid::text FROM users WHERE id = {uid}"))
            .await;
        let r = app
            .get(&format!("/activity-logs?takenDate={day}&user_uuid={uuid}"))
            .token(&fx.super_admin.token)
            .send()
            .await;
        r.assert_no_sql_error();
        let r = app
            .get(&format!(
                "/activity-logs/export/csv?takenDate={day}&user_uuid={uuid}"
            ))
            .token(&fx.super_admin.token)
            .send()
            .await;
        r.assert_no_sql_error();
    }
}
