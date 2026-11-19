//! Uji integrasi DB nyata: screenshots (8 route) + activity-logs (3 route).
//! Tidak tercakup: GCS (signed URL, proxy gambar, blur) — hanya DB + jalur gagal.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use common::{D1, assert_gcs_failure, gcs_env_present, seed, shot_of, timeline_of};
use mz_testkit::TestApp;
use serde_json::{Value, json};

/// Baris screenshot baru milik `owner_id` (pada timeline A D1) dengan uuid acak; mengembalikan uuid.
async fn new_shot(app: &TestApp, owner_id: i32, status: i32, is_blur: bool) -> String {
    let uuid: String = sqlx::query_scalar(
        r#"WITH n AS (SELECT gen_random_uuid() AS id)
           INSERT INTO screenshot (uuid, "fileFullpath", "isBlur", "takenAt", "trackingTimelineId", "projectId", status)
           SELECT n.id,
                  'companies/' || c.uuid || '/projects/' || p.uuid || '/users/' || u.uuid || '/screenshots/' || n.id || '.jpg',
                  $3, tt."startAt" + interval '7 minutes', tt.id, tt."projectId", $2
           FROM n, "trackingTimeline" tt
           JOIN tracking tr ON tr.id = tt."trackingId" JOIN users u ON u.id = tr."userId"
           JOIN project p ON p.id = tt."projectId"
           JOIN "userCompany" uc ON uc."userId" = u.id AND uc.status = 1
           JOIN company c ON c.id = uc."companyId"
           WHERE tr."userId" = $1 AND tt.note = 'itseed A' AND tr."takenDate" = $4::date
           ORDER BY uc."isDefault" DESC, uc.id LIMIT 1
           RETURNING uuid::text"#,
    )
    .bind(owner_id)
    .bind(status)
    .bind(is_blur)
    .bind(D1)
    .fetch_one(app.db())
    .await
    .unwrap();
    uuid
}

#[tokio::test]
async fn find_one_access_rules() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    seed(&app, &fx).await;
    let shot = shot_of(&app, fx.staff.id).await;

    // pemilik
    let r = app
        .get(&format!("/screenshots/{shot}"))
        .token(&fx.staff.token)
        .send()
        .await;
    r.assert_ok();
    assert_eq!(r.data()["uuid"], json!(shot));
    assert_eq!(r.data()["status"], json!(3));
    assert!(r.data()["takenAt"].is_string());
    assert!(r.data()["screenshotUrl"].as_str().unwrap().contains(&shot));
    // role company-wide (admin, manager, super admin)
    for u in [&fx.admin, &fx.manager, &fx.super_admin] {
        app.get(&format!("/screenshots/{shot}"))
            .token(&u.token)
            .send()
            .await
            .assert_ok();
    }
    // staff lain (scope self) -> not found (success:false, 200)
    let r = app
        .get(&format!("/screenshots/{shot}"))
        .token(&fx.staff2.token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json["success"], json!(false));
    assert!(r.json["data"].is_null());
    // uuid tak ada / format salah / tanpa token
    let r = app
        .get("/screenshots/00000000-0000-4000-8000-000000000001")
        .token(&fx.staff.token)
        .send()
        .await;
    assert_eq!(r.json["success"], json!(false));
    app.get("/screenshots/not-a-uuid")
        .token(&fx.staff.token)
        .send()
        .await
        .assert_status(400);
    app.get(&format!("/screenshots/{shot}"))
        .send()
        .await
        .assert_status(401);
    // screenshot status 0 (dihapus) tidak terlihat
    let del = new_shot(&app, fx.staff.id, 0, false).await;
    let r = app
        .get(&format!("/screenshots/{del}"))
        .token(&fx.staff.token)
        .send()
        .await;
    assert_eq!(r.json["success"], json!(false));
}

