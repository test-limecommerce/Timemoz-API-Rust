//! Kunjungan mobile (`/mobile/store-visits/**`, proxy foto `/v1/mobile/store-visits/.../file`,
//! cron sapu). Sales uji = `staff2` (satu kunjungan aktif per user: semua test di sini memegang
//! [`SERIAL`] dan membatalkan kunjungan aktif staff2 sebelum mulai).

use mz_testkit::{Company, TestApp};
use serde_json::{Value, json};

use super::{SERIAL, insert_store, setup, tag};

const LAT: f64 = -7.7501067;
const LNG: f64 = 110.2973078;

async fn reset(app: &TestApp, c: &Company, enforce: bool) {
    app.exec(&format!(
        r#"UPDATE "storeVisit" SET state = 'cancelled' WHERE "userId" = {uid} AND state = 'active';
           UPDATE company SET "storeVisitDefaultRadiusMeters" = 150, "storeVisitMaxAccuracyMeters" = 100,
                  "storeVisitBlockMockLocation" = true, "storeVisitRequirePhotos" = true,
                  "storeVisitAllowOverride" = false, "storeVisitMinDurationMinutes" = 0,
                  "storeVisitCountsAsAttendance" = true, "storeVisitEnforceGroupScope" = {enforce}
            WHERE id = {cid};"#,
        uid = c.staff2.id,
        cid = c.id
    ))
    .await;
}

fn uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn check_in_body(store: &str, crid: &str, lat: f64, lng: f64) -> Value {
    json!({ "storeId": store, "clientRequestId": crid, "lat": lat, "lng": lng, "accuracyMeters": 10 })
}

