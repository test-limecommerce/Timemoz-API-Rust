//! Uji integrasi route admin `/v1/admin/clock/**` (daftar, selfie, detail, ekspor Excel/PDF,
//! selfie stream/URL) terhadap Postgres nyata + emulator GCS lokal. Jalankan:
//! `MZ_TEST_DATABASE_URL=... MZ_TEST_REQUIRE_DB=1 cargo test -p mz-attendance --test it_attendance_admin`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_arguments
)]

mod common;

use common::{app, geofence, reset};
use mz_testkit::{Company, TestApp};
use serde_json::{Value, json};

static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const SCHEDULE: &str = r#"{"monday":{"enabled":true,"startTime":"09:00 AM","endTime":"05:00 PM"},
  "tuesday":{"enabled":true,"startTime":"09:00 AM","endTime":"05:00 PM"}}"#;

struct Fx {
    gf_uuid: String,
    late: String,
    active: String,
}

/// Dua shift staff: Senin 2026-06-01 09:30Z (telat 30 mnt, ditutup, selfie in/out) dan
/// Selasa 2026-06-02 08:00Z (aktif, tanpa selfie) di geofence berjadwal 09:00–17:00 UTC.
async fn seed(app: &TestApp, c: &Company) -> Fx {
    reset(app, c.id).await;
    let (gf_id, gf_uuid) = geofence(
        app,
        c.id,
        c.admin.id,
        "Kantor Pusat",
        -6.2,
        106.8,
        100,
        false,
        Some(SCHEDULE),
    )
    .await;
    let late: String = sqlx::query_scalar(
        r#"INSERT INTO "mobileShift" ("userId", "companyId", "clockInAt", "clockOutAt", "durationSeconds",
                                      status, source, "clockInLat", "clockInLng", "clockInAccuracy",
                                      "clockInGeofenceId", "clockInSelfieKey", "clockInVerified",
                                      "clockOutGeofenceId", "clockOutSelfieKey", "clockOutVerified", "clockOutIsMock", note)
           VALUES ($1, $2, '2026-06-01 09:30:00', '2026-06-01 13:30:00', 14400, 'auto_closed', 'mobile',
                   -6.2, 106.8, 5.5, $3, 'selfies/x/in.jpg', 'verified', $3, 'selfies/x/out.jpg', 'verified', false, 'catatan')
           RETURNING uuid::text"#,
    )
    .bind(c.staff.id)
    .bind(c.id)
    .bind(gf_id)
    .fetch_one(app.db())
    .await
    .unwrap();
    let active: String = sqlx::query_scalar(
        r#"INSERT INTO "mobileShift" ("userId", "companyId", "clockInAt", status, source)
           VALUES ($1, $2, '2026-06-02 08:00:00', 'active', 'mobile') RETURNING uuid::text"#,
    )
    .bind(c.staff2.id)
    .bind(c.id)
    .fetch_one(app.db())
    .await
    .unwrap();
    app.exec(&format!(
        r#"INSERT INTO "geofenceAuditLog" ("geofenceId", "userId", event, lat, lng, accuracy, timestamp)
           VALUES ({gf_id}, {u}, 'enter', -6.2, 106.8, 8.5, '2026-06-01 09:31:00'),
                  ({gf_id}, {u}, 'exit', -6.21, 106.81, 7, '2026-06-01 12:00:00'),
                  ({gf_id}, {u}, 'enter', -6.2, 106.8, 7, '2026-06-02 12:00:00');"#,
        u = c.staff.id
    ))
    .await;
    Fx {
        gf_uuid,
        late,
        active,
    }
}

const RANGE: &str = "startDate=2026-06-01&endDate=2026-06-02";