#[tokio::test]
async fn create_screenshot_inserts_row_then_gcs_fails() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    if gcs_env_present() {
        return;
    }
    let fx = app.company().await;
    let s = seed(&app, &fx).await;
    let tl = timeline_of(&app, fx.staff.id).await;
    let before: i64 = app
        .scalar(&format!(
            "SELECT COUNT(*) FROM screenshot WHERE status = 1 AND \"takenAt\" = '2026-03-10 09:30:00' AND \"fileFullpath\" LIKE '%{}%'",
            fx.staff.uuid
        ))
        .await;
    let body = json!({"takenAt": "2026-03-10T09:30:00.000Z", "isBlur": true,
        "projectUuid": s.project_uuid, "trackingTimelineUuid": tl});
    let r = app
        .post("/screenshots")
        .token(&fx.staff.token)
        .json(&body)
        .send()
        .await;
    r.assert_status(200);
    // GCS tidak ada kredensial: penerbitan presigned URL gagal, tetapi INSERT sudah dieksekusi.
    assert_eq!(r.json["success"], json!(false));
    assert_eq!(r.json["message"], json!("register screenshot failed"));
    let after: i64 = app
        .scalar(&format!(
            "SELECT COUNT(*) FROM screenshot WHERE status = 1 AND \"takenAt\" = '2026-03-10 09:30:00' AND \"fileFullpath\" LIKE '%{}%'",
            fx.staff.uuid
        ))
        .await;
    assert_eq!(after, before + 1);
    // timeline milik user lain -> not found
    let r = app
        .post("/screenshots")
        .token(&fx.staff2.token)
        .json(&body)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json["message"], json!("Tracking timeline not found"));
    // validasi DTO
    let r = app
        .post("/screenshots")
        .token(&fx.staff.token)
        .json(&json!({}))
        .send()
        .await;
    r.assert_status(400);
}

#[tokio::test]
async fn update_screenshot_confirms_upload() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    if gcs_env_present() {
        return;
    }
    let fx = app.company().await;
    let s = seed(&app, &fx).await;
    let shot = new_shot(&app, fx.staff.id, 1, false).await;
    let expected = format!(
        "companies/{}/projects/{}/users/{}/screenshots/{shot}.jpg",
        s.company_uuid, s.project_uuid, fx.staff.uuid
    );
    // key bukan hasil penerbitan -> 400
    let r = app
        .patch(&format!("/screenshots/{shot}"))
        .token(&fx.staff.token)
        .json(&json!({"fileFullpath": "companies/x/y.jpg"}))
        .send()
        .await;
    r.assert_status(400);
    // bukan pemilik -> 404
    app.patch(&format!("/screenshots/{shot}"))
        .token(&fx.staff2.token)
        .json(&json!({"fileFullpath": expected}))
        .send()
        .await
        .assert_status(404);
    // sukses (isBlur & takenAt dikirim -> COALESCE)
    let r = app
        .patch(&format!("/screenshots/{shot}"))
        .token(&fx.staff.token)
        .json(&json!({"fileFullpath": expected, "isBlur": true, "takenAt": "2026-03-10T09:40:00.000Z"}))
        .send()
        .await;
    r.assert_ok();
    assert_eq!(r.json["message"], json!("update screenshot success"));
    assert_eq!(r.data()["status"], json!(3));
    assert_eq!(r.data()["isBlur"], json!(true));
    assert_eq!(r.data()["fileFullpath"], json!(expected));
    // kedua kali -> sudah selesai
    let r = app
        .patch(&format!("/screenshots/{shot}"))
        .token(&fx.staff.token)
        .json(&json!({"fileFullpath": expected}))
        .send()
        .await;
    r.assert_ok();
    assert_eq!(r.json["message"], json!("Screenshot already finished"));
    // tidak ada
    app.patch("/screenshots/00000000-0000-4000-8000-000000000002")
        .token(&fx.staff.token)
        .json(&json!({"fileFullpath": expected}))
        .send()
        .await
        .assert_status(404);
    // isBlur null -> success:false (parity Prisma)
    let shot2 = new_shot(&app, fx.staff.id, 1, false).await;
    let expected2 = format!(
        "companies/{}/projects/{}/users/{}/screenshots/{shot2}.jpg",
        s.company_uuid, s.project_uuid, fx.staff.uuid
    );
    let r = app
        .patch(&format!("/screenshots/{shot2}"))
        .token(&fx.staff.token)
        .json(&json!({"fileFullpath": expected2, "isBlur": null}))
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json["success"], json!(false));
}

