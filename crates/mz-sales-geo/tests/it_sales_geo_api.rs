//! Uji integrasi 20 route mz-sales-geo (geofence, negative zone, visit master) lewat HTTP terhadap
//! Postgres nyata + helper `pub` (geo, plan, visit master).
//! Jalankan: `MZ_TEST_DATABASE_URL=... MZ_TEST_REQUIRE_DB=1 cargo test -p mz-sales-geo --test it_sales_geo_api`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::time::Duration;

use axum::Router;
use mz_core::AppState;
use mz_testkit::{Company, TestApp, TestResponse};
use serde_json::{Value, json};
use tokio::sync::{Mutex, MutexGuard};

static SERIAL: Mutex<()> = Mutex::const_new(());

fn build(state: &AppState) -> Router {
    mz_sales_geo::router(state)
        .with_state(state.clone())
        .layer(mz_audit::layer())
}

struct Ctx {
    app: TestApp,
    fx: Company,
    _guard: MutexGuard<'static, ()>,
}

async fn setup() -> Option<Ctx> {
    let guard = SERIAL.lock().await;
    let app = TestApp::new(build).await?;
    let fx = app.company().await;
    app.exec(&format!(
        r#"UPDATE company SET "moduleSalesTrackerEnabled" = true WHERE id = {cid};
           INSERT INTO "companyPlanOverride" ("companyId", "totalGeofence") VALUES ({cid}, 0)
             ON CONFLICT ("companyId") DO UPDATE SET "totalGeofence" = 0;
           -- Izin bawaan tiga fitur (di produksi diisi sinkronisasi registry saat boot).
           INSERT INTO "rolePermission" ("accessLevelId", "featureKey", action)
             SELECT al.id, k.f, a.a::"PermissionAction" FROM "accessLevel" al
             CROSS JOIN (VALUES ('geofences'), ('negative-zones'), ('visit-masters')) AS k(f)
             CROSS JOIN (VALUES ('READ'), ('WRITE'), ('DELETE')) AS a(a)
             WHERE al."isSystem" ON CONFLICT DO NOTHING;
           DELETE FROM geofence WHERE "companyId" = {cid};
           DELETE FROM "negativeZone" WHERE "companyId" = {cid};"#,
        cid = fx.id
    ))
    .await;
    Some(Ctx {
        app,
        fx,
        _guard: guard,
    })
}

fn assert_key_order(text: &str, keys: &[&str]) {
    let mut from = 0;
    for k in keys {
        let needle = format!("\"{k}\":");
        let pos = text[from..]
            .find(&needle)
            .unwrap_or_else(|| panic!("kunci `{k}` tidak ada sesudah {from} di {text}"));
        from += pos + needle.len();
    }
}

fn messages(r: &TestResponse) -> Vec<String> {
    r.json["message"]
        .as_array()
        .unwrap_or_else(|| panic!("message bukan array: {}", r.text()))
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect()
}

fn code_of(r: &TestResponse) -> String {
    r.json["code"].as_str().unwrap_or_default().to_owned()
}

