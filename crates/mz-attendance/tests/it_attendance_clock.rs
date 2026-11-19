//! Uji integrasi route mobile (`/mobile/clock/**`, `/mobile/kyc/face`, `/mobile/dashboard/summary`,
//! `/mobile/company/config`, logo bertanda tangan) terhadap Postgres nyata + emulator GCS lokal.
//! Jalankan:
//! `MZ_TEST_DATABASE_URL=... MZ_TEST_REQUIRE_DB=1 cargo test -p mz-attendance --test it_attendance_clock`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_arguments
)]

mod common;

use common::{app, geofence, gps, multipart, reset};
use serde_json::{Value, json};

static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test]
async fn clock_and_break_flow() {
    let _g = SERIAL.lock().await;
    let Some((app, _em)) = app("flow").await else {
        return;
    };
    let c = app.company().await;
    reset(&app, c.id).await;
    let staff = &c.staff.token;

    // Tanpa token → 401; validasi DTO.
    app.post("/mobile/clock/in")
        .json(&json!({}))
        .send()
        .await
        .assert_status(401);
    let r = app
        .post("/mobile/clock/in")
        .token(staff)
        .json(&json!({}))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        json!(["clientTimestamp must be a number conforming to the specified constraints"])
    );
    let r = app
        .post("/mobile/clock/in")
        .token(staff)
        .json(&json!({ "clientTimestamp": 1, "gps": "nope" }))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(r.json["message"], "Invalid GPS JSON");

    // Tanpa geofence: clock-in unverified, geofenceId null.
    let r = app
        .post("/mobile/clock/in")
        .token(staff)
        .json(&json!({ "clientTimestamp": 1_749_430_800_000_i64, "note": "pagi" }))
        .send()
        .await;
    r.assert_status(201).assert_no_sql_error();
    let d = r.data();
    let keys: Vec<&str> = d.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "shiftId",
            "clockInAt",
            "geofenceId",
            "insideGeofence",
            "verificationStatus"
        ]
    );
    assert_eq!(d["geofenceId"], Value::Null);
    assert_eq!(d["insideGeofence"], false);
    assert_eq!(d["verificationStatus"], "unverified");
    let shift_id = d["shiftId"].as_str().unwrap().to_owned();

    let r = app
        .post("/mobile/clock/in")
        .token(staff)
        .json(&json!({ "clientTimestamp": 1 }))
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(
        r.json,
        json!({ "code": "CLOCK_ALREADY_IN", "message": "An active shift already exists" })
    );

    let r = app.get("/mobile/clock/current").token(staff).send().await;
    r.assert_ok();
    assert_eq!(r.data()["shiftId"], shift_id.as_str());
    assert_eq!(r.data()["note"], "pagi");
    assert_eq!(r.data()["activeBreak"], Value::Null);

    // Istirahat.
    let r = app
        .get("/mobile/clock/break/policy")
        .token(staff)
        .send()
        .await;
    assert_eq!(
        r.json,
        json!({ "data": { "maxMinutes": { "lunch": 60, "prayer": 30, "coffee": 15, "other": 60 }, "countTowardWorkHours": false } })
    );
    let r = app
        .post("/mobile/clock/break/start")
        .token(staff)
        .json(&json!({ "type": "bogus" }))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        json!(["type must be one of the following values: "])
    );
    let r = app
        .post("/mobile/clock/break/start")
        .token(staff)
        .json(&json!({ "type": "other" }))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(r.json["code"], "BREAK_LABEL_REQUIRED");
    let r = app
        .post("/mobile/clock/break/start")
        .token(staff)
        .json(&json!({ "type": "coffee" }))
        .send()
        .await;
    r.assert_status(201);
    assert_eq!(r.data()["type"], "coffee");
    assert_eq!(r.data()["maxMinutes"], 15);
    let r = app
        .post("/mobile/clock/break/start")
        .token(staff)
        .json(&json!({ "type": "lunch" }))
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(r.json["code"], "BREAK_ALREADY_ACTIVE");
    let r = app
        .get("/mobile/clock/break/current")
        .token(staff)
        .send()
        .await;
    r.assert_ok();
    assert_eq!(r.data()["customLabel"], Value::Null);
    let r = app.get("/mobile/clock/current").token(staff).send().await;
    assert_eq!(r.data()["activeBreak"]["type"], "coffee");
    let r = app
        .post("/mobile/clock/break/end")
        .token(staff)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.data()["exceeded"], false);
    let r = app
        .post("/mobile/clock/break/end")
        .token(staff)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json,
        json!({ "code": "BREAK_NOT_ACTIVE", "message": "No active break found" })
    );

    // Clock-out.
    let r = app
        .post("/mobile/clock/out")
        .token(staff)
        .json(&json!({ "clientTimestamp": 0 }))
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(r.data()["shiftId"], shift_id.as_str());
    assert_eq!(r.data()["totalBreakSeconds"], 0);
    let client_out: Option<chrono::NaiveDateTime> = app
        .scalar(&format!(
            r#"SELECT "clockOutClientAt" FROM "mobileShift" WHERE uuid = '{shift_id}'"#
        ))
        .await;
    assert_eq!(client_out, None);
    let r = app.get("/mobile/clock/current").token(staff).send().await;
    assert_eq!(r.json, json!({ "data": null }));
    let r = app
        .post("/mobile/clock/out")
        .token(staff)
        .json(&json!({}))
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(r.json["code"], "CLOCK_NOT_IN");
    let r = app
        .post("/mobile/clock/break/end")
        .token(staff)
        .send()
        .await;
    assert_eq!(
        r.json,
        json!({ "code": "BREAK_NOT_ACTIVE", "message": "No active shift found" })
    );
    let r = app
        .post("/mobile/clock/break/start")
        .token(staff)
        .json(&json!({ "type": "lunch" }))
        .send()
        .await;
    assert_eq!(r.json["code"], "BREAK_NO_ACTIVE_SHIFT");
    assert_eq!(
        app.get("/mobile/clock/break/current")
            .token(staff)
            .send()
            .await
            .json,
        json!({ "data": null })
    );

    // Riwayat sendiri.
    let r = app
        .get("/mobile/clock/attendance?startDate=2000-01-01&endDate=2100-01-01")
        .token(staff)
        .send()
        .await;
    r.assert_ok().assert_no_sql_error();
    assert_eq!(
        r.json["meta"],
        json!({ "total": 1, "page": 1, "limit": 20, "totalPages": 1 })
    );
    assert_eq!(r.json["summary"]["totalPresent"], 1);
    let row = &r.json["data"][0];
    assert_eq!(row["status"], "closed");
    assert_eq!(row["clockIn"]["hasSelfie"], false);
    assert_eq!(row["clockOut"]["isMock"], false);
    let r = app
        .get("/mobile/clock/attendance?startDate=2026-02-01&endDate=2026-01-01")
        .token(staff)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(r.json["message"], "endDate must be on or after startDate");
    reset(&app, c.id).await;
}

