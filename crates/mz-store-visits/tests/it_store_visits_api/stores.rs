//! Toko admin (`/v1/admin/stores`) & mobile (`/mobile/stores`) terhadap Postgres nyata.

use mz_testkit::TestApp;
use serde_json::{Value, json};

use super::{SERIAL, insert_group, insert_store, setup, tag};

const RANDOM_UUID: &str = "4d1a2b3c-5e6f-4a7b-8c9d-0e1f2a3b4c5d";

fn store_body(code: &str) -> Value {
    json!({
        "code": code.to_lowercase(),
        "name": format!("Toko {code}"),
        "ownerName": "Pak Slamet",
        "phone": "0812-3456-7890",
        "address": "Jl. Kaliurang KM 5 No. 21",
        "lat": -7.7501067,
        "lng": 110.2973078,
        "radiusMeters": 150,
    })
}

/// Kode toko unik `XX-YYYYYY` dari [`tag`].
fn new_code() -> String {
    let t: String = tag().chars().filter(char::is_ascii_alphanumeric).collect();
    let (a, b) = t.split_at(t.len().min(4));
    let b: String = b.chars().take(6).collect();
    format!("T{a}-{}", if b.is_empty() { "1".to_owned() } else { b })
}

/// Hapus bucket limiter export company (Redis uji dipakai bersama antar run).
async fn clear_export_limit(app: &TestApp, company_id: i32) {
    use fred::prelude::KeysInterface;
    let _: Result<(), _> = app
        .state()
        .redis
        .next()
        .del::<(), _>(format!("rl:rl:export:company:{company_id}"))
        .await;
}