async fn audit_actions(c: &Ctx, entity: &str, id: i32, at_least: usize) -> Vec<String> {
    for _ in 0..50 {
        let rows: Vec<String> = sqlx::query_scalar(
            r#"SELECT action::text FROM "auditLog"
                WHERE "entityType"::text = $1 AND "entityId" = $2 ORDER BY id"#,
        )
        .bind(entity)
        .bind(id)
        .fetch_all(c.app.db())
        .await
        .unwrap();
        if rows.len() >= at_least {
            return rows;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("baris audit {entity}#{id} < {at_least}");
}

async fn new_geofence(c: &Ctx, body: Value) -> Value {
    let r = c
        .app
        .post("/v1/admin/geofences")
        .token(&c.fx.admin.token)
        .json(&body)
        .send()
        .await;
    r.assert_status(201);
    r.data().clone()
}

fn gf_body(name: &str, lat: f64, lng: f64, radius: i32) -> Value {
    json!({ "name": name, "lat": lat, "lng": lng, "radiusMeters": radius })
}

// ───────────────────────── geofence admin ─────────────────────────

#[tokio::test]
async fn geofence_admin_crud_audit_and_validation() {
    let Some(c) = setup().await else { return };
    let app = &c.app;

    // 401 / 403 / validasi
    app.get("/v1/admin/geofences")
        .send()
        .await
        .assert_status(401);
    // `RolesGuard` Node menolak role yang salah dengan 401 "Access denied: insufficient permissions".
    let r = app
        .post("/v1/admin/geofences")
        .token(&c.fx.manager.token)
        .json(&gf_body("x", 0.0, 0.0, 100))
        .send()
        .await;
    r.assert_status(401);
    assert_eq!(r.json["message"], "Access denied: insufficient permissions");
    app.get("/v1/admin/geofences")
        .token(&c.fx.staff.token)
        .send()
        .await
        .assert_status(401);
    let r = app
        .post("/v1/admin/geofences")
        .token(&c.fx.admin.token)
        .json(&json!({}))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(messages(&r)[0], "name must be a string");
    let r = app
        .post("/v1/admin/geofences")
        .token(&c.fx.admin.token)
        .json(&json!({"name":"x","lat":1,"lng":1,"radiusMeters":10,
            "weeklySchedule":{"monday":{"enabled":true,"startTime":"9","endTime":"09:00 AM"}},
            "breakTime":{"hours":30,"minutes":0}}))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        messages(&r),
        vec![
            "radiusMeters must not be less than 50",
            "weeklySchedule.monday.startTime must be in format \"HH:MM AM/PM\"",
            "breakTime.hours must not be greater than 23",
        ]
    );

    // Buat minimal: semua kolom skalar, urutan skema.
    let r = app
        .post("/v1/admin/geofences")
        .token(&c.fx.admin.token)
        .json(&gf_body("Kantor", -6.2088, 106.8456, 100))
        .send()
        .await;
    r.assert_status(201);
    assert_key_order(
        &r.text(),
        &[
            "id",
            "uuid",
            "companyId",
            "name",
            "address",
            "lat",
            "lng",
            "radiusMeters",
            "enforced",
            "applicableTo",
            "applicableIds",
            "isActive",
            "createdById",
            "timezone",
            "weeklySchedule",
            "breakTimeHours",
            "breakTimeMinutes",
            "createdAt",
            "updatedAt",
        ],
    );
    let gf = r.data().clone();
    let id = gf["id"].as_i64().unwrap() as i32;
    let uuid = gf["uuid"].as_str().unwrap().to_owned();
    assert_eq!(gf["companyId"], c.fx.id);
    assert_eq!(gf["createdById"], c.fx.admin.id);
    assert_eq!(gf["enforced"], false);
    assert_eq!(gf["applicableTo"], "all");
    assert_eq!(gf["applicableIds"], json!([]));
    assert_eq!(gf["isActive"], true);
    assert_eq!(gf["address"], Value::Null);
    assert_eq!(gf["weeklySchedule"], Value::Null);
    assert_eq!(gf["breakTimeHours"], 0);
    assert!(gf["createdAt"].as_str().unwrap().ends_with('Z'));

    // Buat penuh.
    let full = new_geofence(
        &c,
        json!({
            "name": "Gudang", "address": "Jl. A", "lat": 5, "lng": 6, "radiusMeters": 150,
            "enforced": true, "applicableTo": "user_ids", "applicableIds": [c.fx.staff.id],
            "timezone": "Asia/Jakarta", "isActive": true, "breakTime": {"hours": 1, "minutes": 30},
            "weeklySchedule": {"monday": {"enabled": true, "startTime": "09:00 AM", "endTime": "06:00 PM"}},
        }),
    )
    .await;
    assert_eq!(full["lat"], 5);
    assert_eq!(full["enforced"], true);
    assert_eq!(full["breakTimeMinutes"], 30);
    assert_eq!(full["weeklySchedule"]["monday"]["startTime"], "09:00 AM");
    assert_eq!(full["applicableIds"], json!([c.fx.staff.id]));

    // Daftar: manager boleh (override @Roles), urut id.
    let r = app
        .get("/v1/admin/geofences")
        .token(&c.fx.manager.token)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    let ids: Vec<i64> = r
        .data()
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids.len(), 2);
    assert!(ids[0] < ids[1]);

    // Patch: nama + ukuran; kolom lain tetap; updatedAt berubah.
    let r = app
        .patch(&format!("/v1/admin/geofences/{uuid}"))
        .token(&c.fx.admin.token)
        .json(&json!({"name": "Kantor 2", "radiusMeters": 120, "address": null}))
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.data()["name"], "Kantor 2");
    assert_eq!(r.data()["radiusMeters"], 120);
    assert_eq!(r.data()["lat"], -6.2088);
    assert!(r.data()["updatedAt"].is_string());
    // Validasi patch + null di kolom non-null (Prisma melempar → 500).
    let r = app
        .patch(&format!("/v1/admin/geofences/{uuid}"))
        .token(&c.fx.admin.token)
        .json(&json!({"lat": 91}))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(messages(&r), vec!["lat must not be greater than 90"]);
    app.patch(&format!("/v1/admin/geofences/{uuid}"))
        .token(&c.fx.admin.token)
        .json(&json!({"name": null}))
        .send()
        .await
        .assert_status(500);
    // `weeklySchedule: null` (kolom Json?): Node/Prisma menyimpan JSON null, 200 (paritas sales-a gf4).
    let full_uuid = full["uuid"].as_str().unwrap().to_owned();
    let r = app
        .patch(&format!("/v1/admin/geofences/{full_uuid}"))
        .token(&c.fx.admin.token)
        .json(&json!({"weeklySchedule": null}))
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.data()["weeklySchedule"], Value::Null);
    let stored: Option<String> =
        sqlx::query_scalar(r#"SELECT "weeklySchedule"::text FROM geofence WHERE uuid::text = $1"#)
            .bind(&full_uuid)
            .fetch_one(c.app.db())
            .await
            .unwrap();
    assert_eq!(stored.as_deref(), Some("null"));
    // 404 + uuid rusak (P2023 → 500) + lintas company.
    app.patch("/v1/admin/geofences/00000000-0000-4000-8000-000000000000")
        .token(&c.fx.admin.token)
        .json(&json!({}))
        .send()
        .await
        .assert_status(404);
    app.patch("/v1/admin/geofences/not-a-uuid")
        .token(&c.fx.admin.token)
        .json(&json!({}))
        .send()
        .await
        .assert_status(500);
    let other = app.other_company_admin().await;
    app.patch(&format!("/v1/admin/geofences/{uuid}"))
        .token(&other.admin.token)
        .json(&json!({"name": "hack"}))
        .send()
        .await
        .assert_status(404);

    // Hapus = soft delete.
    let r = app
        .delete(&format!("/v1/admin/geofences/{uuid}"))
        .token(&c.fx.admin.token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json, json!({"data": {"deleted": true}}));
    let active: bool = app
        .scalar(&format!(
            "SELECT \"isActive\" FROM geofence WHERE id = {id}"
        ))
        .await;
    assert!(!active);
    app.delete(&format!("/v1/admin/geofences/{uuid}"))
        .token(&c.fx.manager.token)
        .send()
        .await
        .assert_status(401);

    // Audit: create + update + update (soft delete).
    let acts = audit_actions(&c, "geofence", id, 3).await;
    assert_eq!(acts[0], "create");
    assert_eq!(acts.len(), 3, "{acts:?}");
}