#[tokio::test]
async fn previous_day_shift_is_abandoned() {
    let _g = SERIAL.lock().await;
    let Some((app, _em)) = app("prev").await else {
        return;
    };
    let c = app.company().await;
    reset(&app, c.id).await;
    let staff = &c.staff.token;
    let old: (i32, String) = sqlx::query_as(
        r#"INSERT INTO "mobileShift" ("userId", "companyId", "clockInAt", status, source)
           VALUES ($1, $2, NOW() - interval '2 days', 'active', 'mobile') RETURNING id, uuid::text"#,
    )
    .bind(c.staff.id)
    .bind(c.id)
    .fetch_one(app.db())
    .await
    .unwrap();
    app.exec(&format!(
        r#"INSERT INTO "mobileBreak" ("shiftId", "userId", type, "startedAt", status)
           VALUES ({}, {}, 'lunch', NOW() - interval '2 days', 'active')"#,
        old.0, c.staff.id
    ))
    .await;
    // Shift kemarin dianggap tidak ada.
    assert_eq!(
        app.get("/mobile/clock/current")
            .token(staff)
            .send()
            .await
            .json,
        json!({ "data": null })
    );
    let r = app
        .post("/mobile/clock/out")
        .token(staff)
        .json(&json!({}))
        .send()
        .await;
    assert_eq!(r.json["code"], "CLOCK_NOT_IN");
    // Clock-in baru menutup shift kemarin tanpa jam pulang.
    let r = app
        .post("/mobile/clock/in")
        .token(staff)
        .json(&json!({ "clientTimestamp": "1749430800000" }))
        .send()
        .await;
    r.assert_status(201);
    let (status, out, dur): (String, Option<chrono::NaiveDateTime>, Option<i32>) = sqlx::query_as(
        r#"SELECT status::text, "clockOutAt", "durationSeconds" FROM "mobileShift" WHERE id = $1"#,
    )
    .bind(old.0)
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!((status.as_str(), out, dur), ("auto_closed", None, None));
    let brk: String = app
        .scalar(&format!(
            r#"SELECT status::text FROM "mobileBreak" WHERE "shiftId" = {}"#,
            old.0
        ))
        .await;
    assert_eq!(brk, "closed");

    // Auto-close: hanya admin; menutup shift aktif > thresholdHours.
    app.exec(&format!(
        r#"UPDATE "mobileShift" SET "clockInAt" = NOW() - interval '20 hours'
            WHERE "userId" = {} AND status = 'active'"#,
        c.staff.id
    ))
    .await;
    // RolesGuard Node: 401 `Access denied: insufficient permissions`.
    app.post("/mobile/clock/auto-close")
        .token(staff)
        .json(&json!({}))
        .send()
        .await
        .assert_status(401);
    let r = app
        .post("/mobile/clock/auto-close")
        .token(&c.admin.token)
        .json(&json!({ "thresholdHours": "abc" }))
        .send()
        .await;
    r.assert_status(400);
    let r = app
        .post("/mobile/clock/auto-close")
        .token(&c.admin.token)
        .json(&json!({ "thresholdHours": 30 }))
        .send()
        .await;
    assert_eq!(
        r.json,
        json!({ "data": { "closedCount": 0, "shiftIds": [] } })
    );
    let r = app
        .post("/mobile/clock/auto-close")
        .token(&c.admin.token)
        .json(&json!({}))
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(r.data()["closedCount"], 1);
    let st: String = app
        .scalar(&format!(
            r#"SELECT status::text FROM "mobileShift" WHERE uuid = '{}'"#,
            r.data()["shiftIds"][0].as_str().unwrap()
        ))
        .await;
    assert_eq!(st, "auto_closed");
    reset(&app, c.id).await;
}