#[tokio::test]
async fn admin_store_crud_and_validation() {
    let Some((app, _em, c)) = setup().await else {
        return;
    };
    let tok = &c.admin.token;
    let code = new_code();

    // 400 validasi (urutan pesan class-validator).
    let r = app
        .post("/v1/admin/stores")
        .token(tok)
        .json(&json!({ "code": "x" }))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"][0],
        "code must match format like MJ-001 (2-6 letters/digits, dash, 1-6 letters/digits)"
    );

    // STAFF ditolak RolesGuard (401 `Access denied: insufficient permissions`, seperti Node).
    app.post("/v1/admin/stores")
        .token(&c.staff.token)
        .json(&store_body(&code))
        .send()
        .await
        .assert_status(401);

    // Create → 201, kode di-uppercase, telepon dinormalisasi, qrToken 64 hex.
    let r = app
        .post("/v1/admin/stores")
        .token(tok)
        .json(&store_body(&code))
        .send()
        .await;
    r.assert_status(201).assert_no_sql_error();
    let d = &r.json["data"];
    assert_eq!(d["code"], code.as_str());
    assert_eq!(d["phone"], "+6281234567890");
    assert_eq!(d["radiusMeters"], 150);
    assert_eq!(d["totalVisits"], 0);
    assert_eq!(d["group"], Value::Null);
    let qr = d["qrToken"].as_str().unwrap();
    assert_eq!(qr.len(), 64);
    assert!(d.get("companyId").is_none());
    let id = d["id"].as_str().unwrap().to_owned();

    // Kode sama → 409 STORE_CODE_EXISTS.
    let r = app
        .post("/v1/admin/stores")
        .token(tok)
        .json(&store_body(&code))
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(r.json["code"], "STORE_CODE_EXISTS");

    // Region tak dikenal → 404.
    let mut b = store_body(&new_code());
    b["groupId"] = json!(RANDOM_UUID);
    let r = app
        .post("/v1/admin/stores")
        .token(tok)
        .json(&b)
        .send()
        .await;
    r.assert_status(404);
    assert_eq!(r.json["code"], "STORE_GROUP_NOT_FOUND");

    // Detail memuat qrToken; list tidak.
    let r = app
        .get(&format!("/v1/admin/stores/{id}"))
        .token(tok)
        .send()
        .await;
    r.assert_ok().assert_no_sql_error();
    assert_eq!(r.json["data"]["qrToken"], qr);
    let r = app
        .get(&format!(
            "/v1/admin/stores?search={code}&sort=code&order=desc"
        ))
        .token(tok)
        .send()
        .await;
    r.assert_ok().assert_no_sql_error();
    assert_eq!(r.json["data"].as_array().unwrap().len(), 1);
    assert!(r.json["data"][0].get("qrToken").is_none());
    assert_eq!(r.json["meta"]["total"], 1);
    assert!(r.json["summary"]["totalStores"].as_i64().unwrap() >= 1);
    app.get("/v1/admin/stores?sort=nope")
        .token(tok)
        .send()
        .await
        .assert_status(400);
    app.get("/v1/admin/stores/not-a-uuid")
        .token(tok)
        .send()
        .await
        .assert_status(500);
    let r = app
        .get(&format!("/v1/admin/stores/{RANDOM_UUID}"))
        .token(tok)
        .send()
        .await;
    r.assert_status(404);
    assert_eq!(
        r.json,
        json!({ "code": "STORE_NOT_FOUND", "message": "Store not found" })
    );

    // Update: region aktif + rename; arsip region → 422.
    let gcode = tag();
    let (gid, guuid) = insert_group(&app, c.id, c.admin.id, &gcode).await;
    let r = app
        .patch(&format!("/v1/admin/stores/{id}"))
        .token(tok)
        .json(&json!({ "name": "Toko Baru Sekali", "groupId": guuid, "radiusMeters": null, "code": null }))
        .send()
        .await;
    r.assert_ok().assert_no_sql_error();
    assert_eq!(r.json["data"]["name"], "Toko Baru Sekali");
    assert_eq!(r.json["data"]["group"]["id"], guuid.as_str());
    assert_eq!(r.json["data"]["radiusMeters"], Value::Null);
    assert_eq!(r.json["data"]["code"], code.as_str());
    app.exec(&format!(
        r#"UPDATE "storeGroup" SET "isActive" = false WHERE id = {gid}"#
    ))
    .await;
    let r = app
        .patch(&format!("/v1/admin/stores/{id}"))
        .token(tok)
        .json(&json!({ "groupId": guuid }))
        .send()
        .await;
    r.assert_status(422);
    assert_eq!(r.json["code"], "STORE_GROUP_INACTIVE");

    // Eligible sales: company belum mengunci wilayah → anggota aktif (minimal admin).
    let r = app
        .get(&format!("/v1/admin/stores/{id}/eligible-sales"))
        .token(tok)
        .send()
        .await;
    r.assert_ok().assert_no_sql_error();
    assert!(r.json["data"].is_array());
    app.get(&format!("/v1/admin/stores/{RANDOM_UUID}/eligible-sales"))
        .token(tok)
        .send()
        .await
        .assert_status(404);

    // Audit `store` tercatat.
    // (ditulis `tokio::spawn` sesudah respons → tunggu sebentar)
    let mut n: i64 = 0;
    for _ in 0..30 {
        n = app
            .scalar(&format!(
                r#"SELECT COUNT(*) FROM "auditLog" WHERE "entityType"::text ILIKE 'store' AND "entityUuid"::text = '{id}'"#
            ))
            .await;
        if n >= 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(n >= 2, "audit store rows: {n}");

    // Delete (soft) → detail 404, kode boleh dipakai ulang.
    let r = app
        .delete(&format!("/v1/admin/stores/{id}"))
        .token(tok)
        .send()
        .await;
    r.assert_ok();
    assert_eq!(r.json["data"], json!({ "id": id, "deleted": true }));
    app.get(&format!("/v1/admin/stores/{id}"))
        .token(tok)
        .send()
        .await
        .assert_status(404);
    app.delete(&format!("/v1/admin/stores/{id}"))
        .token(tok)
        .send()
        .await
        .assert_status(404);
    app.post("/v1/admin/stores")
        .token(tok)
        .json(&store_body(&code))
        .send()
        .await
        .assert_status(201);
}