#[tokio::test]
async fn geofence_plan_limit_returns_402_and_frees_on_delete() {
    let Some(c) = setup().await else { return };
    let app = &c.app;
    app.exec(&format!(
        r#"UPDATE "companyPlanOverride" SET "totalGeofence" = 2 WHERE "companyId" = {}"#,
        c.fx.id
    ))
    .await;
    let a = new_geofence(&c, gf_body("A", 1.0, 1.0, 100)).await;
    new_geofence(&c, gf_body("B", 1.0, 1.0, 100)).await;
    let r = app
        .post("/v1/admin/geofences")
        .token(&c.fx.admin.token)
        .json(&gf_body("C", 1.0, 1.0, 100))
        .send()
        .await;
    r.assert_status(402);
    assert_eq!(
        r.json,
        json!({
            "code": "GEOFENCE_LIMIT_REACHED",
            "message": "Your plan allows 2 geofences. Upgrade the plan to add more.",
            "details": { "limit": 2, "used": 2 }
        })
    );
    assert_key_order(&r.text(), &["code", "message", "details", "limit", "used"]);
    // Jatah kembali sesudah DELETE (soft delete).
    app.delete(&format!(
        "/v1/admin/geofences/{}",
        a["uuid"].as_str().unwrap()
    ))
    .token(&c.fx.admin.token)
    .send()
    .await
    .assert_status(200);
    new_geofence(&c, gf_body("C", 1.0, 1.0, 100)).await;
    app.exec(&format!(
        r#"UPDATE "companyPlanOverride" SET "totalGeofence" = 0 WHERE "companyId" = {}"#,
        c.fx.id
    ))
    .await;
}

// ───────────────────────── geofence mobile ─────────────────────────

#[tokio::test]
async fn geofence_mobile_list_nearby_schedule_dashboard() {
    let Some(c) = setup().await else { return };
    let app = &c.app;
    let all = new_geofence(&c, gf_body("Semua", 0.0, 0.0, 100)).await;
    let mine = new_geofence(
        &c,
        json!({"name": "Saya", "lat": 0.0, "lng": 0.01, "radiusMeters": 100, "enforced": true,
               "applicableTo": "user_ids", "applicableIds": [c.fx.staff.id]}),
    )
    .await;
    new_geofence(
        &c,
        json!({"name": "Orang Lain", "lat": 0.0, "lng": 0.02, "radiusMeters": 100,
               "applicableTo": "user_ids", "applicableIds": [c.fx.staff2.id]}),
    )
    .await;
    let off = new_geofence(&c, gf_body("Mati", 0.0, 0.03, 100)).await;
    app.delete(&format!(
        "/v1/admin/geofences/{}",
        off["uuid"].as_str().unwrap()
    ))
    .token(&c.fx.admin.token)
    .send()
    .await
    .assert_status(200);

    // List: aktif & berlaku untuk user.
    let r = app
        .get("/mobile/geofences")
        .token(&c.fx.staff.token)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    let names: Vec<&str> = r
        .data()
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["Semua", "Saya"]);
    assert_key_order(
        &r.text(),
        &["id", "name", "lat", "lng", "radiusMeters", "enforced"],
    );
    assert_eq!(r.data()[0]["id"], all["uuid"]);
    assert_eq!(r.data()[0]["lat"], 0);
    app.get("/mobile/geofences").send().await.assert_status(401);

    // Nearby: urut jarak; batas; validasi.
    let r = app
        .get("/mobile/geofences/nearby?lat=0&lng=0.0099&limit=1")
        .token(&c.fx.staff.token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.data().as_array().unwrap().len(), 1);
    assert_eq!(r.data()[0]["name"], "Saya");
    assert_eq!(r.data()[0]["distanceMeters"], 11);
    assert_key_order(
        &r.text(),
        &[
            "id",
            "name",
            "lat",
            "lng",
            "radiusMeters",
            "enforced",
            "distanceMeters",
        ],
    );
    let r = app
        .get("/mobile/geofences/nearby?lat=0&lng=0")
        .token(&c.fx.staff.token)
        .send()
        .await;
    assert_eq!(r.data()[0]["name"], "Semua");
    assert_eq!(r.data().as_array().unwrap().len(), 2);
    let r = app
        .get("/mobile/geofences/nearby?lat=abc&limit=99")
        .token(&c.fx.staff.token)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        messages(&r),
        vec![
            "lat must be a number conforming to the specified constraints",
            "lng must be a number conforming to the specified constraints",
            "limit must not be greater than 50",
        ]
    );

    // Jadwal.
    let r = app
        .get(&format!(
            "/mobile/geofences/{}/schedule",
            mine["uuid"].as_str().unwrap()
        ))
        .token(&c.fx.staff.token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(
        r.json,
        json!({"data": {"id": mine["uuid"], "name": "Saya", "timezone": null,
                        "weeklySchedule": null, "breakTime": {"hours": 0, "minutes": 0}}})
    );
    app.get(&format!(
        "/mobile/geofences/{}/schedule",
        off["uuid"].as_str().unwrap()
    ))
    .token(&c.fx.staff.token)
    .send()
    .await
    .assert_status(404);
    app.get("/mobile/geofences/xyz/schedule")
        .token(&c.fx.staff.token)
        .send()
        .await
        .assert_status(500);

    // Dashboard (semua geofence company, termasuk nonaktif).
    let r = app
        .get("/mobile/geofences/dashboard")
        .token(&c.fx.staff.token)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    let s = &r.data()["summary"];
    assert_eq!(s["total_zones"], 4);
    assert_eq!(s["active"], 3);
    assert_eq!(s["inactive"], 1);
    let total_emp: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "userCompany" WHERE "companyId" = {}"#,
            c.fx.id
        ))
        .await;
    assert_eq!(s["employees_covered"], total_emp);
    assert_eq!(r.data()["zones"][1]["totalAssigned"], 1);
    assert_eq!(r.data()["zones"][0]["totalAssigned"], total_emp);
    assert_key_order(
        &r.text(),
        &[
            "id",
            "name",
            "address",
            "lat",
            "lng",
            "radiusMeters",
            "totalAssigned",
            "isActive",
        ],
    );
}