#[tokio::test]
async fn delete_screenshot_rules() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    seed(&app, &fx).await;
    let shot = new_shot(&app, fx.staff.id, 3, false).await;
    // staff lain -> 404, baris tidak berubah
    app.delete(&format!("/screenshots/{shot}"))
        .token(&fx.staff2.token)
        .send()
        .await
        .assert_status(404);
    let st: i32 = app
        .scalar(&format!(
            "SELECT status FROM screenshot WHERE uuid = '{shot}'"
        ))
        .await;
    assert_eq!(st, 3);
    // pemilik
    let r = app
        .delete(&format!("/screenshots/{shot}"))
        .token(&fx.staff.token)
        .send()
        .await;
    r.assert_ok();
    assert_eq!(r.data()["isDeleted"], json!(true));
    let st: i32 = app
        .scalar(&format!(
            "SELECT status FROM screenshot WHERE uuid = '{shot}'"
        ))
        .await;
    assert_eq!(st, 0);
    app.delete(&format!("/screenshots/{shot}"))
        .token(&fx.staff.token)
        .send()
        .await
        .assert_status(404);
    // role company-wide boleh menghapus milik orang lain
    let shot2 = new_shot(&app, fx.staff.id, 3, false).await;
    app.delete(&format!("/screenshots/{shot2}"))
        .token(&fx.admin.token)
        .send()
        .await
        .assert_ok();
    app.delete("/screenshots/xyz")
        .token(&fx.admin.token)
        .send()
        .await
        .assert_status(400);
}

#[tokio::test]
async fn blur_screenshot_db_and_failure_path() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    if gcs_env_present() {
        return;
    }
    let fx = app.company().await;
    seed(&app, &fx).await;
    let shot = shot_of(&app, fx.staff.id).await;
    // pemilik: SELECT & otorisasi lolos, unduhan GCS gagal
    let r = app
        .put(&format!("/screenshots/blur/{shot}"))
        .token(&fx.staff.token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json["success"], json!(false));
    assert_eq!(r.json["message"], json!("Failed to blur image"));
    // bukan pemilik/role non-company-wide
    let r = app
        .put(&format!("/screenshots/blur/{shot}"))
        .token(&fx.staff2.token)
        .send()
        .await;
    assert_eq!(r.json["message"], json!("Screenshot not found"));
    // status bukan FINISHED
    let up = new_shot(&app, fx.staff.id, 1, false).await;
    let r = app
        .put(&format!("/screenshots/blur/{up}"))
        .token(&fx.staff.token)
        .send()
        .await;
    assert_eq!(r.json["message"], json!("Screenshot not found"));
    app.put("/screenshots/blur/bad")
        .token(&fx.staff.token)
        .send()
        .await
        .assert_status(400);
}

#[tokio::test]
async fn open_url_rules() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    seed(&app, &fx).await;
    let plain = shot_of(&app, fx.staff.id).await;
    let blurred = new_shot(&app, fx.staff.id, 3, true).await;

    let r = app
        .get(&format!("/screenshots/{plain}/open-url"))
        .token(&fx.staff.token)
        .send()
        .await;
    r.assert_ok();
    assert_eq!(r.json["message"], json!("Screenshot is not blurred"));
    assert!(
        r.data()["openUrl"]
            .as_str()
            .unwrap()
            .contains("variant=original")
    );
    // staff lain
    let r = app
        .get(&format!("/screenshots/{plain}/open-url"))
        .token(&fx.staff2.token)
        .send()
        .await;
    assert_eq!(r.json["success"], json!(false));
    assert_eq!(
        r.json["message"],
        json!("Access denied: screenshot does not belong to you")
    );
    // blur tetapi company belum mengaktifkan blur-role
    let r = app
        .get(&format!("/screenshots/{blurred}/open-url"))
        .token(&fx.staff.token)
        .send()
        .await;
    assert_eq!(r.json["success"], json!(false));
    assert!(
        r.json["message"]
            .as_str()
            .unwrap()
            .contains("role is not allowed")
    );
    // aktifkan blur untuk STAFF
    app.exec(&format!(
        r#"UPDATE company SET "imageBlurEnabled" = true, "imageBlurRoles" = ARRAY['STAFF'] WHERE id = {}"#,
        fx.id
    ))
    .await;
    let r = app
        .get(&format!("/screenshots/{blurred}/open-url"))
        .token(&fx.staff.token)
        .send()
        .await;
    r.assert_ok();
    assert_eq!(r.json["message"], json!("Success get open image url"));
    // tidak ada
    let r = app
        .get("/screenshots/00000000-0000-4000-8000-000000000003/open-url")
        .token(&fx.staff.token)
        .send()
        .await;
    assert_eq!(r.json["message"], json!("Screenshot not found"));
    app.exec(&format!(
        r#"UPDATE company SET "imageBlurEnabled" = false, "imageBlurRoles" = NULL WHERE id = {}"#,
        fx.id
    ))
    .await;
}

