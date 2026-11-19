//! Uji integrasi 8 route `/trackings` (mz-trackings) terhadap Postgres nyata.
//! Setiap test memakai tanggal `takenDate` sendiri supaya baris `tracking`
//! (unik per user+tanggal) tidak bertabrakan antar test yang berjalan paralel.
//! Fixture user/company tidak pernah dinonaktifkan atau dihapus.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use mz_testkit::{Company, TestApp, User};
use serde_json::{Value, json};

/// Seed satu project (langsung SQL) dan kembalikan id-nya.
async fn seed_project(app: &TestApp, _fx: &Company) -> i32 {
    app.scalar(
        r#"INSERT INTO project (code, name, "screenshotPerHour", "projectTypeId", "startDate", status)
           VALUES ('T' || substr(md5(random()::text), 1, 5), 'Tracking seed', 3,
                   (SELECT id FROM "projectType" WHERE code = 'FIXED_HOURS' ORDER BY id LIMIT 1),
                   '2035-01-01', 1)
           RETURNING id"#,
    )
    .await
}

/// Seed `tracking` untuk user di tanggal `date`; kembalikan (id, uuid).
async fn seed_tracking(app: &TestApp, user_id: i32, date: &str) -> (i32, String) {
    sqlx::query_as::<_, (i32, String)>(
        r#"INSERT INTO tracking ("isAbsent", "takenDate", "userId", status)
           VALUES (false, $1::date, $2, 1) RETURNING id, uuid::text"#,
    )
    .bind(date)
    .bind(user_id)
    .fetch_one(app.db())
    .await
    .unwrap()
}

/// Seed timeline `(start, end)` (timestamp literal) untuk tracking `tid`.
async fn seed_timeline(app: &TestApp, tid: i32, project_id: i32, start: &str, end: &str) {
    sqlx::query(
        r#"INSERT INTO "trackingTimeline" (note, "isOffline", "startAt", "endAt", "trackingId", "projectId", status, timezone)
           VALUES ('seed', false, $1::timestamp, $2::timestamp, $3, $4, 1, 'Asia/Jakarta')"#,
    )
    .bind(start)
    .bind(end)
    .bind(tid)
    .bind(project_id)
    .execute(app.db())
    .await
    .unwrap();
}

/// Tracking satu hari dengan dua timeline (2 jam + 30 menit = 9000 dtk).
async fn seed_day(app: &TestApp, fx: &Company, user: &User, date: &str) -> (i32, String, i32) {
    let project = seed_project(app, fx).await;
    let (tid, uuid) = seed_tracking(app, user.id, date).await;
    seed_timeline(
        app,
        tid,
        project,
        &format!("{date} 09:00:00"),
        &format!("{date} 11:00:00"),
    )
    .await;
    seed_timeline(
        app,
        tid,
        project,
        &format!("{date} 13:00:00"),
        &format!("{date} 13:30:00"),
    )
    .await;
    (tid, uuid, project)
}

// ───────────────────────── GET /trackings ─────────────────────────

#[tokio::test]
async fn find_all_own_day_returns_timelines_and_total() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    seed_day(&app, &fx, &fx.staff, "2035-01-10").await;

    let res = app
        .get("/trackings?takenDate=2035-01-10")
        .token(&fx.staff.token)
        .send()
        .await;
    res.assert_ok();
    res.assert_no_sql_error();
    let d = res.data();
    assert_eq!(d["total"], 1);
    let row = &d["result"][0];
    assert_eq!(row["userId"], fx.staff.id);
    assert_eq!(row["totalTakenInSecond"], 9000);
    assert_eq!(row["user"]["username"], fx.staff.username.as_str());
    // Non-privileged: tidak ada userCompany.
    assert!(row["user"].get("userCompany").is_none());
    let tls = row["trackingTimeline"].as_array().unwrap();
    assert_eq!(tls.len(), 2);
    assert!(tls[0]["project"]["code"].is_string());
    assert_eq!(tls[0]["status"], 1);
    assert!(tls[0]["integration"].is_null());
    assert!(tls[0]["screenshot"].is_array());
    assert_eq!(d["workLimit"], Value::Null);
}