#[tokio::test]
async fn admin_bulk_group_and_customer() {
    let Some((app, _em, c)) = setup().await else {
        return;
    };
    let tok = &c.admin.token;
    let (_, s1) = insert_store(&app, c.id, c.admin.id, &new_code(), -7.7, 110.3, None).await;
    let (_, s2) = insert_store(&app, c.id, c.admin.id, &new_code(), -7.7, 110.3, None).await;
    let (gid, guuid) = insert_group(&app, c.id, c.admin.id, &tag()).await;

    let r = app
        .patch("/v1/admin/stores/bulk-group")
        .token(tok)
        .json(&json!({ "storeUuids": [s1, s2, RANDOM_UUID], "groupId": guuid }))
        .send()
        .await;
    r.assert_ok().assert_no_sql_error();
    assert_eq!(r.json["data"], json!({ "updated": 2 }));
    let n: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM store WHERE "groupId" = {gid}"#
        ))
        .await;
    assert_eq!(n, 2);
    // Filter list per region.
    let r = app
        .get(&format!("/v1/admin/stores?groupId={guuid}"))
        .token(tok)
        .send()
        .await;
    r.assert_ok().assert_no_sql_error();
    assert_eq!(r.json["meta"]["total"], 2);

    let r = app
        .patch("/v1/admin/stores/bulk-group")
        .token(tok)
        .json(&json!({ "storeUuids": [] }))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        json!([
            "storeUuids must contain at least 1 elements",
            "groupId must be a UUID"
        ])
    );

    // Customer fixture.
    let cust: String = app
        .scalar(&format!(
            r#"INSERT INTO customer ("companyId", code, name, "createdById")
               VALUES ({}, 'C{}', 'PT Uji', {}) RETURNING uuid::text"#,
            c.id,
            tag(),
            c.admin.id
        ))
        .await;
    let r = app
        .patch("/v1/admin/stores/bulk-customer")
        .token(tok)
        .json(&json!({ "storeIds": [s1, RANDOM_UUID], "customerId": cust }))
        .send()
        .await;
    r.assert_ok().assert_no_sql_error();
    assert_eq!(
        r.json["data"],
        json!({ "updated": 1, "notFound": [RANDOM_UUID] })
    );
    let r = app
        .get(&format!(
            "/v1/admin/stores?customerId={cust}&unassignedCustomer=false"
        ))
        .token(tok)
        .send()
        .await;
    r.assert_ok();
    assert_eq!(r.json["meta"]["total"], 1);
    assert_eq!(r.json["data"][0]["customer"]["id"], cust.as_str());
    let r = app
        .patch("/v1/admin/stores/bulk-customer")
        .token(tok)
        .json(&json!({ "storeIds": [s2], "customerId": RANDOM_UUID }))
        .send()
        .await;
    r.assert_status(404);
    assert_eq!(r.json["code"], "CUSTOMER_NOT_FOUND");
    let r = app
        .patch("/v1/admin/stores/bulk-customer")
        .token(tok)
        .json(&json!({ "storeIds": [s1], "customerId": null }))
        .send()
        .await;
    assert_eq!(r.json["data"], json!({ "updated": 1, "notFound": [] }));
}

#[tokio::test]
async fn admin_export_csv_and_xlsx() {
    let Some((app, _em, c)) = setup().await else {
        return;
    };
    let _g = SERIAL.lock().await;
    clear_export_limit(&app, c.id).await;
    app.exec(&format!(
        r#"UPDATE company SET "reportExportsPerMonth" = -1, "reportExportsUsedThisMonth" = 0,
           "reportExportsResetAt" = NULL WHERE id = {}"#,
        c.id
    ))
    .await;
    let tok = &c.admin.token;
    let code = new_code();
    insert_store(&app, c.id, c.admin.id, &code, -7.7, 110.3, None).await;

    let r = app
        .get(&format!("/v1/admin/stores/export?search={code}"))
        .token(tok)
        .send()
        .await;
    r.assert_ok();
    assert_eq!(r.headers["content-type"], "text/csv; charset=utf-8");
    assert_eq!(
        r.headers["content-disposition"],
        "attachment; filename=\"stores-export.csv\""
    );
    let text = String::from_utf8(r.bytes.clone()).unwrap();
    assert!(text.starts_with("\u{feff}id,code,name,ownerName,phone,address,lat,lng,radiusMeters,isActive,region,customerCode,customerName,totalVisits,lastVisitAt,lastVisitStatus,createdAt,updatedAt\r\n"));
    assert!(text.contains(&code));
    assert_eq!(text.matches("\r\n").count(), 2);

    let r = app
        .get(&format!(
            "/v1/admin/stores/export?format=xlsx&search={code}"
        ))
        .token(tok)
        .send()
        .await;
    r.assert_ok();
    assert_eq!(
        r.headers["content-type"],
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
    );
    assert!(r.bytes.starts_with(b"PK"));

    app.get("/v1/admin/stores/export?format=pdf")
        .token(tok)
        .send()
        .await
        .assert_status(400);

    // Kuota habis → 402 EXPORT_QUOTA_EXCEEDED (dicek sesudah batas baris).
    app.exec(&format!(
        r#"UPDATE company SET "reportExportsPerMonth" = 1, "reportExportsUsedThisMonth" = 1,
           "reportExportsResetAt" = NOW() + interval '10 days' WHERE id = {}"#,
        c.id
    ))
    .await;
    let r = app.get("/v1/admin/stores/export").token(tok).send().await;
    r.assert_status(402).assert_no_sql_error();
    assert_eq!(r.json["code"], "EXPORT_QUOTA_EXCEEDED");
    app.exec(&format!(
        r#"UPDATE company SET "reportExportsPerMonth" = -1 WHERE id = {}"#,
        c.id
    ))
    .await;
    clear_export_limit(&app, c.id).await;
}