#[tokio::test]
async fn geofence_and_negative_zone_rules() {
    let _g = SERIAL.lock().await;
    let Some((app, _em)) = app("geo").await else {
        return;
    };
    let c = app.company().await;
    reset(&app, c.id).await;
    let staff = &c.staff.token;
    let clock_in = |g: Option<String>| {
        let mut body = json!({ "clientTimestamp": 1 });
        if let Some(g) = g {
            body["gps"] = Value::String(g);
        }
        app.post("/mobile/clock/in").token(staff).json(&body).send()
    };

    // Geofence tak enforced: di luar → geofence terdekat, insideGeofence=false.
    let (_, far_uuid) = geofence(
        &app, c.id, c.admin.id, "Jauh", -6.3, 106.9, 100, false, None,
    )
    .await;
    let (_, near_uuid) = geofence(
        &app, c.id, c.admin.id, "Dekat", -6.2, 106.8, 100, false, None,
    )
    .await;
    let r = clock_in(Some(gps(-6.21, 106.8, false))).await;
    r.assert_status(201);
    assert_eq!(r.data()["geofenceId"], near_uuid.as_str());
    assert_eq!(r.data()["insideGeofence"], false);
    assert_eq!(r.data()["verificationStatus"], "unverified");
    reset_shifts(&app, c.id).await;
    // Tanpa GPS → geofence tertua (id terkecil).
    let r = clock_in(None).await;
    assert_eq!(r.data()["geofenceId"], far_uuid.as_str());
    reset_shifts(&app, c.id).await;
    // GPS mock → failed.
    let r = clock_in(Some(gps(-6.2, 106.8, true))).await;
    assert_eq!(r.data()["verificationStatus"], "failed");
    reset_shifts(&app, c.id).await;
    // Di dalam → verified.
    let r = clock_in(Some(gps(-6.2, 106.8, false))).await;
    assert_eq!(r.data()["geofenceId"], near_uuid.as_str());
    assert_eq!(r.data()["insideGeofence"], true);
    assert_eq!(r.data()["verificationStatus"], "verified");
    reset_shifts(&app, c.id).await;

    // Geofence enforced.
    let (_, enf_uuid) = geofence(
        &app, c.id, c.admin.id, "Wajib", -6.25, 106.85, 50, true, None,
    )
    .await;
    let r = clock_in(None).await;
    r.assert_status(403);
    assert_eq!(
        r.json,
        json!({ "code": "CLOCK_LOCATION_REQUIRED", "message": "A valid (non-mock) location is required to clock in at an enforced geofence" })
    );
    // Di dalam geofence tak enforced tidak cukup.
    let r = clock_in(Some(gps(-6.2, 106.8, false))).await;
    r.assert_status(403);
    assert_eq!(r.json["code"], "CLOCK_OUTSIDE_GEOFENCE");
    assert_eq!(
        r.json["details"]["nearestGeofence"]["id"],
        enf_uuid.as_str()
    );
    assert_eq!(r.json["details"]["nearestGeofence"]["radiusMeters"], 50);
    assert!(r.json["details"]["distanceMeters"].as_i64().unwrap() > 50);
    let r = clock_in(Some(gps(-6.25, 106.85, false))).await;
    r.assert_status(201);
    assert_eq!(r.data()["geofenceId"], enf_uuid.as_str());
    // Clock-out di dalam geofence → verified + clockOutGeofenceId.
    let r = app
        .post("/mobile/clock/out")
        .token(staff)
        .json(
            &json!({ "gps": gps(-6.25, 106.85, false), "clientTimestamp": 1_749_459_600_000_i64 }),
        )
        .send()
        .await;
    r.assert_status(200);
    let verified: String = app
        .scalar(&format!(
            r#"SELECT "clockOutVerified"::text FROM "mobileShift" WHERE uuid = '{}'"#,
            r.data()["shiftId"].as_str().unwrap()
        ))
        .await;
    assert_eq!(verified, "verified");
    reset(&app, c.id).await;

    // Negative zone.
    app.exec(&format!(
        r#"INSERT INTO "negativeZone" ("companyId", name, lat, lng, "radiusMeters", "createdById")
           VALUES ({}, 'Zona Larangan', -6.4, 106.7, 200, {})"#,
        c.id, c.admin.id
    ))
    .await;
    let r = clock_in(None).await;
    r.assert_status(403);
    assert_eq!(
        r.json["message"],
        "A valid (non-mock) location is required because this company has negative zones configured"
    );
    let r = clock_in(Some(gps(-6.4, 106.7, false))).await;
    r.assert_status(403);
    assert_eq!(r.json["code"], "CLOCK_INSIDE_NEGATIVE_ZONE");
    assert_eq!(r.json["details"]["negativeZone"]["name"], "Zona Larangan");
    assert_eq!(r.json["details"]["distanceMeters"], 0);
    clock_in(Some(gps(-6.2, 106.8, false)))
        .await
        .assert_status(201);
    // Clock-out tanpa GPS tetap boleh; di dalam zona ditolak.
    let r = app
        .post("/mobile/clock/out")
        .token(staff)
        .json(&json!({ "gps": gps(-6.4, 106.7, false) }))
        .send()
        .await;
    assert_eq!(r.json["code"], "CLOCK_INSIDE_NEGATIVE_ZONE");
    app.post("/mobile/clock/out")
        .token(staff)
        .json(&json!({}))
        .send()
        .await
        .assert_status(200);
    reset(&app, c.id).await;
}