#[tokio::test]
async fn geofence_event_log_and_history() {
    let Some(c) = setup().await else { return };
    let app = &c.app;
    let ts = 1_749_430_800_000_i64;
    let ev = |extra: Value| {
        let mut b = json!({"lat": 0.0, "lng": 0.0, "accuracy": 8.5, "timestamp": ts});
        b.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        b
    };

    // Tanpa geofence sama sekali: tidak dicatat, tetap 201.
    let r = app
        .post("/mobile/geofences/event")
        .token(&c.fx.staff.token)
        .json(&ev(json!({})))
        .send()
        .await;
    r.assert_status(201);
    assert_eq!(
        r.json,
        json!({"data": {"logged": false, "reason": "NO_APPLICABLE_GEOFENCE",
                        "geofenceId": null, "event": null, "distanceMeters": null}})
    );

    let near = new_geofence(&c, gf_body("Dekat", 0.0, 0.0, 100)).await;
    let far = new_geofence(&c, gf_body("Jauh", 0.0, 0.5, 100)).await;
    let near_uuid = near["uuid"].as_str().unwrap();
    let far_uuid = far["uuid"].as_str().unwrap();

    // Tanpa geofenceId: terdekat yang memuat titik → enter.
    let r = app
        .post("/mobile/geofences/event")
        .token(&c.fx.staff.token)
        .json(&ev(json!({"geofenceId": null})))
        .send()
        .await;
    r.assert_status(201);
    assert_eq!(
        r.json,
        json!({"data": {"logged": true, "geofenceId": near_uuid, "event": "enter", "distanceMeters": 0}})
    );
    // Dengan geofenceId jauh → exit.
    let r = app
        .post("/mobile/geofences/event")
        .token(&c.fx.staff.token)
        .json(&ev(
            json!({"geofenceId": far_uuid, "timestamp": ts + 60_000}),
        ))
        .send()
        .await;
    r.assert_status(201);
    assert_eq!(r.data()["event"], "exit");
    assert_eq!(r.data()["distanceMeters"], 55_660);
    // 404 + validasi.
    app.post("/mobile/geofences/event")
        .token(&c.fx.staff.token)
        .json(&ev(
            json!({"geofenceId": "00000000-0000-4000-8000-000000000000"}),
        ))
        .send()
        .await
        .assert_status(404);
    let r = app
        .post("/mobile/geofences/event")
        .token(&c.fx.staff.token)
        .json(&json!({"geofenceId": "x", "lat": "1"}))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(messages(&r)[0], "geofenceId must be a UUID");
    assert_eq!(messages(&r).len(), 5);
    app.post("/mobile/geofences/event")
        .token(&c.fx.staff.token)
        .json(&ev(json!({"timestamp": 9.0e15})))
        .send()
        .await
        .assert_status(500);

    // Riwayat: terbaru dulu, filter, halaman.
    let r = app
        .get("/mobile/geofences/event")
        .token(&c.fx.staff.token)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(
        r.json["meta"],
        json!({"total": 2, "page": 1, "limit": 20, "totalPages": 1})
    );
    assert_eq!(r.json["data"][0]["event"], "exit");
    assert_eq!(r.json["data"][0]["geofenceName"], "Jauh");
    assert_eq!(r.json["data"][0]["timestamp"], "2025-06-09T01:01:00.000Z");
    assert_eq!(r.json["data"][0]["accuracy"], 8.5);
    assert_key_order(
        &r.text(),
        &[
            "id",
            "geofenceId",
            "geofenceName",
            "event",
            "lat",
            "lng",
            "accuracy",
            "timestamp",
        ],
    );
    let r = app
        .get(&format!(
            "/mobile/geofences/event?geofenceId={near_uuid}&event=enter&startDate=2025-06-09&endDate=2025-06-09&limit=1"
        ))
        .token(&c.fx.staff.token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json["meta"]["total"], 1);
    assert_eq!(r.json["data"].as_array().unwrap().len(), 1);
    // Log milik user lain tidak terlihat.
    let r = app
        .get("/mobile/geofences/event")
        .token(&c.fx.staff2.token)
        .send()
        .await;
    assert_eq!(r.json["meta"]["total"], 0);
    // 400 / 404.
    let r = app
        .get("/mobile/geofences/event?startDate=2025-06-10&endDate=2025-06-09")
        .token(&c.fx.staff.token)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(r.json["message"], "endDate must be on or after startDate");
    app.get("/mobile/geofences/event?geofenceId=00000000-0000-4000-8000-000000000000")
        .token(&c.fx.staff.token)
        .send()
        .await
        .assert_status(404);
    let r = app
        .get("/mobile/geofences/event?page=0&limit=1000&event=x")
        .token(&c.fx.staff.token)
        .send()
        .await;
    assert_eq!(
        messages(&r),
        vec![
            "event must be one of the following values: enter, exit",
            "page must not be less than 1",
            "limit must not be greater than 100",
        ]
    );
    // startDate berbentuk ISO dengan waktu → Invalid Date di Node → 500.
    app.get("/mobile/geofences/event?startDate=2025-06-09T10:00:00Z")
        .token(&c.fx.staff.token)
        .send()
        .await
        .assert_status(500);
}