#[tokio::test]
async fn mobile_stores_flow_and_scope() {
    let Some((app, _em, c)) = setup().await else {
        return;
    };
    let _g = SERIAL.lock().await;
    let tok = &c.staff.token;
    // Titik unik per run supaya penjaga duplikat & kotak jarak tidak tertukar dengan data lain.
    let seed = (tag().bytes().map(u64::from).sum::<u64>() % 1000) as f64;
    let (lat, lng) = (-60.0 - seed / 100.0, 20.0 + seed / 100.0);
    let name = format!("Toko Lapangan {}", tag());

    let body = json!({
        "name": name, "ownerName": "Bu Sari", "phone": "081234567890",
        "address": "Jl. Lapangan No. 1", "lat": lat, "lng": lng,
        "code": "IGNORED-1", "radiusMeters": 9
    });
    let r = app
        .post("/mobile/stores")
        .token(tok)
        .json(&body)
        .send()
        .await;
    r.assert_status(201).assert_no_sql_error();
    let d = &r.json["data"];
    assert!(d["code"].as_str().unwrap().starts_with("SLS-"));
    assert_eq!(d["phone"], "+6281234567890");
    assert_eq!(d["distanceMeters"], Value::Null);
    assert!(d.get("qrToken").is_none() && d.get("radiusMeters").is_none());
    let id = d["id"].as_str().unwrap().to_owned();
    let (radius, qr): (Option<i32>, String) =
        sqlx::query_as(r#"SELECT "radiusMeters", "qrToken" FROM store WHERE uuid = $1::uuid"#)
            .bind(&id)
            .fetch_one(app.db())
            .await
            .unwrap();
    assert_eq!(radius, None);

    // Duplikat dekat → 409.
    let r = app
        .post("/mobile/stores")
        .token(tok)
        .json(&body)
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(r.json["code"], "STORE_DUPLICATE_NEARBY");
    assert_eq!(r.json["details"]["storeId"], id.as_str());

    // Daftar berjarak.
    let r = app
        .get(&format!(
            "/mobile/stores?lat={lat}&lng={lng}&radiusKm=1&sort=distance"
        ))
        .token(tok)
        .send()
        .await;
    r.assert_ok().assert_no_sql_error();
    assert_eq!(r.json["data"][0]["id"], id.as_str());
    assert_eq!(r.json["data"][0]["distanceMeters"], 0);
    let r = app
        .get("/mobile/stores?sort=distance")
        .token(tok)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(r.json["message"], "sort=distance requires lat and lng");
    app.get(&format!("/mobile/stores?lat={lat}"))
        .token(tok)
        .send()
        .await
        .assert_status(400);
    let r = app
        .get(&format!(
            "/mobile/stores?search={}",
            name.replace(' ', "%20")
        ))
        .token(tok)
        .send()
        .await;
    r.assert_ok();
    assert_eq!(r.json["meta"]["total"], 1);

    // Detail + resolve-qr.
    let r = app
        .get(&format!("/mobile/stores/{id}"))
        .token(tok)
        .send()
        .await;
    r.assert_ok().assert_no_sql_error();
    assert_eq!(r.json["data"]["recentVisits"], json!([]));
    let r = app
        .post("/mobile/stores/resolve-qr")
        .token(tok)
        .json(&json!({ "qrToken": format!(" {} ", qr.to_uppercase()), "lat": lat, "lng": lng }))
        .send()
        .await;
    r.assert_ok().assert_no_sql_error();
    assert_eq!(r.json["data"]["id"], id.as_str());
    assert_eq!(r.json["data"]["distanceMeters"], 0);
    let r = app
        .post("/mobile/stores/resolve-qr")
        .token(tok)
        .json(&json!({ "qrToken": "0".repeat(64) }))
        .send()
        .await;
    r.assert_status(404);
    app.post("/mobile/stores/resolve-qr")
        .token(tok)
        .json(&json!({ "qrToken": "zz" }))
        .send()
        .await
        .assert_status(400);

    // Wilayah: STAFF memegang region lain + company mengunci → 403 + audit outOfScope.
    let (gid, _) = insert_group(&app, c.id, c.admin.id, &tag()).await;
    app.exec(&format!(
        r#"INSERT INTO "storeGroupUser" ("groupId", "userId") VALUES ({gid}, {});
           UPDATE company SET "storeVisitEnforceGroupScope" = true WHERE id = {};"#,
        c.staff.id, c.id
    ))
    .await;
    let r = app
        .get(&format!("/mobile/stores/{id}"))
        .token(tok)
        .send()
        .await;
    let audits: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "storeVisitAuditLog" a JOIN store s ON s.id = a."storeId"
               WHERE s.uuid = '{id}' AND a.event = 'outOfScope' AND a.lat = 0"#
        ))
        .await;
    app.exec(&format!(
        r#"UPDATE "storeGroupUser" SET "deletedAt" = NOW() WHERE "groupId" = {gid};
           UPDATE company SET "storeVisitEnforceGroupScope" = false WHERE id = {};"#,
        c.id
    ))
    .await;
    r.assert_status(403);
    assert_eq!(r.json["code"], "STORE_OUT_OF_SCOPE");
    assert_eq!(audits, 1);

    // Region tujuan tak dikenal → 404; validasi → 400.
    let mut b = body.clone();
    b["name"] = json!(format!("Toko Lain {}", tag()));
    b["groupId"] = json!(RANDOM_UUID);
    let r = app.post("/mobile/stores").token(tok).json(&b).send().await;
    r.assert_status(404);
    assert_eq!(r.json["code"], "STORE_GROUP_NOT_FOUND");
    let r = app
        .post("/mobile/stores")
        .token(tok)
        .json(&json!({ "name": "To" }))
        .send()
        .await;
    r.assert_status(400);
    app.get(&format!("/mobile/stores/{RANDOM_UUID}"))
        .token(tok)
        .send()
        .await
        .assert_status(404);
}

#[tokio::test]
async fn reveal_resolver_scopes_staff_only() {
    let Some((app, _em, c)) = setup().await else {
        return;
    };
    let (gid, _) = insert_group(&app, c.id, c.admin.id, &tag()).await;
    let (_, sid) = insert_store(&app, c.id, c.admin.id, &new_code(), 1.0, 1.0, Some(gid)).await;
    let st = app.state();
    let t =
        mz_store_visits::stores::admin::find_for_reveal(st, c.id, c.admin.id, &sid, Some("ADMIN"))
            .await
            .unwrap();
    assert_eq!(t.record_uuid.as_deref(), Some(sid.as_str()));
    assert!(t.record.get("qrToken").is_none());
    assert!(t.record_label.unwrap().contains(" — "));
    // Role tak dikenal = MANAGER ke atas.
    assert!(
        mz_store_visits::stores::admin::find_for_reveal(st, c.id, c.staff.id, &sid, None)
            .await
            .is_ok()
    );
    // Toko company lain → 404.
    assert!(
        mz_store_visits::stores::admin::find_for_reveal(
            st,
            c.id + 100_000,
            c.admin.id,
            &sid,
            Some("ADMIN")
        )
        .await
        .is_err()
    );
}