#[tokio::test]
async fn admin_attendance_list_and_detail() {
    let _g = SERIAL.lock().await;
    let Some((app, _em)) = app("adm_list").await else {
        return;
    };
    let c = app.company().await;
    let fx = seed(&app, &c).await;
    let admin = &c.admin.token;

    app.get(&format!("/v1/admin/clock/attendance?{RANGE}"))
        .send()
        .await
        .assert_status(401);
    app.get(&format!("/v1/admin/clock/attendance?{RANGE}"))
        .token(&c.staff.token)
        .send()
        .await
        .assert_status(401);
    let r = app
        .get("/v1/admin/clock/attendance?limit=500")
        .token(admin)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        json!([
            "startDate should not be empty",
            "startDate must be a valid ISO 8601 date string",
            "endDate should not be empty",
            "endDate must be a valid ISO 8601 date string",
            "limit must not be greater than 100"
        ])
    );
    let r = app
        .get(&format!(
            "/v1/admin/clock/attendance?{RANGE}&geofenceUuid=00000000-0000-4000-8000-000000000000"
        ))
        .token(admin)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(r.json["message"], "Geofence not found");

    let r = app
        .get(&format!("/v1/admin/clock/attendance?{RANGE}"))
        .token(&c.manager.token)
        .send()
        .await;
    r.assert_ok().assert_no_sql_error();
    let keys: Vec<&str> = r
        .json
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, ["startDate", "endDate", "summary", "data", "meta"]);
    assert_eq!(
        r.json["meta"],
        json!({ "total": 2, "page": 1, "limit": 20, "totalPages": 1 })
    );
    // Telat: Senin 09:30 > 09:00; Selasa 08:00 di geofence "all" berjadwal → tidak telat.
    assert_eq!(r.json["summary"]["totalPresent"], 2);
    assert_eq!(r.json["summary"]["totalLate"], 1);
    assert_eq!(r.json["summary"]["averageWorkSeconds"], 14400);
    assert!(r.json["summary"]["totalAbsent"].as_u64().is_some());
    let first = &r.json["data"][0];
    assert_eq!(first["shiftId"], fx.active.as_str());
    assert_eq!(first["status"], "active");
    assert_eq!(first["clockOut"], Value::Null);
    let second = &r.json["data"][1];
    assert_eq!(second["status"], "closed");
    assert_eq!(second["clockIn"]["accuracyMeters"], 5.5);
    assert_eq!(second["clockIn"]["lat"], -6.2);
    assert_eq!(
        second["clockIn"]["geofence"],
        json!({ "id": fx.gf_uuid, "name": "Kantor Pusat", "radiusMeters": 100 })
    );
    assert_eq!(second["clockIn"]["hasSelfie"], true);
    assert_eq!(second["user"]["id"], c.staff.uuid.as_str());
    let in_keys: Vec<&str> = second["clockIn"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        in_keys,
        [
            "at",
            "lat",
            "lng",
            "accuracyMeters",
            "isMock",
            "verificationStatus",
            "geofence",
            "hasSelfie"
        ]
    );

    // Filter user & geofence (ringkasan tetap company-wide).
    let r = app
        .get(&format!(
            "/v1/admin/clock/attendance?{RANGE}&userUuid={}&geofenceUuid={}&limit=1",
            c.staff.uuid, fx.gf_uuid
        ))
        .token(admin)
        .send()
        .await;
    r.assert_ok();
    assert_eq!(r.json["meta"]["total"], 1);
    assert_eq!(r.json["summary"]["totalPresent"], 2);

    // Detail: event geofence dalam jendela shift (bukan hari berikutnya).
    let r = app
        .get(&format!("/v1/admin/clock/attendance/{}", fx.late))
        .token(admin)
        .send()
        .await;
    r.assert_ok().assert_no_sql_error();
    let ev = r.data()["geofenceEvents"].as_array().unwrap();
    assert_eq!(ev.len(), 2, "{ev:?}");
    assert_eq!(
        ev[0],
        json!({
            "source": "geofence",
            "event": "enter",
            "geofence": { "id": fx.gf_uuid, "name": "Kantor Pusat" },
            "lat": -6.2,
            "lng": 106.8,
            "accuracy": 8.5,
            "timestamp": 1_780_306_260_000_i64
        })
    );
    assert_eq!(r.data()["note"], "catatan");
    let r = app
        .get("/v1/admin/clock/attendance/00000000-0000-4000-8000-000000000000")
        .token(admin)
        .send()
        .await;
    r.assert_status(404);
    assert_eq!(r.json["message"], "Shift not found");
    // Shift aktif tanpa clock-out: jendela s.d. clockIn + 24 j.
    let r = app
        .get(&format!("/v1/admin/clock/attendance/{}", fx.active))
        .token(admin)
        .send()
        .await;
    r.assert_ok();
    assert_eq!(r.data()["geofenceEvents"], json!([]));

    // Mobile: riwayat sendiri staff.
    let r = app
        .get(&format!("/mobile/clock/attendance?{RANGE}"))
        .token(&c.staff.token)
        .send()
        .await;
    r.assert_ok().assert_no_sql_error();
    assert_eq!(r.json["meta"]["total"], 1);
    assert_eq!(r.json["data"][0]["status"], "auto_closed");
    assert_eq!(
        r.json["summary"],
        json!({ "totalPresent": 1, "totalLate": 1, "averageWorkSeconds": 14400 })
    );
    reset(&app, c.id).await;
}

