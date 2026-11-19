//! `/v1/admin/store-visits/**` (daftar + KPI, detail, export, foto) dan `/v1/mobile/offline/sync`.
//! Kunjungan fixture dibuat lewat SQL dalam state `completed`/`cancelled` saja (index parsial
//! "satu kunjungan aktif per user" dipakai bersama test lain). Daftar disaring `storeId` fixture
//! supaya tidak terpengaruh data test paralel.

use mz_testkit::TestApp;
use serde_json::{Value, json};

use super::{SERIAL, insert_group, insert_store, setup, tag};

/// Token unik per pemanggilan (limiter per token tidak bocor antar-test / antar-run).
fn fresh_token(app: &TestApp, user_id: i32) -> String {
    app.token_with(json!({ "userId": user_id, "n": tag() }))
}

/// Kunjungan fixture (`checkInAt` = sekarang - `minutes_ago`). Kembalikan `(id, uuid)`.
#[allow(clippy::too_many_arguments)]
async fn insert_visit(
    app: &TestApp,
    company_id: i32,
    store_id: i32,
    user_id: i32,
    group_id: Option<i32>,
    state: &str,
    status: Option<&str>,
    gps_issue: bool,
) -> (i32, String) {
    sqlx::query_as(
        r#"INSERT INTO "storeVisit" ("companyId", "storeId", "userId", "visitNumber", "clientRequestId",
             state, status, "checkInAt", "checkInLat", "checkInLng", "checkInAccuracy", "checkInDistance",
             "checkInGpsValidation", "hasGpsIssue", "storeGroupId", "durationSeconds", "checkOutAt",
             "checkOutDistance", "checkOutGpsValidation", "checkOutIsMock", tags, note)
           VALUES ($1, $2, $3, $4, gen_random_uuid(), $5::"StoreVisitState", $6::"StoreVisitStatus",
             NOW() - interval '1 hour', -7.75, 110.29, 12, 30, 'valid'::"GpsValidation", $7, $8,
             CASE WHEN $5 = 'completed' THEN 1440 END,
             CASE WHEN $5 = 'completed' THEN NOW() - interval '30 minutes' END,
             CASE WHEN $5 = 'completed' THEN 28 END,
             CASE WHEN $5 = 'completed' THEN 'valid'::"GpsValidation" END,
             CASE WHEN $5 = 'completed' THEN false END,
             ARRAY['restock'], 'Catatan, "kutip"')
           RETURNING id, uuid::text"#,
    )
    .bind(company_id)
    .bind(store_id)
    .bind(user_id)
    .bind(format!("IT-{}", tag()))
    .bind(state)
    .bind(status)
    .bind(gps_issue)
    .bind(group_id)
    .fetch_one(app.db())
    .await
    .unwrap()
}

struct Fx {
    store_code: String,
    store_uuid: String,
    group_uuid: String,
    done_uuid: String,
    done_id: i32,
    cancelled_uuid: String,
}

async fn fixture(app: &TestApp, c: &mz_testkit::Company) -> Fx {
    let t = tag();
    let (gid, group_uuid) = insert_group(app, c.id, c.admin.id, &format!("G{t}")).await;
    let (sid, store_uuid) = insert_store(
        app,
        c.id,
        c.admin.id,
        &format!("S{t}"),
        -7.75,
        110.29,
        Some(gid),
    )
    .await;
    let (done_id, done_uuid) = insert_visit(
        app,
        c.id,
        sid,
        c.staff.id,
        Some(gid),
        "completed",
        Some("success"),
        true,
    )
    .await;
    let (_, cancelled_uuid) =
        insert_visit(app, c.id, sid, c.staff.id, None, "cancelled", None, false).await;
    Fx {
        store_code: format!("S{t}"),
        store_uuid,
        group_uuid,
        done_uuid,
        done_id,
        cancelled_uuid,
    }
}