#[tokio::test]
async fn full_visit_flow() {
    let _g = SERIAL.lock().await;
    let Some((app, em, c)) = setup().await else {
        return;
    };
    reset(&app, &c, false).await;
    let t = &c.staff2.token;
    let code = tag();
    let (store_id, store_uuid) = insert_store(&app, c.id, c.admin.id, &code, LAT, LNG, None).await;

    // ── 401 / validasi / 404 ──
    app.get("/mobile/store-visits/active")
        .send()
        .await
        .assert_status(401);
    let r = app
        .post("/mobile/store-visits/validate-location")
        .token(t)
        .json(&json!({}))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(r.json["message"][0], "storeId must be a UUID");
    let r = app
        .post("/mobile/store-visits/validate-location")
        .token(t)
        .json(&json!({ "storeId": uuid(), "lat": LAT, "lng": LNG, "accuracyMeters": 5 }))
        .send()
        .await;
    r.assert_status(404).assert_no_sql_error();
    assert_eq!(r.json["code"], "STORE_NOT_FOUND");
    app.get("/mobile/store-visits/validate-location")
        .token(t)
        .send()
        .await
        .assert_status(400);
    app.get("/mobile/store-visits/not-a-uuid")
        .token(t)
        .send()
        .await
        .assert_status(400);

    // ── validate-location ──
    let r = app
        .post("/mobile/store-visits/validate-location")
        .token(t)
        .json(&json!({ "storeId": store_uuid, "lat": LAT, "lng": LNG, "accuracyMeters": 5 }))
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(r.json["data"]["gpsValidation"], "valid");
    assert_eq!(r.json["data"]["canCheckIn"], true);

    // ── check-in ditolak: di luar radius (beraudit), mock ──
    let r = app
        .post("/mobile/store-visits")
        .token(t)
        .json(&check_in_body(&store_uuid, &uuid(), -7.80, 110.30))
        .send()
        .await;
    r.assert_status(422).assert_no_sql_error();
    assert_eq!(r.json["code"], "VISIT_OUT_OF_RADIUS");
    assert!(r.json["details"]["distanceMeters"].as_i64().unwrap() > 150);
    let audits: i64 = app
        .scalar(&format!(r#"SELECT COUNT(*) FROM "storeVisitAuditLog" WHERE "storeId" = {store_id} AND event = 'outOfRadius'"#))
        .await;
    assert_eq!(audits, 1);
    let mut mock = check_in_body(&store_uuid, &uuid(), LAT, LNG);
    mock["isMock"] = json!(true);
    let r = app
        .post("/mobile/store-visits")
        .token(t)
        .json(&mock)
        .send()
        .await;
    r.assert_status(422);
    assert_eq!(r.json["code"], "VISIT_MOCK_LOCATION_BLOCKED");

    // ── check-in sukses + replay ──
    let crid = uuid();
    let r = app
        .post("/mobile/store-visits")
        .token(t)
        .json(&check_in_body(&store_uuid, &crid, LAT, LNG))
        .send()
        .await;
    r.assert_status(201).assert_no_sql_error();
    let visit = r.json["data"].clone();
    let visit_uuid = visit["id"].as_str().unwrap().to_owned();
    assert!(visit["visitNumber"].as_str().unwrap().starts_with("VST-"));
    assert_eq!(visit["state"], "active");
    assert_eq!(visit["photosRequired"], json!(["store", "selfie"]));
    assert_eq!(visit["checkOutAllowedAt"], Value::Null);
    assert_eq!(visit["plan"], Value::Null);
    assert!(visit.get("isMock").is_none());
    let r = app
        .post("/mobile/store-visits")
        .token(t)
        .json(&check_in_body(&store_uuid, &crid, -7.9, 110.4))
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json["data"]["id"], visit_uuid.as_str());
    let r = app
        .post("/mobile/store-visits")
        .token(t)
        .json(&check_in_body(&store_uuid, &uuid(), LAT, LNG))
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(r.json["code"], "VISIT_ALREADY_ACTIVE");

    // ── baca ──
    let r = app.get("/mobile/store-visits/active").token(t).send().await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(r.json["data"]["id"], visit_uuid.as_str());
    assert_eq!(r.json["data"]["photosCount"], 0);
    let r = app
        .get(&format!("/mobile/store-visits/{visit_uuid}"))
        .token(t)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(r.json["data"]["store"]["ownerName"], "Owner");
    assert_eq!(r.json["data"]["checkOut"], Value::Null);
    app.get(&format!("/mobile/store-visits/{}", uuid()))
        .token(t)
        .send()
        .await
        .assert_status(404);
    // Kunjungan sales lain = 404.
    app.get(&format!("/mobile/store-visits/{visit_uuid}"))
        .token(&c.staff.token)
        .send()
        .await
        .assert_status(404);

    // ── check-out: foto wajib, catatan wajib ──
    let co = json!({ "status": "success", "lat": LAT, "lng": LNG, "accuracyMeters": 8, "tags": ["promo"] });
    let r = app
        .patch(&format!("/mobile/store-visits/{visit_uuid}/check-out"))
        .token(t)
        .json(&co)
        .send()
        .await;
    r.assert_status(422).assert_no_sql_error();
    assert_eq!(r.json["code"], "VISIT_PHOTO_REQUIRED");
    assert_eq!(
        r.json["details"]["missingPhotoKinds"],
        json!(["store", "selfie"])
    );

    let mut photo_ids = Vec::new();
    for kind in ["store", "selfie"] {
        let r = app
            .post(&format!(
                "/mobile/store-visits/{visit_uuid}/photos/upload-url"
            ))
            .token(t)
            .json(&json!({ "kind": kind, "contentType": "image/jpeg", "sizeBytes": 1000 }))
            .send()
            .await;
        r.assert_status(201).assert_no_sql_error();
        let key = r.json["data"]["objectKey"].as_str().unwrap().to_owned();
        let pid = r.json["data"]["photoId"].as_str().unwrap().to_owned();
        // Konfirmasi sebelum PUT → 422.
        let r = app
            .post(&format!(
                "/mobile/store-visits/{visit_uuid}/photos/{pid}/confirm"
            ))
            .token(t)
            .json(&json!({ "takenAt": 1_784_589_180_000_i64 }))
            .send()
            .await;
        r.assert_status(422);
        assert_eq!(r.json["code"], "PHOTO_NOT_UPLOADED");
        em.put(&key, b"\xff\xd8jpeg", "image/jpeg");
        let r = app
            .post(&format!(
                "/mobile/store-visits/{visit_uuid}/photos/{pid}/confirm"
            ))
            .token(t)
            .json(&json!({ "takenAt": 1_784_589_180_000_i64, "lat": LAT, "lng": LNG }))
            .send()
            .await;
        r.assert_status(200).assert_no_sql_error();
        assert_eq!(r.json["data"]["kind"], kind);
        photo_ids.push(pid);
    }
    let r = app
        .post(&format!(
            "/mobile/store-visits/{visit_uuid}/photos/upload-url"
        ))
        .token(t)
        .json(&json!({ "kind": "result", "contentType": "image/gif", "sizeBytes": 1000 }))
        .send()
        .await;
    r.assert_status(422);
    assert_eq!(r.json["code"], "PHOTO_CONTENT_TYPE_INVALID");

    let other =
        json!({ "status": "other", "lat": LAT, "lng": LNG, "accuracyMeters": 8, "note": "  " });
    let r = app
        .patch(&format!("/mobile/store-visits/{visit_uuid}/check-out"))
        .token(t)
        .json(&other)
        .send()
        .await;
    r.assert_status(422);
    assert_eq!(r.json["code"], "VISIT_NOTE_REQUIRED");

    let r = app
        .patch(&format!("/mobile/store-visits/{visit_uuid}/check-out"))
        .token(t)
        .json(&co)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(r.json["data"]["state"], "completed");
    assert_eq!(r.json["data"]["photosCount"], 2);
    assert_eq!(r.json["data"]["checkOut"]["gpsValidation"], "valid");
    let total: i32 = app
        .scalar(&format!(
            r#"SELECT "totalVisits" FROM store WHERE id = {store_id}"#
        ))
        .await;
    assert_eq!(total, 1);
    let r = app
        .patch(&format!("/mobile/store-visits/{visit_uuid}/check-out"))
        .token(t)
        .json(&co)
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(r.json["code"], "VISIT_NOT_ACTIVE");

    // ── detail dengan URL foto bertanda tangan → proxy ──
    let r = app
        .get(&format!("/mobile/store-visits/{visit_uuid}"))
        .token(t)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json["data"]["tags"], json!(["promo"]));
    assert_eq!(r.json["data"]["photos"].as_array().unwrap().len(), 2);
    let url = r.json["data"]["photos"][0]["url"]
        .as_str()
        .unwrap()
        .to_owned();
    let path = url.strip_prefix("https://api.test").unwrap();
    let f = app.get(path).send().await;
    f.assert_status(200);
    assert_eq!(f.bytes, b"\xff\xd8jpeg");
    assert_eq!(f.headers.get("x-robots-tag").unwrap(), "noindex");
    let bad = path.replace("sig=", "sig=x");
    let f = app.get(&bad).send().await;
    f.assert_status(404);
    assert_eq!(f.json["code"], "VISIT_PHOTO_NOT_FOUND");
    app.get(&format!(
        "/v1/mobile/store-visits/{visit_uuid}/photos/nope/file"
    ))
    .send()
    .await
    .assert_status(400);

    // ── bookend + shift absensi (storeVisitCountsAsAttendance) ──
    let r = app
        .get(&format!("/mobile/store-visits/event?visitId={visit_uuid}"))
        .token(t)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    let events: Vec<&str> = r.json["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["event"].as_str().unwrap())
        .collect();
    assert_eq!(events.len(), 2);
    assert!(events.contains(&"enter") && events.contains(&"exit"));
    let shifts: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "mobileShift" WHERE "userId" = {} AND note LIKE 'Auto clock-in from store visit %'"#,
            c.staff2.id
        ))
        .await;
    assert!(shifts >= 1);
    let r = app
        .post("/mobile/store-visits/event")
        .token(t)
        .json(&json!({"lat": LAT, "lng": LNG, "accuracy": 5, "timestamp": 1}))
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(r.json["code"], "NO_ACTIVE_VISIT");

    // ── list & summary ──
    let r = app
        .get(&format!(
            "/mobile/store-visits?storeId={store_uuid}&status=success"
        ))
        .token(t)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(r.json["meta"]["total"], 1);
    assert_eq!(r.json["data"][0]["photosCount"], 2);
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let r = app
        .get(&format!("/mobile/store-visits/summary?date={today}"))
        .token(t)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert!(r.json["data"]["byStatus"]["success"].as_i64().unwrap() >= 1);
    app.get("/mobile/store-visits/summary")
        .token(t)
        .send()
        .await
        .assert_status(400);

    // ── kunjungan kedua: jadwal hari ini diklaim, ping event, cancel melepas klaim ──
    let plan_id: i32 = app
        .scalar(&format!(
            r#"INSERT INTO "storeVisitPlan" ("companyId", "userId", "storeId", "visitDate", note, "createdById")
               VALUES ({}, {}, {store_id}, (now() AT TIME ZONE 'UTC')::date, 'tagih invoice', {}) RETURNING id"#,
            c.id, c.staff2.id, c.admin.id
        ))
        .await;
    let r = app
        .post("/mobile/store-visits")
        .token(t)
        .json(&check_in_body(&store_uuid, &uuid(), LAT, LNG))
        .send()
        .await;
    r.assert_status(201).assert_no_sql_error();
    assert_eq!(r.json["data"]["plan"]["note"], "tagih invoice");
    let v2 = r.json["data"]["id"].as_str().unwrap().to_owned();
    let r = app
        .post("/mobile/store-visits/event")
        .token(t)
        .json(&json!({"lat": LAT, "lng": LNG, "accuracy": 5, "timestamp": 1_784_589_180_000_i64}))
        .send()
        .await;
    r.assert_status(201).assert_no_sql_error();
    assert_eq!(r.json["data"]["event"], "enter");
    let r = app
        .patch(&format!("/mobile/store-visits/{v2}/cancel"))
        .token(t)
        .json(&json!({"reason": " salah toko "}))
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(r.json["data"], json!({ "id": v2, "state": "cancelled" }));
    let (note, plan): (Option<String>, Option<i32>) =
        sqlx::query_as(r#"SELECT note, "planId" FROM "storeVisit" WHERE uuid = $1::uuid"#)
            .bind(&v2)
            .fetch_one(app.db())
            .await
            .unwrap();
    assert_eq!(note.as_deref(), Some("salah toko"));
    assert_eq!(plan, None);
    app.patch(&format!("/mobile/store-visits/{v2}/cancel"))
        .token(t)
        .json(&json!({}))
        .send()
        .await
        .assert_status(409);
    app.exec(&format!(
        r#"DELETE FROM "storeVisitPlan" WHERE id = {plan_id}"#
    ))
    .await;

    reset(&app, &c, false).await;
}