#[tokio::test]
async fn geo_helpers_read_database() {
    let Some(c) = setup().await else { return };
    new_geofence(&c, gf_body("G1", 0.0, 0.0, 100)).await;
    new_geofence(
        &c,
        json!({"name": "G2", "lat": 0.0, "lng": 0.0, "radiusMeters": 100,
               "applicableTo": "user_ids", "applicableIds": [c.fx.staff2.id]}),
    )
    .await;
    let db = c.app.db();
    let gs = mz_sales_geo::geo::active_geofences_for(db, c.fx.id, c.fx.staff.id)
        .await
        .unwrap();
    assert_eq!(gs.len(), 1);
    assert_eq!(gs[0].name, "G1");
    let (g, d) = mz_sales_geo::geo::pick_geofence(&gs, 0.0, 0.0).unwrap();
    assert_eq!((g.name.as_str(), d), ("G1", 0));

    // negative zone via API lalu helper.
    let r = c
        .app
        .post("/v1/admin/negative-zones")
        .token(&c.fx.manager.token)
        .json(&json!({"name": "Parkir", "lat": 0.0, "lng": 0.0, "radiusMeters": 50}))
        .send()
        .await;
    r.assert_status(201);
    let hit = mz_sales_geo::geo::find_containing_negative_zone(db, c.fx.id, 0.0, 0.0)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(hit.zone.name, "Parkir");
    assert_eq!(hit.distance_meters, 0);
    assert!(
        mz_sales_geo::geo::find_containing_negative_zone(db, c.fx.id, 10.0, 10.0)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn plan_limits_resolve_from_override() {
    let Some(c) = setup().await else { return };
    let mut conn = c.app.db().acquire().await.unwrap();
    c.app
        .exec(&format!(
            r#"UPDATE "companyPlanOverride" SET "totalGeofence" = 7, "visitHistoryDays" = 30 WHERE "companyId" = {}"#,
            c.fx.id
        ))
        .await;
    let l = mz_sales_geo::plan::resolve_plan_limits(&mut conn, c.fx.id)
        .await
        .unwrap()
        .expect("paket berjalan");
    assert_eq!(l.total_geofence, 7);
    assert_eq!(l.visit_history_days, 30);
    drop(conn);
    assert_eq!(
        mz_sales_geo::plan::resolve_visit_history_days(c.app.db(), c.fx.id)
            .await
            .unwrap(),
        30
    );
    c.app
        .exec(&format!(
            r#"UPDATE "companyPlanOverride" SET "totalGeofence" = 0, "visitHistoryDays" = NULL WHERE "companyId" = {}"#,
            c.fx.id
        ))
        .await;
}

// ───────────────────────── negative zones ─────────────────────────

#[tokio::test]
async fn negative_zone_admin_and_mobile() {
    let Some(c) = setup().await else { return };
    let app = &c.app;
    app.get("/v1/admin/negative-zones")
        .token(&c.fx.staff.token)
        .send()
        .await
        .assert_status(401);
    app.get("/v1/admin/negative-zones")
        .send()
        .await
        .assert_status(401);
    let r = app
        .post("/v1/admin/negative-zones")
        .token(&c.fx.admin.token)
        .json(&json!({"name": "", "lat": 100, "radiusMeters": 5}))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        messages(&r),
        vec![
            "name must be longer than or equal to 1 characters",
            "lat must not be greater than 90",
            "lng must not be greater than 180",
            "lng must not be less than -180",
            "lng must be a number conforming to the specified constraints",
            "radiusMeters must not be less than 10",
        ]
    );

    let r = app
        .post("/v1/admin/negative-zones")
        .token(&c.fx.manager.token)
        .json(&json!({"name": "Parkir", "lat": -6.2, "lng": 106.8, "radiusMeters": 100}))
        .send()
        .await;
    r.assert_status(201).assert_no_sql_error();
    assert_key_order(
        &r.text(),
        &[
            "id",
            "uuid",
            "companyId",
            "name",
            "lat",
            "lng",
            "radiusMeters",
            "isActive",
            "createdById",
            "createdAt",
            "updatedAt",
        ],
    );
    let z = r.data().clone();
    let (id, uuid) = (
        z["id"].as_i64().unwrap() as i32,
        z["uuid"].as_str().unwrap().to_owned(),
    );
    assert_eq!(z["createdById"], c.fx.manager.id);
    assert_eq!(z["isActive"], true);
    let off = app
        .post("/v1/admin/negative-zones")
        .token(&c.fx.admin.token)
        .json(
            &json!({"name": "Kantin", "lat": 1, "lng": 2, "radiusMeters": 5000, "isActive": false}),
        )
        .send()
        .await;
    off.assert_status(201);
    assert_eq!(off.data()["isActive"], false);

    // Admin melihat semua; mobile hanya aktif.
    let r = app
        .get("/v1/admin/negative-zones")
        .token(&c.fx.manager.token)
        .send()
        .await;
    assert_eq!(r.data().as_array().unwrap().len(), 2);
    let r = app
        .get("/mobile/negative-zones")
        .token(&c.fx.staff.token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(
        r.json,
        json!({"data": [{"id": uuid, "name": "Parkir", "lat": -6.2, "lng": 106.8, "radiusMeters": 100}]})
    );
    app.get("/mobile/negative-zones")
        .send()
        .await
        .assert_status(401);

    // Patch / 404 / null / lintas company.
    let r = app
        .patch(&format!("/v1/admin/negative-zones/{uuid}"))
        .token(&c.fx.admin.token)
        .json(&json!({"name": "Parkir 2", "radiusMeters": 120}))
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.data()["name"], "Parkir 2");
    assert_eq!(r.data()["radiusMeters"], 120);
    app.patch(&format!("/v1/admin/negative-zones/{uuid}"))
        .token(&c.fx.admin.token)
        .json(&json!({"isActive": null}))
        .send()
        .await
        .assert_status(500);
    app.patch("/v1/admin/negative-zones/00000000-0000-4000-8000-000000000000")
        .token(&c.fx.admin.token)
        .json(&json!({}))
        .send()
        .await
        .assert_status(404);
    app.delete("/v1/admin/negative-zones/zzz")
        .token(&c.fx.admin.token)
        .send()
        .await
        .assert_status(500);
    let other = app.other_company_admin().await;
    app.delete(&format!("/v1/admin/negative-zones/{uuid}"))
        .token(&other.admin.token)
        .send()
        .await
        .assert_status(404);

    let r = app
        .delete(&format!("/v1/admin/negative-zones/{uuid}"))
        .token(&c.fx.manager.token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json, json!({"data": {"deleted": true}}));
    let r = app
        .get("/mobile/negative-zones")
        .token(&c.fx.staff.token)
        .send()
        .await;
    assert_eq!(r.data().as_array().unwrap().len(), 0);

    // Audit: create, update, update (soft delete) — 3+ baris.
    let acts = audit_actions(&c, "negativeZone", id, 3).await;
    assert_eq!(acts[0], "create");
}

// ───────────────────────── visit masters ─────────────────────────

fn vm(kind: &str) -> String {
    format!("/v1/admin/visit-masters/{kind}")
}

#[tokio::test]
async fn visit_master_types_crud_default_rules() {
    let Some(c) = setup().await else { return };
    let app = &c.app;
    let tag = format!("T{}", &uuid::Uuid::new_v4().simple().to_string()[..8]).to_uppercase();
    let code = |s: &str| format!("{tag}_{s}");
    // Kosongkan default lama company supaya hasil deterministik.
    app.exec(&format!(
        r#"UPDATE "visitType" SET "isDefault" = false WHERE "companyId" = {}"#,
        c.fx.id
    ))
    .await;

    // Gerbang: role, modul, kind & uuid.
    app.get(&vm("types"))
        .token(&c.fx.staff.token)
        .send()
        .await
        .assert_status(401);
    app.get(&vm("types")).send().await.assert_status(401);
    let r = app.get(&vm("nope")).token(&c.fx.admin.token).send().await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        "Validation failed (enum string is expected)"
    );
    let r = app
        .get(&format!("{}/not-uuid", vm("types")))
        .token(&c.fx.admin.token)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(r.json["message"], "Validation failed (uuid is expected)");

    // Validasi body.
    let r = app
        .post(&vm("types"))
        .token(&c.fx.admin.token)
        .json(&json!({"code": "bad-code", "name": ""}))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        messages(&r),
        vec![
            "code must be 1-32 characters of uppercase letters, digits, or underscore",
            "name must be longer than or equal to 1 characters",
        ]
    );

    // Buat A sebagai default; kode di-uppercase otomatis.
    let r = app
        .post(&vm("types"))
        .token(&c.fx.manager.token)
        .json(
            &json!({"code": code("a").to_lowercase(), "name": "A", "description": "  desk ",
                      "isDefault": true, "sortOrder": "5", "requiresOrder": "true"}),
        )
        .send()
        .await;
    r.assert_status(201).assert_no_sql_error();
    assert_key_order(
        &r.text(),
        &[
            "id",
            "code",
            "name",
            "description",
            "sortOrder",
            "isActive",
            "requiresOrder",
            "requiresPhoto",
            "requiresGps",
            "isDefault",
            "createdAt",
            "updatedAt",
        ],
    );
    let a = r.data().clone();
    assert_eq!(a["code"], code("A"));
    assert_eq!(a["description"], "desk");
    assert_eq!(a["sortOrder"], 5);
    assert_eq!(a["requiresOrder"], true);
    assert_eq!(a["requiresGps"], true);
    assert_eq!(a["isDefault"], true);
    // `updatedAt` memakai default DB `NOW()` saat INSERT (sama dengan Prisma), jadi terisi.
    assert!(a["createdAt"].is_i64() && a["updatedAt"].is_i64());
    assert!(a.get("intervalDays").is_none());
    let a_id = a["id"].as_str().unwrap().to_owned();

    // Kode kembar → 409; intervalDays tidak diterima oleh types → 422.
    let r = app
        .post(&vm("types"))
        .token(&c.fx.admin.token)
        .json(&json!({"code": code("A"), "name": "dup"}))
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(code_of(&r), "VISIT_MASTER_CODE_TAKEN");
    assert_eq!(
        r.json["message"],
        format!(
            "Visit type code {} is already used in this company",
            code("A")
        )
    );
    let r = app
        .post(&vm("types"))
        .token(&c.fx.admin.token)
        .json(&json!({"code": code("X"), "name": "x", "intervalDays": 3}))
        .send()
        .await;
    r.assert_status(422);
    assert_eq!(code_of(&r), "VISIT_MASTER_INVALID_FIELD");
    assert_eq!(
        r.json["message"],
        "intervalDays is not accepted for Visit type"
    );
    // isDefault pada baris nonaktif → 422.
    let r = app
        .post(&vm("types"))
        .token(&c.fx.admin.token)
        .json(&json!({"code": code("X"), "name": "x", "isDefault": true, "isActive": false}))
        .send()
        .await;
    r.assert_status(422);
    assert_eq!(code_of(&r), "VISIT_TYPE_DEFAULT_ARCHIVED");

    // B menjadi default → A kehilangan default (satu transaksi).
    let r = app
        .post(&vm("types"))
        .token(&c.fx.admin.token)
        .json(&json!({"code": code("B"), "name": "B", "isDefault": true}))
        .send()
        .await;
    r.assert_status(201);
    let b_id = r.data()["id"].as_str().unwrap().to_owned();
    let r = app
        .get(&format!("{}/{a_id}", vm("types")))
        .token(&c.fx.admin.token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.data()["isDefault"], false);

    // Daftar: pencarian, filter isDefault, halaman.
    let r = app
        .get(&format!(
            "{}?search={}&isDefault=true",
            vm("types"),
            tag.to_lowercase()
        ))
        .token(&c.fx.admin.token)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(r.json["data"].as_array().unwrap().len(), 1);
    assert_eq!(r.json["data"][0]["id"], b_id);
    assert_eq!(
        r.json["meta"],
        json!({"total": 1, "page": 1, "limit": 20, "totalPages": 1})
    );
    let r = app
        .get(&format!("{}?search={tag}&limit=1&page=2", vm("types")))
        .token(&c.fx.admin.token)
        .send()
        .await;
    assert_eq!(
        r.json["meta"],
        json!({"total": 2, "page": 2, "limit": 1, "totalPages": 2})
    );
    // sortOrder 5 (A) < 0? B sortOrder 0 → B dulu, A di halaman 2.
    assert_eq!(r.json["data"][0]["id"], a_id);

    // Update: ubah default balik ke A; arsip default membersihkan isDefault.
    let r = app
        .patch(&format!("{}/{a_id}", vm("types")))
        .token(&c.fx.admin.token)
        .json(&json!({"isDefault": true}))
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.data()["isDefault"], true);
    assert!(r.data()["updatedAt"].is_i64());
    let r = app
        .patch(&format!("{}/{a_id}", vm("types")))
        .token(&c.fx.admin.token)
        .json(&json!({"isDefault": true, "isActive": false}))
        .send()
        .await;
    r.assert_status(422);
    assert_eq!(code_of(&r), "VISIT_TYPE_DEFAULT_ARCHIVED");
    let r = app
        .patch(&format!("{}/{a_id}", vm("types")))
        .token(&c.fx.admin.token)
        .json(&json!({"isActive": false}))
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.data()["isActive"], false);
    assert_eq!(r.data()["isDefault"], false);
    // Kode kembar saat update → 409; 404 kode khusus.
    let r = app
        .patch(&format!("{}/{a_id}", vm("types")))
        .token(&c.fx.admin.token)
        .json(&json!({"code": code("B")}))
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(code_of(&r), "VISIT_MASTER_CODE_TAKEN");
    let r = app
        .get(&format!(
            "{}/00000000-0000-4000-8000-000000000000",
            vm("types")
        ))
        .token(&c.fx.admin.token)
        .send()
        .await;
    r.assert_status(404);
    assert_eq!(code_of(&r), "VISIT_MASTER_NOT_FOUND");
    assert_eq!(r.json["message"], "Visit type not found");

    // Arsip lewat DELETE (default dibersihkan).
    let r = app
        .delete(&format!("{}/{b_id}", vm("types")))
        .token(&c.fx.admin.token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json, json!({"data": {"id": b_id, "archived": true}}));
    let r = app
        .get(&format!("{}/{b_id}", vm("types")))
        .token(&c.fx.admin.token)
        .send()
        .await;
    assert_eq!(r.data()["isActive"], false);
    assert_eq!(r.data()["isDefault"], false);

    // Modul SALES_TRACKER mati → 403.
    app.exec(&format!(
        r#"UPDATE company SET "moduleSalesTrackerEnabled" = false WHERE id = {}"#,
        c.fx.id
    ))
    .await;
    mz_core::feature_modules::invalidate_company(app.state(), c.fx.id);
    let r = app.get(&vm("types")).token(&c.fx.admin.token).send().await;
    r.assert_status(403);
    app.exec(&format!(
        r#"UPDATE company SET "moduleSalesTrackerEnabled" = true WHERE id = {}"#,
        c.fx.id
    ))
    .await;
    mz_core::feature_modules::invalidate_company(app.state(), c.fx.id);
}