#[tokio::test]
async fn admin_list_summary_and_filters() {
    let Some((app, _em, c)) = setup().await else {
        return;
    };
    let fx = fixture(&app, &c).await;
    let tok = fresh_token(&app, c.admin.id);
    let res = app
        .get(&format!("/v1/admin/store-visits?storeId={}", fx.store_uuid))
        .token(&tok)
        .send()
        .await;
    res.assert_no_sql_error().assert_status(200);
    let body = &res.json;
    let data = body["data"].as_array().unwrap();
    assert_eq!(data.len(), 2, "{body}");
    let s = &body["summary"];
    assert_eq!(s["visitsInRange"], 1);
    assert_eq!(s["completedVisits"], 1);
    assert_eq!(s["cancelledVisits"], 1);
    assert_eq!(s["successRate"], 100);
    assert_eq!(s["gpsIssues"], 1);
    assert_eq!(s["avgDurationSeconds"], 1440);
    let by = s["byGroup"].as_array().unwrap();
    assert_eq!(by.len(), 2);
    assert_eq!(by[0]["group"]["id"], json!(fx.group_uuid));
    assert_eq!(by[1]["group"], Value::Null);
    assert_eq!(
        body["meta"],
        json!({"total": 2, "page": 1, "limit": 20, "totalPages": 1})
    );
    let item = data
        .iter()
        .find(|d| d["id"] == json!(fx.done_uuid))
        .unwrap();
    assert_eq!(item["store"]["id"], json!(fx.store_uuid));
    assert_eq!(item["sales"]["id"], json!(c.staff.uuid));
    assert_eq!(item["checkOut"]["distanceMeters"], 28);
    assert_eq!(item["isPlanned"], false);

    // state/status hanya mempersempit data, bukan summary.
    let res = app
        .get(&format!(
            "/v1/admin/store-visits?storeId={}&state=completed&groupId={}&unassigned=true&planned=false",
            fx.store_uuid, fx.group_uuid
        ))
        .token(&tok)
        .send()
        .await;
    res.assert_no_sql_error().assert_status(200);
    assert_eq!(res.json["data"].as_array().unwrap().len(), 1);
    assert_eq!(res.json["summary"]["cancelledVisits"], 1);

    // search (ILIKE) atas kode toko, huruf kecil.
    let res = app
        .get(&format!(
            "/v1/admin/store-visits?search={}",
            fx.store_code.to_lowercase()
        ))
        .token(&tok)
        .send()
        .await;
    res.assert_no_sql_error().assert_status(200);
    assert_eq!(res.json["meta"]["total"], 2);

    // Validasi + rentang tidak sah.
    let res = app
        .get("/v1/admin/store-visits?limit=101")
        .token(&tok)
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        json!(["limit must not be greater than 100"])
    );
    let res = app
        .get("/v1/admin/store-visits?startDate=2026-08-03&endDate=2026-08-02")
        .token(&tok)
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(res.json["code"], "INVALID_DATE_RANGE");

    // STAFF bukan role admin: RolesGuard bawaan menjawab 401 (UnauthorizedException Node).
    let res = app
        .get("/v1/admin/store-visits")
        .token(&fresh_token(&app, c.staff.id))
        .send()
        .await;
    res.assert_status(401);
}