async fn reset_shifts(app: &mz_testkit::TestApp, company_id: i32) {
    app.exec(&format!(
        r#"DELETE FROM "mobileShift" WHERE "companyId" = {company_id}"#
    ))
    .await;
}

#[tokio::test]
async fn selfie_mock_and_multipart() {
    let _g = SERIAL.lock().await;
    let Some((app, em)) = app("selfie").await else {
        return;
    };
    let c = app.company().await;
    reset(&app, c.id).await;
    let staff = &c.staff.token;
    app.exec(&format!(
        r#"UPDATE company SET "mobileBlockMockLocation" = true, "mobileRequireSelfie" = true WHERE id = {}"#,
        c.id
    ))
    .await;
    let (ct, body) = multipart(
        &[
            ("clientTimestamp", "1749430800000"),
            ("gps", &gps(-6.2, 106.8, true)),
        ],
        None,
    );
    let r = app
        .post("/mobile/clock/in")
        .token(staff)
        .raw(&ct, body)
        .send()
        .await;
    r.assert_status(403);
    assert_eq!(
        r.json,
        json!({ "code": "CLOCK_MOCK_LOCATION_BLOCKED", "message": "Mock location is not allowed" })
    );
    let (ct, body) = multipart(&[("clientTimestamp", "1749430800000")], None);
    let r = app
        .post("/mobile/clock/in")
        .token(staff)
        .raw(&ct, body)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json,
        json!({ "code": "CLOCK_SELFIE_REQUIRED", "message": "Selfie photo is required" })
    );
    // Berkas di field lain → multer `Unexpected field`.
    let (ct, body) = multipart(
        &[("clientTimestamp", "1")],
        Some(("selfie", "a.jpg", "image/jpeg", b"x")),
    );
    let r = app
        .post("/mobile/clock/in")
        .token(staff)
        .raw(&ct, body)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(r.json["message"], "Unexpected field");

    let photo: &[u8] = b"\xff\xd8\xffjpeg-in";
    let (ct, body) = multipart(
        &[("clientTimestamp", "1749430800000"), ("note", "kantor")],
        Some(("photo", "in.jpg", "image/jpeg", photo)),
    );
    let r = app
        .post("/mobile/clock/in")
        .token(staff)
        .raw(&ct, body)
        .send()
        .await;
    r.assert_status(201).assert_no_sql_error();
    let shift = r.data()["shiftId"].as_str().unwrap().to_owned();
    let key: String = app
        .scalar(&format!(
            r#"SELECT "clockInSelfieKey" FROM "mobileShift" WHERE uuid = '{shift}'"#
        ))
        .await;
    assert!(
        key.starts_with(&format!("selfies/{}/{}/", c.id, c.staff.id)) && key.ends_with(".jpg"),
        "{key}"
    );
    assert_eq!(
        em.get(&key).unwrap(),
        ("image/jpeg".to_owned(), photo.to_vec())
    );

    let r = app
        .get(&format!("/mobile/clock/shifts/{shift}/selfie"))
        .token(staff)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.bytes, photo);
    assert_eq!(r.headers["content-type"], "image/jpeg");
    assert_eq!(r.headers["cache-control"], "no-store");
    let r = app
        .get(&format!("/mobile/clock/shifts/{shift}/selfie?type=out"))
        .token(staff)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(r.json["message"], "No selfie available for this punch");
    let r = app
        .get(&format!("/mobile/clock/shifts/{shift}/selfie"))
        .token(&c.staff2.token)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(r.json["message"], "Shift not found");

    let out_photo: &[u8] = b"\xff\xd8\xffjpeg-out";
    let (ct, body) = multipart(&[], Some(("photo", "out.jpg", "image/jpeg", out_photo)));
    let r = app
        .post("/mobile/clock/out")
        .token(staff)
        .raw(&ct, body)
        .send()
        .await;
    r.assert_status(200);
    let out_key: String = app
        .scalar(&format!(
            r#"SELECT "clockOutSelfieKey" FROM "mobileShift" WHERE uuid = '{shift}'"#
        ))
        .await;
    assert!(out_key.ends_with("_out.jpg"), "{out_key}");
    let r = app
        .get(&format!("/mobile/clock/shifts/{shift}/selfie?type=out"))
        .token(staff)
        .send()
        .await;
    assert_eq!(r.bytes, out_photo);
    reset(&app, c.id).await;
}