#[tokio::test]
async fn visit_master_frequencies_and_consumer_helpers() {
    let Some(c) = setup().await else { return };
    let app = &c.app;
    let tag = format!("F{}", &uuid::Uuid::new_v4().simple().to_string()[..8]).to_uppercase();

    // intervalDays wajib dan hanya untuk frequencies.
    let r = app
        .post(&vm("frequencies"))
        .token(&c.fx.admin.token)
        .json(&json!({"code": format!("{tag}_W"), "name": "W"}))
        .send()
        .await;
    r.assert_status(422);
    assert_eq!(code_of(&r), "VISIT_MASTER_FIELD_REQUIRED");
    assert_eq!(
        r.json["message"],
        "intervalDays is required to create Visit frequency"
    );
    let r = app
        .post(&vm("frequencies"))
        .token(&c.fx.admin.token)
        .json(
            &json!({"code": format!("{tag}_W"), "name": "W", "isDefault": true, "intervalDays": 7}),
        )
        .send()
        .await;
    r.assert_status(422);
    assert_eq!(
        r.json["message"],
        "isDefault is not accepted for Visit frequency"
    );
    let r = app
        .post(&vm("frequencies"))
        .token(&c.fx.admin.token)
        .json(&json!({"code": format!("{tag}_W"), "name": "W", "intervalDays": 400}))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        messages(&r),
        vec!["intervalDays must not be greater than 366"]
    );
    let r = app
        .post(&vm("frequencies"))
        .token(&c.fx.admin.token)
        .json(&json!({"code": format!("{tag}_W"), "name": "W", "intervalDays": 7}))
        .send()
        .await;
    r.assert_status(201).assert_no_sql_error();
    assert_key_order(
        &r.text(),
        &[
            "id",
            "code",
            "name",
            "description",
            "sortOrder",
            "isActive",
            "intervalDays",
            "createdAt",
            "updatedAt",
        ],
    );
    assert!(r.data().get("requiresOrder").is_none() && r.data().get("isDefault").is_none());
    assert_eq!(r.data()["intervalDays"], 7);
    let w = r.data()["id"].as_str().unwrap().to_owned();
    let r = app
        .get(&format!("{}?isDefault=true", vm("frequencies")))
        .token(&c.fx.admin.token)
        .send()
        .await;
    r.assert_status(422);
    assert_eq!(code_of(&r), "VISIT_MASTER_INVALID_FIELD");
    let r = app
        .patch(&format!("{}/{w}", vm("frequencies")))
        .token(&c.fx.admin.token)
        .json(&json!({"intervalDays": 14, "requiresGps": false}))
        .send()
        .await;
    r.assert_status(422);
    let r = app
        .patch(&format!("{}/{w}", vm("frequencies")))
        .token(&c.fx.admin.token)
        .json(&json!({"intervalDays": 14}))
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.data()["intervalDays"], 14);
    let r = app
        .get(&format!(
            "{}?search={}&isActive=true",
            vm("frequencies"),
            tag.to_lowercase()
        ))
        .token(&c.fx.admin.token)
        .send()
        .await;
    assert_eq!(r.json["meta"]["total"], 1);
    app.delete(&format!("{}/{w}", vm("frequencies")))
        .token(&c.fx.admin.token)
        .send()
        .await
        .assert_status(200);

    // Helper konsumen: resolve_visit_types / find_default.
    let t = app
        .post(&vm("types"))
        .token(&c.fx.admin.token)
        .json(&json!({"code": format!("{tag}_T"), "name": "T", "isDefault": true}))
        .send()
        .await;
    t.assert_status(201);
    let t_id = t.data()["id"].as_str().unwrap().to_owned();
    let t2 = app
        .post(&vm("types"))
        .token(&c.fx.admin.token)
        .json(&json!({"code": format!("{tag}_U"), "name": "U", "isActive": false}))
        .send()
        .await;
    let u_id = t2.data()["id"].as_str().unwrap().to_owned();
    let mut conn = app.db().acquire().await.unwrap();
    let map = mz_sales_geo::visit_masters::resolve_visit_types(
        &mut conn,
        c.fx.id,
        &[t_id.clone(), t_id.clone()],
    )
    .await
    .unwrap();
    assert_eq!(map.len(), 1);
    assert_eq!(
        mz_sales_geo::visit_masters::to_visit_type_ref(&map[&t_id]),
        json!({"id": t_id, "code": format!("{tag}_T"), "name": "T", "requiresOrder": false,
               "requiresPhoto": false, "requiresGps": true, "isActive": true})
    );
    let missing = "00000000-0000-4000-8000-000000000001".to_owned();
    let e = mz_sales_geo::visit_masters::resolve_visit_types(
        &mut conn,
        c.fx.id,
        &[t_id.clone(), missing.clone()],
    )
    .await
    .unwrap_err();
    assert_eq!(e.status().as_u16(), 404);
    let e = mz_sales_geo::visit_masters::resolve_visit_types(
        &mut conn,
        c.fx.id,
        std::slice::from_ref(&u_id),
    )
    .await
    .unwrap_err();
    assert_eq!(e.status().as_u16(), 422);
    let d = mz_sales_geo::visit_masters::find_default(&mut conn, c.fx.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(d.uuid, t_id);
}