#[tokio::test]
async fn image_and_image_url_db_and_failure_path() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    if gcs_env_present() {
        return;
    }
    let fx = app.company().await;
    seed(&app, &fx).await;
    let plain = shot_of(&app, fx.staff.id).await;
    let blurred = new_shot(&app, fx.staff.id, 3, true).await;
    let uploading = new_shot(&app, fx.staff.id, 1, false).await;

    for suffix in [
        "image",
        "image?variant=display",
        "image?variant=original",
        "image/url",
        "image/url?variant=original",
    ] {
        // pemilik & role company-wide: lolos DB/otorisasi, GCS gagal
        for u in [&fx.staff, &fx.admin, &fx.manager] {
            let r = app
                .get(&format!("/screenshots/{plain}/{suffix}"))
                .token(&u.token)
                .send()
                .await;
            assert_gcs_failure(&r);
            assert_ne!(
                r.status.as_u16(),
                404,
                "{suffix}: pemilik harus lolos otorisasi"
            );
            assert_ne!(r.status.as_u16(), 403, "{suffix}");
        }
        // staff lain: 404
        app.get(&format!("/screenshots/{plain}/{suffix}"))
            .token(&fx.staff2.token)
            .send()
            .await
            .assert_status(404);
        // belum FINISHED: 404
        app.get(&format!("/screenshots/{uploading}/{suffix}"))
            .token(&fx.staff.token)
            .send()
            .await
            .assert_status(404);
    }
    // blur + original, role tidak diizinkan -> 403; display tetap lolos otorisasi
    for p in ["image?variant=original", "image/url?variant=original"] {
        app.get(&format!("/screenshots/{blurred}/{p}"))
            .token(&fx.staff.token)
            .send()
            .await
            .assert_status(403);
    }
    let r = app
        .get(&format!("/screenshots/{blurred}/image"))
        .token(&fx.staff.token)
        .send()
        .await;
    assert_gcs_failure(&r);
    // validasi query & uuid
    app.get(&format!("/screenshots/{plain}/image?variant=bogus"))
        .token(&fx.staff.token)
        .send()
        .await
        .assert_status(400);
    app.get("/screenshots/nope/image")
        .token(&fx.staff.token)
        .send()
        .await
        .assert_status(400);
    app.get(&format!("/screenshots/{plain}/image"))
        .send()
        .await
        .assert_status(401);
}

// ---------------------------------------------------------------- activity logs

#[tokio::test]
async fn activity_logs_find_and_filters() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let s = seed(&app, &fx).await;
    let tl = timeline_of(&app, fx.staff.id).await;

    let r = app
        .get(&format!("/activity-logs?takenDate={D1}"))
        .token(&fx.staff.token)
        .send()
        .await;
    r.assert_ok();
    assert_eq!(r.json["takenDate"], json!(D1));
    let data = r.json["data"].as_array().unwrap();
    assert!(
        data.iter()
            .any(|e| e["type"] == "app" && e["name"] == "ItSeedApp"),
        "{data:?}"
    );
    assert!(
        data.iter()
            .any(|e| e["type"] == "url" && e["host"] == "itseed.example.com")
    );
    assert_eq!(r.json["total"], json!(data.len()));
    let sum = &r.json["summary"];
    assert!(sum["totalTrackedSeconds"].as_i64().unwrap() > 0);
    assert!(sum["topApps"].is_array() && sum["topHosts"].is_array());
    for e in data {
        assert!(e["durationSeconds"].is_i64() || e["durationSeconds"].is_u64());
        assert!(e["activityPercent"].is_number());
        assert!(e["startAt"].is_string());
    }

    // filter: search, project, timeline
    let q = format!(
        "/activity-logs?takenDate={D1}&search=ItSeedApp&project_uuid={}&tracking_timeline_uuid={tl}",
        s.project_uuid
    );
    let r = app.get(&q).token(&fx.staff.token).send().await;
    r.assert_ok();
    assert!(!r.json["data"].as_array().unwrap().is_empty());
    // search dengan metakarakter LIKE
    let r = app
        .get(&format!("/activity-logs?takenDate={D1}&search=%25_%5C"))
        .token(&fx.staff.token)
        .send()
        .await;
    r.assert_ok();
    assert_eq!(r.json["total"], json!(0));
    // tanggal tanpa data
    let r = app
        .get("/activity-logs?takenDate=2020-01-01")
        .token(&fx.staff.token)
        .send()
        .await;
    r.assert_ok();
    assert_eq!(r.json["total"], json!(0));
    // timeline tak dikenal
    app.get(&format!(
        "/activity-logs?takenDate={D1}&tracking_timeline_uuid=00000000-0000-4000-8000-000000000009"
    ))
    .token(&fx.staff.token)
    .send()
    .await
    .assert_status(404);
    // validasi
    app.get("/activity-logs")
        .token(&fx.staff.token)
        .send()
        .await
        .assert_status(400);
    app.get(&format!("/activity-logs?takenDate={D1}"))
        .send()
        .await
        .assert_status(401);
}