#[tokio::test]
async fn kyc_face() {
    let _g = SERIAL.lock().await;
    let Some((app, _em)) = app("kyc").await else {
        return;
    };
    let c = app.company().await;
    let staff = &c.staff.token;
    app.exec(&format!(
        r#"DELETE FROM "userKycFace" WHERE "userId" = {}"#,
        c.staff.id
    ))
    .await;
    let r = app.get("/mobile/kyc/face").token(staff).send().await;
    assert_eq!(r.json, json!({ "data": { "faces": null } }));
    let r = app
        .post("/mobile/kyc/face")
        .token(staff)
        .json(&json!({}))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        json!(["faces should not be empty", "faces must be a string"])
    );
    let faces = r#"{"embeddings":[[1.2,0.3]],"angleCount":1}"#;
    let r = app
        .post("/mobile/kyc/face")
        .token(staff)
        .json(&json!({ "faces": faces }))
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json, json!({ "data": { "faces": faces } }));
    let r = app
        .post("/mobile/kyc/face")
        .token(staff)
        .json(&json!({ "faces": "v2" }))
        .send()
        .await;
    r.assert_status(200);
    let r = app.get("/mobile/kyc/face").token(staff).send().await;
    assert_eq!(r.json, json!({ "data": { "faces": "v2" } }));
}