#[tokio::test]
async fn find_all_empty_day_and_sort_params() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    // Hari tanpa data → result kosong, tetap sukses.
    let res = app
        .get("/trackings?takenDate=1999-12-31")
        .token(&fx.staff.token)
        .send()
        .await;
    res.assert_ok();
    res.assert_no_sql_error();
    assert_eq!(res.data()["total"], 0);
    assert_eq!(res.data()["result"], json!([]));

    // Urutan valid diterima.
    let res = app
        .get("/trackings?takenDate=1999-12-31&orderBy=asc&orderColumn=takenDate")
        .token(&fx.staff.token)
        .send()
        .await;
    res.assert_ok();
    res.assert_no_sql_error();

    // Kolom di luar whitelist → Prisma menolak → `success:false` + pesan error.
    let res = app
        .get("/trackings?takenDate=1999-12-31&orderColumn=password")
        .token(&fx.staff.token)
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(res.json["success"], false);
    assert_eq!(res.json["message"], "cannot get findAll tracking");

    // Tanggal tidak valid → success:false.
    let res = app
        .get("/trackings?takenDate=bukan-tanggal")
        .token(&fx.staff.token)
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(res.json["success"], false);
}

#[tokio::test]
async fn find_all_bad_query_numbers_are_400() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    app.get("/trackings?pageNumber=abc")
        .token(&fx.staff.token)
        .send()
        .await
        .assert_status(400);
    app.get("/trackings?itemsPerPage=1&itemsPerPage=2")
        .token(&fx.staff.token)
        .send()
        .await
        .assert_status(400);
}

#[tokio::test]
async fn find_all_privileged_filters_by_user_uuid_with_user_company() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    seed_day(&app, &fx, &fx.staff, "2035-01-11").await;

    let res = app
        .get(&format!(
            "/trackings?takenDate=2035-01-11&userUuid={}",
            fx.staff.uuid
        ))
        .token(&fx.super_admin.token)
        .send()
        .await;
    res.assert_ok();
    res.assert_no_sql_error();
    let d = res.data();
    assert_eq!(d["total"], 1);
    let row = &d["result"][0];
    assert_eq!(row["userId"], fx.staff.id);
    assert_eq!(row["totalTakenInSecond"], 9000);
    // Privileged: ikut memuat userCompany untuk company aktif.
    let uc = row["user"]["userCompany"].as_array().unwrap();
    assert!(!uc.is_empty());
    assert_eq!(uc[0]["companyId"], fx.id);
}

#[tokio::test]
async fn find_all_staff_cannot_read_other_user_rows() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    seed_day(&app, &fx, &fx.staff2, "2035-01-12").await;

    // Staff meminta data staff2: tidak diizinkan melihat, hasil tetap milik dirinya
    // (tanpa baris staff2).
    let res = app
        .get(&format!(
            "/trackings?takenDate=2035-01-12&userUuid={}",
            fx.staff2.uuid
        ))
        .token(&fx.staff.token)
        .send()
        .await;
    res.assert_ok();
    res.assert_no_sql_error();
    let rows = res.data()["result"].as_array().unwrap().clone();
    assert!(rows.iter().all(|r| r["userId"] != fx.staff2.id));
}

#[tokio::test]
async fn find_all_work_limit_and_device_poll() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    seed_day(&app, &fx, &fx.staff, "2035-01-13").await;
    app.exec(&format!(
        r#"INSERT INTO "userWorkTimeLimit" ("userId", "companyId", "dailyLimitSec", "weeklyLimitSec", status)
           VALUES ({}, {}, 28800, 144000, 1)"#,
        fx.staff.id, fx.id
    ))
    .await;

    // Web: workLimit dihitung dari total hari ini (9000 dtk) dan minggu berjalan.
    let res = app
        .get("/trackings?takenDate=2035-01-13")
        .token(&fx.staff.token)
        .send()
        .await;
    res.assert_ok();
    res.assert_no_sql_error();
    let wl = &res.data()["workLimit"];
    assert_eq!(wl["dailyWorkedSec"], 9000);
    assert!(wl["dailyLimitSec"].is_number() || wl["dailyLimitSec"].is_null());

    // Desktop (sid + tanpa Origin): skip user/screenshot/workLimit.
    let sid: String = app.scalar("SELECT gen_random_uuid()::text").await;
    app.exec(&format!(
        r#"INSERT INTO "userConnected" ("userId", status, os, "macAddress", sid)
           VALUES ({}, 1, 'test', 'mac-trk-{}', '{sid}'::uuid)"#,
        fx.staff.id,
        &sid[..8]
    ))
    .await;
    let token = app.token_with(json!({ "userId": fx.staff.id, "sid": sid }));
    let res = app
        .get("/trackings?takenDate=2035-01-13")
        .token(&token)
        .send()
        .await;
    res.assert_ok();
    res.assert_no_sql_error();
    let row = &res.data()["result"][0];
    assert!(row.get("user").is_none());
    assert_eq!(res.data()["workLimit"], Value::Null);
    assert_eq!(row["trackingTimeline"].as_array().unwrap().len(), 2);
}