#[tokio::test]
async fn activity_logs_user_filter_scope() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    seed(&app, &fx).await;
    // company-wide role boleh memfilter user lain
    for u in [&fx.admin, &fx.manager, &fx.super_admin] {
        let r = app
            .get(&format!(
                "/activity-logs?takenDate={D1}&user_uuid={}",
                fx.staff.uuid
            ))
            .token(&u.token)
            .send()
            .await;
        r.assert_ok();
        assert!(r.json["total"].as_i64().unwrap() > 0);
    }
    // staff tanpa divisi: filter user (termasuk dirinya) -> 403
    for target in [&fx.staff2.uuid, &fx.staff.uuid] {
        let r = app
            .get(&format!("/activity-logs?takenDate={D1}&user_uuid={target}"))
            .token(&fx.staff.token)
            .send()
            .await;
        r.assert_status(403);
    }
    // staff tanpa filter: hanya data sendiri
    let r = app
        .get(&format!("/activity-logs?takenDate={D1}"))
        .token(&fx.staff2.token)
        .send()
        .await;
    r.assert_ok();
}

#[tokio::test]
async fn activity_logs_export_csv() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    seed(&app, &fx).await;
    let r = app
        .get(&format!("/activity-logs/export/csv?takenDate={D1}"))
        .token(&fx.staff.token)
        .send()
        .await;
    r.assert_status(200);
    let ct = r
        .headers
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert!(ct.starts_with("text/csv"), "{ct}");
    let cd = r
        .headers
        .get("content-disposition")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert!(cd.contains(&format!("activity-logs-{D1}.csv")), "{cd}");
    assert!(!r.bytes.is_empty());
    assert!(r.text().contains("ItSeedApp"), "{}", r.text());
    // tanpa data tetap CSV valid (header saja)
    let r = app
        .get("/activity-logs/export/csv?takenDate=2020-01-01")
        .token(&fx.staff.token)
        .send()
        .await;
    r.assert_status(200);
    assert!(!r.bytes.is_empty());
    app.get("/activity-logs/export/csv")
        .token(&fx.staff.token)
        .send()
        .await
        .assert_status(400);
}