#[tokio::test]
async fn dashboard_summary() {
    let _g = SERIAL.lock().await;
    let Some((app, _em)) = app("dash").await else {
        return;
    };
    let c = app.company().await;
    reset(&app, c.id).await;
    let now = chrono::Utc::now().naive_utc();
    let today_start = now.date().and_hms_opt(0, 0, 1).unwrap();
    app.exec(&format!(
        r#"INSERT INTO "mobileShift" ("userId", "companyId", "clockInAt", "clockOutAt", "durationSeconds", status, source)
           VALUES ({uid}, {cid}, '{t}', '{t}'::timestamp + interval '1 hour', 3600, 'closed', 'mobile');
           INSERT INTO "mobileBreak" ("shiftId", "userId", type, "startedAt", "durationSeconds", status)
           SELECT id, {uid}, 'lunch', '{t}', 600, 'closed' FROM "mobileShift" WHERE "userId" = {uid} AND "companyId" = {cid};"#,
        uid = c.staff.id,
        cid = c.id,
        t = today_start.format("%Y-%m-%d %H:%M:%S")
    ))
    .await;
    let r = app
        .get("/mobile/dashboard/summary")
        .token(&c.staff.token)
        .send()
        .await;
    r.assert_ok().assert_no_sql_error();
    let d = r.data();
    let keys: Vec<&str> = d.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "weekRange",
            "todayCheckInAt",
            "days",
            "weeklyTotals",
            "topProjects",
            "upcomingEvents"
        ]
    );
    assert_eq!(d["days"].as_array().unwrap().len(), 7);
    assert_eq!(d["days"][0]["dayName"], "Monday");
    assert_eq!(
        d["todayCheckInAt"],
        format!("{}.000Z", today_start.format("%Y-%m-%dT%H:%M:%S"))
    );
    assert_eq!(d["weeklyTotals"]["totalWorkSeconds"], 3600);
    assert_eq!(d["weeklyTotals"]["totalBreakSeconds"], 600);
    app.get("/mobile/dashboard/summary")
        .send()
        .await
        .assert_status(401);
    reset(&app, c.id).await;
}