// ───────────────────────── GET /trackings/date-time ─────────────────────────

#[tokio::test]
async fn server_date_time_returns_format() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let res = app
        .get("/trackings/date-time")
        .token(&fx.staff.token)
        .send()
        .await;
    res.assert_ok();
    let date = res.data()["date"].as_str().unwrap();
    // Node memakai format YYYY-DD-MM.
    assert_eq!(date.len(), 10);
    assert_eq!(date.matches('-').count(), 2);
    assert!(res.data()["time"].as_str().unwrap().contains(':'));
}

#[tokio::test]
async fn server_date_time_fix_moves_timelines_to_target_day() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let project = seed_project(&app, &fx).await;
    // Tracking lama (2036-05-01) memiliki timeline yang mulai di 2036-05-02.
    let (old_tid, _) = seed_tracking(&app, fx.staff2.id, "2036-05-01").await;
    seed_timeline(
        &app,
        old_tid,
        project,
        "2036-05-02 08:00:00",
        "2036-05-02 09:00:00",
    )
    .await;
    // Tracking tujuan 2036-05-02.
    let (new_tid, _) = seed_tracking(&app, fx.staff2.id, "2036-05-02").await;

    let res = app
        .get(&format!(
            "/trackings/date-time?takenDate=2036-05-02&userUuid={}",
            fx.staff2.uuid
        ))
        .token(&fx.staff2.token)
        .send()
        .await;
    res.assert_ok();
    res.assert_no_sql_error();

    let moved: i32 = app
        .scalar(&format!(
            r#"SELECT "trackingId" FROM "trackingTimeline" WHERE "trackingId" IN ({old_tid}, {new_tid})
               AND "projectId" = {project} AND "startAt" = '2036-05-02 08:00:00'"#
        ))
        .await;
    assert_eq!(moved, new_tid);

    // userUuid tak valid → galat ditelan (tetap 200 success).
    let res = app
        .get("/trackings/date-time?takenDate=2036-05-02&userUuid=bukan-uuid")
        .token(&fx.staff.token)
        .send()
        .await;
    res.assert_ok();
}

// ───────────────────────── POST /trackings ─────────────────────────

#[tokio::test]
async fn create_tracking_is_idempotent_per_day() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let body = json!({
        "takenDate": "2034-04-10",
        "isAbsent": false,
        "from": "web",
        "timezone": "Asia/Jakarta",
    });
    let res = app
        .post("/trackings")
        .token(&fx.staff.token)
        .json(&body)
        .send()
        .await;
    res.assert_ok();
    res.assert_no_sql_error();
    let id = res.data()["id"].clone();
    assert!(id.is_number());
    assert_eq!(res.data()["userId"], fx.staff.id);
    assert_eq!(res.data()["takenDate"], "2034-04-10T00:00:00.000Z");

    // Panggilan kedua untuk hari yang sama: kembali baris yang sama.
    let res = app
        .post("/trackings")
        .token(&fx.staff.token)
        .json(&body)
        .send()
        .await;
    res.assert_ok();
    assert_eq!(res.data()["id"], id);
    let count: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM tracking WHERE "userId" = {} AND "takenDate" = '2034-04-10'"#,
            fx.staff.id
        ))
        .await;
    assert_eq!(count, 1);
}