#[tokio::test]
async fn admin_detail_and_photos() {
    let Some((app, em, c)) = setup().await else {
        return;
    };
    let fx = fixture(&app, &c).await;
    let tok = fresh_token(&app, c.admin.id);
    let photo_uuid: String = app.scalar("SELECT gen_random_uuid()::text").await;
    let key = format!(
        "store-visits/{}/{}/{photo_uuid}-store.jpg",
        c.id, fx.done_uuid
    );
    sqlx::query(
        r#"INSERT INTO "storeVisitPhoto" (uuid, "visitId", kind, "objectKey", "sizeBytes", "contentType",
             "isConfirmed", "takenAt")
           VALUES ($1::uuid, $2, 'store', $3, 4, 'image/jpeg', true, NOW())"#,
    )
    .bind(&photo_uuid)
    .bind(fx.done_id)
    .bind(&key)
    .execute(app.db())
    .await
    .unwrap();
    em.put(&key, b"JPEG", "image/jpeg");

    let res = app
        .get(&format!("/v1/admin/store-visits/{}", fx.done_uuid))
        .token(&tok)
        .send()
        .await;
    res.assert_no_sql_error().assert_status(200);
    let d = &res.json["data"];
    assert_eq!(d["id"], json!(fx.done_uuid));
    assert_eq!(d["group"]["id"], json!(fx.group_uuid));
    assert_eq!(d["plan"], Value::Null);
    assert_eq!(d["photos"][0]["id"], json!(photo_uuid));
    assert!(
        d["photos"][0]["url"]
            .as_str()
            .unwrap()
            .contains(&photo_uuid)
    );

    // Daftar menghitung foto terkonfirmasi tanpa URL.
    let res = app
        .get(&format!("/v1/admin/store-visits?storeId={}", fx.store_uuid))
        .token(&tok)
        .send()
        .await;
    let item = res.json["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["id"] == json!(fx.done_uuid))
        .unwrap()
        .clone();
    assert_eq!(item["photosCount"], 1);
    assert_eq!(item["photoKinds"], json!(["store"]));

    let res = app
        .get(&format!(
            "/v1/admin/store-visits/{}/photos/{photo_uuid}/file",
            fx.done_uuid
        ))
        .token(&tok)
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(res.bytes, b"JPEG");

    let res = app
        .get(&format!(
            "/v1/admin/store-visits/{}/photos/{photo_uuid}/file/url",
            fx.done_uuid
        ))
        .token(&tok)
        .send()
        .await;
    res.assert_status(200);
    assert!(
        res.json["url"]
            .as_str()
            .unwrap()
            .contains("X-Goog-Signature")
    );
    assert_eq!(res.headers["cache-control"], "private, max-age=300");

    // Foto kunjungan lain / tidak ada → 404 seragam.
    let res = app
        .get(&format!(
            "/v1/admin/store-visits/{}/photos/{photo_uuid}/file",
            fx.cancelled_uuid
        ))
        .token(&tok)
        .send()
        .await;
    res.assert_status(404);
    assert_eq!(res.json["code"], "VISIT_PHOTO_NOT_FOUND");

    // 404 detail, 400 uuid rusak, 404 lintas-company.
    let res = app
        .get("/v1/admin/store-visits/3b7e1a90-22c4-4f6d-9a11-8c2de5f47031")
        .token(&tok)
        .send()
        .await;
    res.assert_status(404);
    assert_eq!(
        res.json,
        json!({"code": "VISIT_NOT_FOUND", "message": "Visit not found"})
    );
    let res = app
        .get("/v1/admin/store-visits/nope")
        .token(&tok)
        .send()
        .await;
    res.assert_status(400);
    let other = app.other_company_admin().await;
    let res = app
        .get(&format!("/v1/admin/store-visits/{}", fx.done_uuid))
        .token(&other.admin.token)
        .send()
        .await;
    assert!(res.status != 200);
}