#[tokio::test]
async fn admin_selfies_and_exports() {
    let _g = SERIAL.lock().await;
    let Some((app, em)) = app("adm_selfie").await else {
        return;
    };
    let c = app.company().await;
    let fx = seed(&app, &c).await;
    let admin = &c.admin.token;
    em.put("selfies/x/in.jpg", "image/jpeg", b"IN");
    em.put("selfies/x/out.jpg", "image/jpeg", b"OUT");

    // Daftar selfie: URL bertanda tangan + progres jadwal.
    let r = app
        .get(&format!("/v1/admin/clock/attendance/selfie?{RANGE}"))
        .token(admin)
        .send()
        .await;
    r.assert_ok().assert_no_sql_error();
    let late = &r.json["data"][1];
    assert_eq!(late["shiftId"], fx.late.as_str());
    assert_eq!(
        late["geofence"],
        json!({ "id": fx.gf_uuid, "name": "Kantor Pusat", "address": "Jl. Uji 1" })
    );
    assert_eq!(
        late["schedule"],
        json!({ "startTime": "09:00 AM", "endTime": "05:00 PM" })
    );
    // 09:00→13:30 dari 8 jam = 56.25% → 56.3.
    assert_eq!(late["progressPercent"], 56.3);
    let url = late["clockIn"]["selfieUrl"].as_str().unwrap();
    assert!(
        url.contains("selfies/x/in.jpg") && url.contains("X-Goog-Signature="),
        "{url}"
    );
    assert!(late["clockOut"]["selfieUrl"].as_str().is_some());
    let active = &r.json["data"][0];
    assert_eq!(active["clockIn"]["selfieUrl"], Value::Null);
    assert_eq!(active["progressPercent"], Value::Null);
    assert_eq!(active["clockOut"], Value::Null);

    // Stream & URL selfie admin.
    let base = format!("/v1/admin/clock/shifts/{}/selfie", fx.late);
    let r = app.get(&base).token(admin).send().await;
    r.assert_status(200);
    assert_eq!(r.bytes, b"IN");
    assert_eq!(r.headers["cache-control"], "no-store");
    let r = app
        .get(&format!("{base}?type=clockOut"))
        .token(admin)
        .send()
        .await;
    assert_eq!(r.bytes, b"OUT");
    let r = app.get(&format!("{base}?type=x")).token(admin).send().await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        "type must be one of: in, out, clockIn, clockOut"
    );
    let r = app
        .get("/v1/admin/clock/shifts/nope/selfie")
        .token(admin)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(r.json["message"], "Validation failed (uuid is expected)");
    let r = app
        .get(&format!("/v1/admin/clock/shifts/{}/selfie", fx.active))
        .token(admin)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(r.json["message"], "No selfie available for this punch");
    let r = app
        .get(&format!("{base}/url?type=out"))
        .token(admin)
        .send()
        .await;
    r.assert_ok();
    assert_eq!(r.headers["cache-control"], "private, max-age=300");
    let keys: Vec<&str> = r
        .json
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, ["url", "expiresAt"]);
    assert!(
        r.json["url"]
            .as_str()
            .unwrap()
            .contains("selfies/x/out.jpg")
    );
    let other = app.other_company_admin().await;
    let r = app
        .get(&format!("{base}/url"))
        .token(&other.admin.token)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(r.json["message"], "Shift not found");

    // Ekspor Excel.
    let r = app
        .get(&format!("/v1/admin/clock/attendance/export/excel?{RANGE}"))
        .token(admin)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(
        r.headers["content-type"],
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
    );
    assert_eq!(
        r.headers["content-disposition"],
        "attachment; filename=\"attendance-2026-06-01-to-2026-06-02.xlsx\""
    );
    assert_eq!(
        r.headers["content-length"],
        r.bytes.len().to_string().as_str()
    );
    assert!(r.bytes.starts_with(b"PK"));
    // Ekspor PDF.
    let r = app
        .get(&format!("/v1/admin/clock/attendance/export/pdf?{RANGE}"))
        .token(admin)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.headers["content-type"], "application/pdf");
    assert_eq!(
        r.headers["content-disposition"],
        "attachment; filename=\"attendance-2026-06-01-to-2026-06-02.pdf\""
    );
    let text = String::from_utf8_lossy(&r.bytes);
    assert!(text.starts_with("%PDF"));
    assert!(text.contains("(2026-06-01 09:30 UTC)"), "jam clock-in");
    assert!(text.contains("(30m)"), "telat 30 menit");
    assert!(text.contains("(catatan)"));
    let r = app
        .get("/v1/admin/clock/attendance/export/pdf?startDate=2026-06-03&endDate=2026-06-01")
        .token(admin)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(r.json["message"], "endDate must be on or after startDate");
    reset(&app, c.id).await;
}