#[tokio::test]
async fn create_tracking_reactivates_soft_deleted_row() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let body = json!({ "takenDate": "2034-04-12", "isAbsent": true, "from": "web" });
    let first = app
        .post("/trackings")
        .token(&fx.staff.token)
        .json(&body)
        .send()
        .await;
    first.assert_ok();
    let id = first.data()["id"].clone();
    assert_eq!(first.data()["isAbsent"], true);

    // Soft delete lalu create lagi: baris dihidupkan kembali (id sama).
    app.delete(&format!(
        "/trackings/{}",
        first.data()["uuid"].as_str().unwrap()
    ))
    .token(&fx.staff.token)
    .send()
    .await
    .assert_ok();
    let again = app
        .post("/trackings")
        .token(&fx.staff.token)
        .json(&json!({ "takenDate": "2034-04-12", "isAbsent": false, "from": "web" }))
        .send()
        .await;
    again.assert_ok();
    assert_eq!(again.data()["id"], id);
    assert_eq!(again.data()["isAbsent"], false);
}

#[tokio::test]
async fn create_tracking_validation_and_user_scope() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    // takenDate & isAbsent wajib.
    app.post("/trackings")
        .token(&fx.staff.token)
        .json(&json!({}))
        .send()
        .await
        .assert_status(400);
    app.post("/trackings")
        .token(&fx.staff.token)
        .json(&json!({ "takenDate": "2034-04-13", "isAbsent": "no" }))
        .send()
        .await
        .assert_status(400);

    // userUuid tidak dikenal → success false "user not found".
    let res = app
        .post("/trackings")
        .token(&fx.super_admin.token)
        .json(&json!({
            "takenDate": "2034-04-14",
            "isAbsent": false,
            "userUuid": "00000000-0000-4000-8000-0000000000aa",
        }))
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(res.json["message"], "user not found");

    // Staff tidak boleh membuat tracking untuk staff2.
    let res = app
        .post("/trackings")
        .token(&fx.staff.token)
        .json(&json!({
            "takenDate": "2034-04-14",
            "isAbsent": false,
            "userUuid": fx.staff2.uuid,
        }))
        .send()
        .await;
    assert_eq!(res.json["success"], false);
    assert_eq!(res.json["message"], "user not found");
}

// ───────────────────────── GET /trackings/:uuid ─────────────────────────

#[tokio::test]
async fn find_one_returns_detail_and_respects_scope() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let (_, uuid, _) = seed_day(&app, &fx, &fx.staff, "2035-02-01").await;

    let res = app
        .get(&format!("/trackings/{uuid}"))
        .token(&fx.staff.token)
        .send()
        .await;
    res.assert_ok();
    res.assert_no_sql_error();
    let d = res.data();
    assert_eq!(d["uuid"], uuid.as_str());
    assert_eq!(d["totalTakenInSecond"], Value::Null);
    assert_eq!(d["user"]["id"], fx.staff.id);
    assert_eq!(d["trackingTimeline"].as_array().unwrap().len(), 2);

    // Admin boleh melihat data staff.
    let res = app
        .get(&format!("/trackings/{uuid}"))
        .token(&fx.admin.token)
        .send()
        .await;
    res.assert_ok();
    assert_eq!(res.data()["uuid"], uuid.as_str());

    // Staff2 (bukan pemilik, bukan leader) → success false.
    let res = app
        .get(&format!("/trackings/{uuid}"))
        .token(&fx.staff2.token)
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(res.json["success"], false);
    assert!(res.json["data"].is_null());
}

#[tokio::test]
async fn find_one_not_found_and_bad_uuid() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let missing = "00000000-0000-4000-8000-0000000000bb";
    let res = app
        .get(&format!("/trackings/{missing}"))
        .token(&fx.staff.token)
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(res.json["success"], false);
    assert_eq!(
        res.json["message"],
        format!("Data tracking by uuid {missing} not found")
    );

    app.get("/trackings/xyz")
        .token(&fx.staff.token)
        .send()
        .await
        .assert_status(400);
}

// ───────────────────────── PATCH /trackings/:uuid ─────────────────────────