#[tokio::test]
async fn admin_export_csv_xlsx_and_quota() {
    let Some((app, _em, c)) = setup().await else {
        return;
    };
    let _g = SERIAL.lock().await;
    let fx = fixture(&app, &c).await;
    let tok = fresh_token(&app, c.admin.id);
    let saved: (i32, i32, Option<chrono::NaiveDateTime>) = sqlx::query_as(
        r#"SELECT "reportExportsPerMonth", "reportExportsUsedThisMonth", "reportExportsResetAt" FROM company WHERE id = $1"#,
    )
    .bind(c.id)
    .fetch_one(app.db())
    .await
    .unwrap();
    app.exec(&format!(
        r#"UPDATE company SET "reportExportsPerMonth" = -1 WHERE id = {}"#,
        c.id
    ))
    .await;

    let res = app
        .get(&format!(
            "/v1/admin/store-visits/export?storeId={}",
            fx.store_uuid
        ))
        .token(&tok)
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(res.headers["content-type"], "text/csv; charset=utf-8");
    assert_eq!(
        res.headers["content-disposition"],
        "attachment; filename=\"store-visits-export.csv\""
    );
    let csv = res.text();
    assert!(csv.starts_with('\u{feff}'));
    assert!(csv.contains("id,visitNumber,salesId"));
    assert!(csv.contains(&fx.done_uuid));
    assert!(csv.contains("\"Catatan, \"\"kutip\"\"\""));
    assert_eq!(csv.matches("\r\n").count(), 3);

    let res = app
        .get(&format!(
            "/v1/admin/store-visits/export?storeId={}&format=xlsx",
            fx.store_uuid
        ))
        .token(&tok)
        .send()
        .await;
    res.assert_status(200);
    assert!(res.bytes.starts_with(b"PK"));

    let res = app
        .get("/v1/admin/store-visits/export?format=pdf")
        .token(&tok)
        .send()
        .await;
    res.assert_status(400);

    app.exec(&format!(
        r#"UPDATE company SET "reportExportsPerMonth" = 0, "reportExportsUsedThisMonth" = 0,
             "reportExportsResetAt" = NOW() + interval '10 days' WHERE id = {}"#,
        c.id
    ))
    .await;
    let res = app
        .get(&format!(
            "/v1/admin/store-visits/export?storeId={}",
            fx.store_uuid
        ))
        .token(&tok)
        .send()
        .await;
    res.assert_no_sql_error().assert_status(402);
    assert_eq!(res.json["code"], "EXPORT_QUOTA_EXCEEDED");

    sqlx::query(
        r#"UPDATE company SET "reportExportsPerMonth" = $2, "reportExportsUsedThisMonth" = $3,
             "reportExportsResetAt" = $4 WHERE id = $1"#,
    )
    .bind(c.id)
    .bind(saved.0)
    .bind(saved.1)
    .bind(saved.2)
    .execute(app.db())
    .await
    .unwrap();
}

#[tokio::test]
async fn offline_sync_bundle_and_etag() {
    let Some((app, _em, c)) = setup().await else {
        return;
    };
    let t = tag();
    let (sid, store_uuid) =
        insert_store(&app, c.id, c.admin.id, &format!("O{t}"), -7.7, 110.3, None).await;
    let plan_uuid: String = sqlx::query_scalar(
        r#"INSERT INTO "storeVisitPlan" ("companyId", "userId", "storeId", "visitDate", "createdById", note)
           VALUES ($1, $2, $3, (NOW() AT TIME ZONE 'UTC')::date, $4, 'tagih') RETURNING uuid::text"#,
    )
    .bind(c.id)
    .bind(c.staff2.id)
    .bind(sid)
    .bind(c.admin.id)
    .fetch_one(app.db())
    .await
    .unwrap();

    let tok = fresh_token(&app, c.staff2.id);
    let res = app
        .get("/v1/mobile/offline/sync?days=2")
        .token(&tok)
        .send()
        .await;
    res.assert_no_sql_error().assert_status(200);
    let b = &res.json;
    let keys: Vec<&str> = b.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        &keys[..8],
        &[
            "syncedAt",
            "syncToken",
            "range",
            "config",
            "scope",
            "activeVisit",
            "stores",
            "plans"
        ]
    );
    assert_eq!(b["range"]["days"], 2);
    assert!(b["config"]["timeZone"].is_string());
    let plan = b["plans"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == json!(plan_uuid))
        .unwrap();
    assert_eq!(plan["storeId"], json!(store_uuid));
    assert_eq!(plan["status"], "pending");
    assert_eq!(plan["note"], "tagih");
    let token = b["syncToken"].as_str().unwrap().to_owned();
    assert_eq!(token.len(), 32);
    assert_eq!(res.headers["etag"], format!("\"{token}\"").as_str());

    // If-None-Match cocok → 304 tanpa body.
    let res = app
        .get("/v1/mobile/offline/sync?days=2")
        .token(&tok)
        .header("if-none-match", &format!("W/\"{token}\""))
        .send()
        .await;
    res.assert_status(304);
    assert!(res.bytes.is_empty());

    // Validasi.
    let res = app
        .get("/v1/mobile/offline/sync?days=8")
        .token(&tok)
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        json!(["days must not be greater than 7"])
    );
    let res = app
        .get("/v1/mobile/offline/sync?date=2026-02-31")
        .token(&tok)
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(res.json["code"], "INVALID_DATE_RANGE");
}