#[tokio::test]
async fn company_config_and_signed_logo() {
    let _g = SERIAL.lock().await;
    let Some((app, em)) = app("config").await else {
        return;
    };
    let c = app.company().await;
    let old_path: Option<String> = app
        .scalar(&format!(
            r#"SELECT "profilePictureFullPath" FROM company WHERE id = {}"#,
            c.id
        ))
        .await;
    // Upload resmi menulis `/companies/<uuid>/<file>` (garis miring ikut jadi bagian key).
    let logo_key = format!("/companies/{}/logo.png", c.uuid);
    app.exec(&format!(
        r#"UPDATE company SET "profilePictureFullPath" = '{logo_key}' WHERE id = {}"#,
        c.id
    ))
    .await;
    em.put(&logo_key, "image/png", b"\x89PNGlogo");

    let r = app
        .get("/mobile/company/config")
        .token(&c.admin.token)
        .send()
        .await;
    r.assert_ok().assert_no_sql_error();
    let d = r.data();
    let keys: Vec<&str> = d.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "company",
            "modules",
            "attendance",
            "break",
            "storeVisit",
            "leave",
            "features",
            "limits",
            "integrations",
            "subscriptionLock"
        ]
    );
    assert_eq!(d["company"]["id"], c.uuid.as_str());
    assert_eq!(d["break"]["maxMinutesLunch"], 60);
    assert_eq!(d["modules"], json!(["TIME_TRACKING", "SALES_TRACKER"]));
    assert_eq!(d["subscriptionLock"]["locked"], false);
    assert!(d["company"]["pricingTier"].is_object());
    assert_eq!(d["company"]["dailyReportSentAt"].as_str().unwrap().len(), 8);
    let logo_url = d["company"]["logoUrl"].as_str().unwrap().to_owned();
    let prefix = format!("http://be.test/v1/mobile/companies/{}/logo?exp=", c.uuid);
    assert!(logo_url.starts_with(&prefix), "{logo_url}");
    let path = logo_url.trim_start_matches("http://be.test");

    // Proxy logo tanpa JWT.
    let r = app.get(path).send().await;
    r.assert_status(200);
    assert_eq!(r.bytes, b"\x89PNGlogo");
    assert_eq!(r.headers["content-type"], "image/png");
    assert_eq!(r.headers["cache-control"], "private, max-age=86400");
    assert_eq!(r.headers["referrer-policy"], "no-referrer");
    let bad = format!("{}x", path);
    let r = app.get(&bad).send().await;
    r.assert_status(404);
    assert_eq!(
        r.json,
        json!({ "code": "COMPANY_LOGO_NOT_FOUND", "message": "Logo not found" })
    );
    assert_eq!(r.headers["x-robots-tag"], "noindex");
    let r = app.get("/v1/mobile/companies/not-a-uuid/logo").send().await;
    r.assert_status(400);
    assert_eq!(r.json["message"], "Validation failed (uuid is expected)");
    // Berkas bukan gambar → 404 yang sama.
    em.put(&logo_key, "application/pdf", b"%PDF");
    app.get(path).send().await.assert_status(404);

    // Logo di luar folder company → null.
    app.exec(&format!(
        r#"UPDATE company SET "profilePictureFullPath" = 'screenshots/x.png' WHERE id = {}"#,
        c.id
    ))
    .await;
    let r = app
        .get("/mobile/company/config")
        .token(&c.admin.token)
        .send()
        .await;
    assert_eq!(r.data()["company"]["logoUrl"], Value::Null);
    app.get("/mobile/company/config")
        .send()
        .await
        .assert_status(401);

    sqlx::query(r#"UPDATE company SET "profilePictureFullPath" = $1 WHERE id = $2"#)
        .bind(old_path)
        .bind(c.id)
        .execute(app.db())
        .await
        .unwrap();
}