#[tokio::test]
async fn update_tracking_changes_date_and_absence() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let (_, uuid, _) = seed_day(&app, &fx, &fx.staff, "2035-02-02").await;

    let res = app
        .patch(&format!("/trackings/{uuid}"))
        .token(&fx.staff.token)
        .json(&json!({ "takenDate": "2035-02-20", "isAbsent": true }))
        .send()
        .await;
    res.assert_ok();
    res.assert_no_sql_error();
    assert_eq!(res.data()["isAbsent"], true);
    assert_eq!(res.data()["takenDate"], "2035-02-20T00:00:00.000Z");

    // isAbsent null diperbolehkan (kolom nullable). takenDate yang tidak dikirim
    // memakai "sekarang" seperti `moment.utc(undefined)` di Node.
    let res = app
        .patch(&format!("/trackings/{uuid}"))
        .token(&fx.staff.token)
        .json(&json!({ "isAbsent": null }))
        .send()
        .await;
    res.assert_ok();
    res.assert_no_sql_error();
    assert_eq!(res.data()["isAbsent"], Value::Null);
    let today: String = app
        .scalar(
            "SELECT to_char((now() AT TIME ZONE 'UTC')::date, 'YYYY-MM-DD') || 'T00:00:00.000Z'",
        )
        .await;
    assert_eq!(res.data()["takenDate"], today.as_str());
}

#[tokio::test]
async fn update_tracking_failures_return_cannot_update() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let (_, uuid_a, _) = seed_day(&app, &fx, &fx.staff, "2035-02-03").await;
    let (_, _uuid_b, _) = seed_day(&app, &fx, &fx.staff, "2035-02-04").await;

    // Pindah ke tanggal yang sudah dipakai (unik userId+takenDate) → gagal.
    let res = app
        .patch(&format!("/trackings/{uuid_a}"))
        .token(&fx.staff.token)
        .json(&json!({ "takenDate": "2035-02-04" }))
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(res.json["success"], false);
    assert_eq!(res.json["message"], "cannot update tracking");

    // Staff2 tidak boleh mengubah milik staff.
    let res = app
        .patch(&format!("/trackings/{uuid_a}"))
        .token(&fx.staff2.token)
        .json(&json!({ "isAbsent": true }))
        .send()
        .await;
    assert_eq!(res.json["success"], false);

    // Uuid tidak ada.
    let res = app
        .patch("/trackings/00000000-0000-4000-8000-0000000000cc")
        .token(&fx.staff.token)
        .json(&json!({ "isAbsent": true }))
        .send()
        .await;
    assert_eq!(res.json["success"], false);

    // Tanggal tidak valid → DTO 400.
    app.patch(&format!("/trackings/{uuid_a}"))
        .token(&fx.staff.token)
        .json(&json!({ "takenDate": "nope" }))
        .send()
        .await
        .assert_status(400);
}

// ───────────────────────── DELETE /trackings/:uuid ─────────────────────────

#[tokio::test]
async fn delete_tracking_soft_deletes() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let (tid, uuid, _) = seed_day(&app, &fx, &fx.staff, "2035-02-05").await;

    let res = app
        .delete(&format!("/trackings/{uuid}"))
        .token(&fx.staff.token)
        .send()
        .await;
    res.assert_ok();
    res.assert_no_sql_error();
    assert_eq!(res.data()["isDeleted"], true);
    let status: i32 = app
        .scalar(&format!("SELECT status FROM tracking WHERE id = {tid}"))
        .await;
    assert_eq!(status, 0);

    // Node `remove` tidak memfilter status: hapus ulang tetap sukses (parity).
    let res = app
        .delete(&format!("/trackings/{uuid}"))
        .token(&fx.staff.token)
        .send()
        .await;
    res.assert_ok();
    // Namun detail sudah tidak ditemukan (findOne memfilter status aktif).
    let res = app
        .get(&format!("/trackings/{uuid}"))
        .token(&fx.staff.token)
        .send()
        .await;
    assert_eq!(res.json["success"], false);
}

#[tokio::test]
async fn delete_tracking_scope_and_bad_uuid() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let (_, uuid, _) = seed_day(&app, &fx, &fx.staff2, "2035-02-06").await;

    // Staff (bukan pemilik) → not found body, data tetap ada.
    let res = app
        .delete(&format!("/trackings/{uuid}"))
        .token(&fx.staff.token)
        .send()
        .await;
    assert_eq!(res.json["success"], false);
    let still: i64 = app
        .scalar(&format!(
            "SELECT COUNT(*) FROM tracking WHERE uuid = '{uuid}'::uuid AND status = 1"
        ))
        .await;
    assert_eq!(still, 1);

    app.delete("/trackings/bad")
        .token(&fx.staff.token)
        .send()
        .await
        .assert_status(400);
}