#[tokio::test]
async fn activity_logs_post_upsert() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    seed(&app, &fx).await;
    // timeline B hari D2 milik staff2 (tidak dipakai GET lain)
    let tl: String = app
        .scalar(&format!(
            r#"SELECT tt.uuid::text FROM "trackingTimeline" tt JOIN tracking tr ON tr.id = tt."trackingId"
               WHERE tr."userId" = {} AND tt.note = 'itseed B' AND tr."takenDate" = '2026-03-11'"#,
            fx.staff2.id
        ))
        .await;
    let title = format!("ItPost {}", uuid_like());
    let base = |start: i64, end: i64, url: Option<&str>| {
        let mut b = json!({
            "process_name": "ItPostApp", "window_title": title,
            "startAt": start, "endAt": end, "timezone": "UTC", "tracking_timeline_uuid": tl,
        });
        if let Some(u) = url {
            b["full_url"] = json!(u);
        }
        b
    };
    // validasi
    app.post("/activity-logs")
        .token(&fx.staff2.token)
        .json(&json!({}))
        .send()
        .await
        .assert_status(400);
    app.post("/activity-logs")
        .json(&base(1, 2, None))
        .send()
        .await
        .assert_status(401);
    let mut no_tl = base(1_773_234_300, 1_773_234_360, None);
    no_tl["tracking_timeline_uuid"] = json!("  ");
    app.post("/activity-logs")
        .token(&fx.staff2.token)
        .json(&no_tl)
        .send()
        .await
        .assert_status(400);
    let mut bad_tl = base(1_773_234_300, 1_773_234_360, None);
    bad_tl["tracking_timeline_uuid"] = json!("00000000-0000-4000-8000-00000000000a");
    app.post("/activity-logs")
        .token(&fx.staff2.token)
        .json(&bad_tl)
        .send()
        .await
        .assert_status(404);

    // company belum mengaktifkan activity tracking
    app.exec(&format!(
        r#"UPDATE company SET "useActivityTrack" = false WHERE id = {}"#,
        fx.id
    ))
    .await;
    let r = app
        .post("/activity-logs")
        .token(&fx.staff2.token)
        .json(&base(1_773_234_300, 1_773_234_360, None))
        .send()
        .await;
    r.assert_ok();
    assert!(r.json["message"].as_str().unwrap().contains("disabled"));
    assert!(r.json["data"].is_null());

    app.exec(&format!(
        r#"UPDATE company SET "useActivityTrack" = true WHERE id = {}"#,
        fx.id
    ))
    .await;
    // app baru
    let r = app
        .post("/activity-logs")
        .token(&fx.staff2.token)
        .json(&base(1_773_234_300, 1_773_234_360, None))
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(
        r.json["message"],
        json!("activity log created successfully"),
        "{}",
        r.json
    );
    assert_eq!(r.data()["name"], json!("ItPostApp"));
    let id1 = r.data()["id"].as_i64().unwrap();
    // sesi sama (startAt identik) -> diperbarui, endAt diperpanjang
    let r = app
        .post("/activity-logs")
        .token(&fx.staff2.token)
        .json(&base(1_773_234_300, 1_773_234_390, None))
        .send()
        .await;
    assert_eq!(
        r.json["message"],
        json!("activity log updated successfully"),
        "{}",
        r.json
    );
    assert_eq!(r.data()["id"].as_i64().unwrap(), id1);
    // sesi berurutan (mulai tepat saat sesi sebelumnya berakhir) -> digabung
    let r = app
        .post("/activity-logs")
        .token(&fx.staff2.token)
        .json(&base(1_773_234_390, 1_773_234_420, None))
        .send()
        .await;
    r.assert_ok();
    assert_eq!(r.data()["id"].as_i64().unwrap(), id1, "{}", r.json);
    // dengan full_url -> upsert url
    let r = app
        .post("/activity-logs")
        .token(&fx.staff2.token)
        .json(&base(
            1_773_234_300,
            1_773_234_360,
            Some("https://it-post.example.org/a/b?c=1"),
        ))
        .send()
        .await;
    r.assert_ok();
    assert_eq!(
        r.json["message"],
        json!("activity log created successfully"),
        "{}",
        r.json
    );
    assert_eq!(r.data()["host"], json!("it-post.example.org"));
    let r = app
        .post("/activity-logs")
        .token(&fx.staff2.token)
        .json(&base(
            1_773_234_300,
            1_773_234_400,
            Some("https://it-post.example.org/a/b?c=1"),
        ))
        .send()
        .await;
    assert_eq!(
        r.json["message"],
        json!("activity log updated successfully"),
        "{}",
        r.json
    );
    // judul kosong -> fallback ke process_name / host
    let mut empty = base(
        1_773_238_000,
        1_773_238_060,
        Some("https://empty-title.example.org/"),
    );
    empty["window_title"] = json!("");
    let r = app
        .post("/activity-logs")
        .token(&fx.staff2.token)
        .json(&empty)
        .send()
        .await;
    r.assert_ok();
    assert_eq!(r.data()["title"], json!("empty-title.example.org"));
    let n: i64 = app
        .scalar("SELECT COUNT(*) FROM app WHERE name = 'ItPostApp'")
        .await;
    assert!(n >= 2);
    let _: Value = r.json;
}

fn uuid_like() -> String {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("{n:x}")
}