#[tokio::test]
async fn out_of_scope_and_auto_close() {
    let _g = SERIAL.lock().await;
    let Some((app, _em, c)) = setup().await else {
        return;
    };
    reset(&app, &c, true).await;
    let t = &c.staff2.token;
    let (store_id, store_uuid) = insert_store(&app, c.id, c.admin.id, &tag(), LAT, LNG, None).await;

    // Enforcement on + toko tanpa region → 403 beraudit.
    let r = app
        .post("/mobile/store-visits")
        .token(t)
        .json(&check_in_body(&store_uuid, &uuid(), LAT, LNG))
        .send()
        .await;
    r.assert_status(403).assert_no_sql_error();
    assert_eq!(r.json["code"], "STORE_OUT_OF_SCOPE");
    let n: i64 = app
        .scalar(&format!(r#"SELECT COUNT(*) FROM "storeVisitAuditLog" WHERE "storeId" = {store_id} AND event = 'outOfScope'"#))
        .await;
    assert_eq!(n, 1);

    // Auto-close: kunjungan aktif berumur 20 jam dibatalkan oleh sapuan.
    reset(&app, &c, false).await;
    let r = app
        .post("/mobile/store-visits")
        .token(t)
        .json(&check_in_body(&store_uuid, &uuid(), LAT, LNG))
        .send()
        .await;
    r.assert_status(201);
    let v = r.json["data"]["id"].as_str().unwrap().to_owned();
    app.exec(&format!(
        r#"UPDATE "storeVisit" SET "checkInAt" = now() - interval '20 hours' WHERE uuid = '{v}'"#
    ))
    .await;
    let res = mz_store_visits::visits::auto_close::run_hourly_sweep(app.state()).await;
    assert!(res.auto_close.unwrap().cancelled >= 1);
    assert!(res.photos.is_some());
    let state: String = app
        .scalar(&format!(
            r#"SELECT state::text FROM "storeVisit" WHERE uuid = '{v}'"#
        ))
        .await;
    assert_eq!(state, "cancelled");
    reset(&app, &c, false).await;
}