// ───────────────────────── GET /trackings/count/:uuid ─────────────────────────

#[tokio::test]
async fn count_tracking_month_and_week_totals() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    // Maret 2035 (month=2, 0-based seperti moment): 05 = 9000 dtk, 06 = 1800 dtk.
    seed_day(&app, &fx, &fx.staff, "2035-03-05").await;
    let project = seed_project(&app, &fx).await;
    let (tid, _) = seed_tracking(&app, fx.staff.id, "2035-03-06").await;
    seed_timeline(
        &app,
        tid,
        project,
        "2035-03-06 10:00:00",
        "2035-03-06 10:30:00",
    )
    .await;

    let res = app
        .get(&format!(
            "/trackings/count/{}?month=2&year=2035&date=2035-03-05",
            fx.staff.uuid
        ))
        .token(&fx.staff.token)
        .send()
        .await;
    res.assert_ok();
    res.assert_no_sql_error();
    let d = res.data();
    assert_eq!(d["totalTakenSecondOfMonth"], 10800);
    let days = d["monthCountDate"].as_array().unwrap();
    assert_eq!(days.len(), 31);
    let d5 = days
        .iter()
        .find(|x| x["takenDate"] == "2035-03-05")
        .unwrap();
    assert_eq!(d5["totalTakenInSecond"], 9000);
    let empty = days
        .iter()
        .find(|x| x["takenDate"] == "2035-03-01")
        .unwrap();
    assert_eq!(empty["totalTakenInSecond"], Value::Null);
    assert_eq!(empty["percentage"], 0);
    assert!(d["totalTakenSecondOfWeek"].as_i64().unwrap() >= 9000);
    assert!(!d["hourCount"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn count_tracking_scope_invalid_month_and_unknown_user() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    // Admin melihat staff: boleh.
    let res = app
        .get(&format!(
            "/trackings/count/{}?month=2&year=2035",
            fx.staff.uuid
        ))
        .token(&fx.admin.token)
        .send()
        .await;
    res.assert_ok();
    res.assert_no_sql_error();

    // Staff2 melihat staff → ditolak (403).
    app.get(&format!(
        "/trackings/count/{}?month=2&year=2035",
        fx.staff.uuid
    ))
    .token(&fx.staff2.token)
    .send()
    .await
    .assert_status(403);

    // Bulan tidak valid → success false.
    let res = app
        .get(&format!("/trackings/count/{}?month=abc", fx.staff.uuid))
        .token(&fx.staff.token)
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(res.json["success"], false);

    // User tidak dikenal: tidak ada data, total nol.
    let res = app
        .get("/trackings/count/00000000-0000-4000-8000-0000000000dd?month=2&year=2035")
        .token(&fx.staff.token)
        .send()
        .await;
    res.assert_ok();
    assert_eq!(res.data()["totalTakenSecondOfMonth"], 0);

    app.get("/trackings/count/nope")
        .token(&fx.staff.token)
        .send()
        .await
        .assert_status(400);
}

// ───────────────────────── POST /trackings/sync-local-db ─────────────────────────

#[tokio::test]
async fn sync_local_db_stores_payload() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let payload = "[{\"uuid\":\"local-1\"}]";
    let res = app
        .post("/trackings/sync-local-db?takenDate=2035-04-01")
        .token(&fx.staff.token)
        .json(&json!({ "data": payload }))
        .send()
        .await;
    res.assert_status(201);
    res.assert_no_sql_error();
    assert_eq!(res.json["success"], true);
    let n: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "syncDbLocal" WHERE "userId" = {} AND data = '{payload}' AND "takenDate" = '2035-04-01'"#,
            fx.staff.id
        ))
        .await;
    assert_eq!(n, 1);

    // Tanpa takenDate: memakai hari ini.
    let res = app
        .post("/trackings/sync-local-db")
        .token(&fx.staff.token)
        .json(&json!({ "data": "[]" }))
        .send()
        .await;
    res.assert_status(201);
    assert_eq!(res.json["success"], true);

    // takenDate tidak valid → success false (status tetap 201 seperti Node).
    let res = app
        .post("/trackings/sync-local-db?takenDate=bukan")
        .token(&fx.staff.token)
        .json(&json!({ "data": "[]" }))
        .send()
        .await;
    res.assert_status(201);
    assert_eq!(res.json["success"], false);

    // data bukan string → 400.
    app.post("/trackings/sync-local-db")
        .token(&fx.staff.token)
        .json(&json!({ "data": [] }))
        .send()
        .await
        .assert_status(400);
}

// ───────────────────────── integrasi + screenshot pada daftar ─────────────────────────

#[tokio::test]
async fn find_all_includes_screenshots_and_integrations() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let project = seed_project(&app, &fx).await;
    let ext: String = app
        .scalar("SELECT 'ext-' || substr(md5(random()::text), 1, 8)")
        .await;
    // Level 1: timeline dengan taskId.
    let task_id: i32 = app
        .scalar(&format!(
            r#"INSERT INTO task ("projectId", "companyId", source, "externalId", name, "lastSyncedAt")
               VALUES ({project}, {}, 'clickup', '{ext}', 'Task {ext}', NOW()) RETURNING id"#,
            fx.id
        ))
        .await;
    // Level 2: timeline dengan clickupTimeEntryId, dicocokkan ke task project.
    let (tid, _) = seed_tracking(&app, fx.staff.id, "2035-01-16").await;
    sqlx::query(
        r#"INSERT INTO "trackingTimeline" (note, "isOffline", "startAt", "endAt", "trackingId", "projectId", status, timezone, "taskId")
           VALUES ('with task', false, '2035-01-16 09:00:00', '2035-01-16 10:00:00', $1, $2, 1, 'Asia/Jakarta', $3)"#,
    )
    .bind(tid)
    .bind(project)
    .bind(task_id)
    .execute(app.db())
    .await
    .unwrap();
    sqlx::query(
        r#"INSERT INTO "trackingTimeline" (note, "isOffline", "startAt", "endAt", "trackingId", "projectId", status, timezone, "clickupTimeEntryId")
           VALUES ('clickup fallback', false, '2035-01-16 11:00:00', '2035-01-16 11:20:00', $1, $2, 1, 'Asia/Jakarta', 'ce-1')"#,
    )
    .bind(tid)
    .bind(project)
    .execute(app.db())
    .await
    .unwrap();
    // Screenshot status 3 untuk timeline pertama.
    let tl_first: i32 = app
        .scalar(&format!(
            r#"SELECT id FROM "trackingTimeline" WHERE "trackingId" = {tid} ORDER BY id LIMIT 1"#
        ))
        .await;
    app.exec(&format!(
        r#"INSERT INTO screenshot ("fileFullpath", "isBlur", "takenAt", "trackingTimelineId", "projectId", status)
           VALUES ('/tmp/none.png', true, '2035-01-16 09:30:00', {tl_first}, {project}, 3)"#
    ))
    .await;

    let res = app
        .get("/trackings?takenDate=2035-01-16")
        .token(&fx.staff.token)
        .send()
        .await;
    res.assert_ok();
    res.assert_no_sql_error();
    let tls = res.data()["result"][0]["trackingTimeline"].clone();
    let tls = tls.as_array().unwrap();
    assert_eq!(tls.len(), 2);
    // Timeline pertama: screenshot + integrasi via taskId.
    assert_eq!(tls[0]["screenshot"].as_array().unwrap().len(), 1);
    assert_eq!(tls[0]["screenshot"][0]["isBlur"], true);
    assert!(
        tls[0]["screenshot"][0]["screenshotUrl"]
            .as_str()
            .unwrap()
            .starts_with("/screenshots/")
    );
    assert_eq!(tls[0]["integration"]["platform"], "clickup");
    assert_eq!(
        tls[0]["integration"]["task"]["name"],
        format!("Task {ext}").as_str()
    );
    // Timeline kedua: fallback clickup berbasis project.
    assert_eq!(tls[1]["integration"]["platform"], "clickup");
    assert_eq!(
        tls[1]["integration"]["task"]["name"],
        format!("Task {ext}").as_str()
    );
    assert_eq!(res.data()["result"][0]["totalTakenInSecond"], 4800);
}
