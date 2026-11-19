//! Region toko (`/v1/admin/store-groups`) dan jadwal kunjungan (`/v1/admin/store-visit-plans`,
//! `/v1/mobile/store-visit-plans`). Sales yang ditugaskan/dijadwalkan = MANAGER fixture (role tidak
//! diperiksa), supaya scope STAFF yang dipakai area lain tidak terganggu; assignment dibersihkan di akhir.

use serde_json::{Value, json};

use super::{SERIAL, insert_group, insert_store, setup, tag};
use mz_store_visits::plan_dates::{add_plan_days, today_plan_date};

#[tokio::test]
async fn store_groups_crud_and_sales() {
    let Some((app, _em, c)) = setup().await else {
        return;
    };
    let _g = SERIAL.lock().await;
    let admin = &c.admin.token;
    let code = format!("G{}", tag());

    // STAFF ditolak RolesGuard (401 "Access denied: insufficient permissions").
    app.get("/v1/admin/store-groups")
        .token(&c.staff.token)
        .send()
        .await
        .assert_status(401);
    // Validasi.
    let r = app
        .post("/v1/admin/store-groups")
        .token(admin)
        .json(&json!({ "code": "x", "name": "" }))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        json!([
            "code must be 2-32 characters of uppercase letters, digits, or hyphens",
            "name must be longer than or equal to 1 characters"
        ])
    );

    let r = app
        .post("/v1/admin/store-groups")
        .token(admin)
        .json(&json!({ "code": code, "name": "Jogja Barat" }))
        .send()
        .await;
    r.assert_status(201);
    let g = r.data().clone();
    assert_eq!(g["storeCount"], 0);
    assert_eq!(g["description"], Value::Null);
    let uuid = g["id"].as_str().unwrap().to_owned();
    let gid: i32 = app
        .scalar(&format!(
            "SELECT id FROM \"storeGroup\" WHERE uuid = '{uuid}'"
        ))
        .await;

    let r = app
        .post("/v1/admin/store-groups")
        .token(admin)
        .json(&json!({ "code": code, "name": "Dup" }))
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(r.json["code"], "STORE_GROUP_CODE_TAKEN");

    let r = app
        .get(&format!(
            "/v1/admin/store-groups?search={code}&sort=code&order=desc"
        ))
        .token(admin)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(r.json["meta"]["total"], 1);
    assert!(r.json["summary"]["unassignedSalesCount"].is_number());

    let r = app
        .patch(&format!("/v1/admin/store-groups/{uuid}"))
        .token(admin)
        .json(&json!({ "name": "Jogja B", "description": "Barat", "isActive": false }))
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.data()["name"], "Jogja B");
    assert_eq!(r.data()["isActive"], true);

    app.get("/v1/admin/store-groups/00000000-0000-4000-8000-000000000000")
        .token(admin)
        .send()
        .await
        .assert_status(404);

    // Penugasan: uuid asing → 422; manager → 200.
    let r = app
        .put(&format!("/v1/admin/store-groups/{uuid}/sales"))
        .token(admin)
        .json(&json!({ "userUuids": ["00000000-0000-4000-8000-000000000000"] }))
        .send()
        .await;
    r.assert_status(422);
    assert_eq!(r.json["code"], "STORE_GROUP_ASSIGNEE_INVALID");
    let r = app
        .put(&format!("/v1/admin/store-groups/{uuid}/sales"))
        .token(admin)
        .json(&json!({ "userUuids": [c.manager.uuid] }))
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(r.data()[0]["id"], c.manager.uuid);
    assert_eq!(r.data()[0]["isActiveMember"], true);
    let r = app
        .get(&format!("/v1/admin/store-groups/{uuid}"))
        .token(admin)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.data()["salesCount"], 1);
    assert_eq!(r.data()["sales"].as_array().unwrap().len(), 1);

    // Arsip ditolak selama ada sales.
    let r = app
        .delete(&format!("/v1/admin/store-groups/{uuid}"))
        .token(admin)
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(
        r.json["details"],
        json!({ "storeCount": 0, "salesCount": 1 })
    );

    let r = app
        .put(&format!("/v1/admin/store-groups/{uuid}/sales"))
        .token(admin)
        .json(&json!({ "userUuids": [] }))
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.data(), &json!([]));
    assert_eq!(
        app.scalar::<i64>(&format!(
            "SELECT COUNT(*) FROM \"storeGroupUser\" WHERE \"groupId\" = {gid} AND \"deletedAt\" IS NOT NULL"
        ))
        .await,
        1
    );
    let r = app
        .get(&format!("/v1/admin/store-groups/{uuid}/sales"))
        .token(admin)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json, json!({ "data": [] }));

    let r = app
        .delete(&format!("/v1/admin/store-groups/{uuid}"))
        .token(admin)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.data(), &json!({ "id": uuid, "archived": true }));

    let r = app
        .put(&format!("/v1/admin/store-groups/{uuid}/sales"))
        .token(admin)
        .json(&json!({ "userUuids": [c.manager.uuid] }))
        .send()
        .await;
    r.assert_status(422);
    assert_eq!(r.json["code"], "STORE_GROUP_INACTIVE");

    let r = app
        .post(&format!("/v1/admin/store-groups/{uuid}/restore"))
        .token(admin)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.data()["isActive"], true);
}

#[tokio::test]
async fn store_visit_plans_admin_and_mobile() {
    let Some((app, _em, c)) = setup().await else {
        return;
    };
    let _g = SERIAL.lock().await;
    let admin = &c.admin.token;
    let t = tag();
    let (gid, _) = insert_group(&app, c.id, c.admin.id, &format!("P{t}")).await;
    let (_, s1) = insert_store(
        &app,
        c.id,
        c.admin.id,
        &format!("A{t}"),
        -7.75,
        110.3,
        Some(gid),
    )
    .await;
    let (_, s2) = insert_store(
        &app,
        c.id,
        c.admin.id,
        &format!("B{t}"),
        -7.76,
        110.31,
        None,
    )
    .await;
    // Hari tanpa jadwal lain milik manager.
    let base: i64 = app
        .scalar(&format!(
            "SELECT COALESCE(MAX(\"visitDate\") - CURRENT_DATE, 0)::bigint FROM \"storeVisitPlan\" WHERE \"userId\" = {}",
            c.manager.id
        ))
        .await;
    let d1 = add_plan_days(&today_plan_date(), base.max(0) + 1).unwrap();
    let d2 = add_plan_days(&d1, 1).unwrap();

    // Validasi & tanggal lampau.
    let r = app
        .post("/v1/admin/store-visit-plans")
        .token(admin)
        .json(&json!({ "userId": c.manager.uuid, "dates": ["2020-01-01"], "stores": [{ "storeId": s1 }] }))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(r.json["code"], "PLAN_DATE_IN_PAST");
    let r = app
        .post("/v1/admin/store-visit-plans")
        .token(admin)
        .json(&json!({ "userId": "x", "stores": [] }))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        json!([
            "userId must be a UUID",
            "stores must contain at least 1 elements"
        ])
    );

    let r = app
        .post("/v1/admin/store-visit-plans")
        .token(admin)
        .json(&json!({
            "userId": c.manager.uuid, "dates": [d1, d2],
            "stores": [{ "storeId": s1, "sortOrder": 2, "note": "  tagih  " }, { "storeId": s2 }]
        }))
        .send()
        .await;
    r.assert_status(201).assert_no_sql_error();
    assert_eq!(
        r.json,
        json!({ "created": 4, "skipped": 0, "dates": 2, "stores": 2 })
    );
    // Audit `createMany` → satu baris ringkasan `storeVisitPlan` (ditulis sesudah respons).
    let mut audited = 0i64;
    for _ in 0..40 {
        audited = app
            .scalar(&format!(
                "SELECT COUNT(*) FROM \"auditLog\" WHERE \"companyId\" = {} AND \"entityType\"::text = 'storeVisitPlan' AND \"createdAt\" > NOW() - interval '1 minute'",
                c.id
            ))
            .await;
        if audited > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(
        audited > 0,
        "audit storeVisitPlan createMany tidak tertulis"
    );
    let r = app
        .post("/v1/admin/store-visit-plans")
        .token(admin)
        .json(&json!({ "userId": c.manager.uuid, "dates": [d1], "stores": [{ "storeId": s1 }] }))
        .send()
        .await;
    r.assert_status(201);
    assert_eq!(r.json["skipped"], 1);
    let r = app
        .post("/v1/admin/store-visit-plans")
        .token(admin)
        .json(&json!({ "userId": c.manager.uuid, "dates": [d1], "stores": [{ "storeId": s1 }], "skipExisting": false }))
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(r.json["code"], "PLAN_DUPLICATE");

    // Daftar, kalender, kepatuhan.
    let q = format!("startDate={d1}&endDate={d2}&salesId={}", c.manager.uuid);
    let r = app
        .get(&format!("/v1/admin/store-visit-plans?{q}&status=pending"))
        .token(admin)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(r.json["meta"]["total"], 4);
    let first = r.data()[0].clone();
    assert_eq!(first["visitDate"], d1);
    assert_eq!(first["note"], "tagih");
    assert_eq!(first["sales"]["id"], c.manager.uuid);
    assert_eq!(first["store"]["group"]["code"], format!("P{t}"));
    let plan = first["id"].as_str().unwrap().to_owned();
    app.get(&format!("/v1/admin/store-visit-plans?{q}&status=missed"))
        .token(admin)
        .send()
        .await
        .assert_status(200);
    app.get("/v1/admin/store-visit-plans?startDate=2026-01-01")
        .token(admin)
        .send()
        .await
        .assert_status(400);

    let r = app
        .get(&format!("/v1/admin/store-visit-plans/calendar?{q}"))
        .token(admin)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(
        r.json["data"][0],
        json!({ "date": d1, "salesId": c.manager.uuid, "planned": 2, "fulfilled": 0, "missed": 0, "pending": 2 })
    );
    for g in ["sales", "region"] {
        let r = app
            .get(&format!(
                "/v1/admin/store-visit-plans/compliance?{q}&groupBy={g}"
            ))
            .token(admin)
            .send()
            .await;
        r.assert_status(200).assert_no_sql_error();
        assert_eq!(r.json["totals"]["planned"], 4);
        assert_eq!(r.json["totals"]["complianceRate"], Value::Null);
    }

    // Detail, ubah, hapus.
    let r = app
        .get(&format!("/v1/admin/store-visit-plans/{plan}"))
        .token(admin)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.data()["id"], plan);
    let r = app
        .patch(&format!("/v1/admin/store-visit-plans/{plan}"))
        .token(admin)
        .json(&json!({ "note": "   ", "sortOrder": null }))
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(r.data()["note"], Value::Null);
    assert_eq!(r.data()["sortOrder"], Value::Null);
    let r = app
        .patch(&format!("/v1/admin/store-visit-plans/{plan}"))
        .token(admin)
        .json(&json!({ "visitTypeId": "00000000-0000-4000-8000-000000000000" }))
        .send()
        .await;
    r.assert_status(404);
    assert_eq!(r.json["code"], "VISIT_TYPE_NOT_FOUND");
    app.delete(&format!("/v1/admin/store-visit-plans/{plan}"))
        .token(admin)
        .send()
        .await
        .assert_status(204);
    let r = app
        .delete(&format!("/v1/admin/store-visit-plans/{plan}"))
        .token(admin)
        .send()
        .await;
    r.assert_status(404);
    assert_eq!(r.json["code"], "PLAN_NOT_FOUND");
    let r = app
        .get(&format!("/v1/admin/store-visit-plans/{plan}"))
        .token(admin)
        .send()
        .await;
    r.assert_status(404);
    assert_eq!(r.json["code"], "STORE_VISIT_PLAN_NOT_FOUND");

    // Ganti isi hari d2: hanya s2.
    let r = app
        .put("/v1/admin/store-visit-plans/day")
        .token(admin)
        .json(&json!({ "userId": c.manager.uuid, "date": d2, "stores": [{ "storeId": s2, "sortOrder": 1 }] }))
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(r.json["date"], d2);
    assert_eq!(r.data().as_array().unwrap().len(), 1);
    assert_eq!(r.data()[0]["sortOrder"], 1);

    // Mobile (manager sebagai sales).
    let r = app
        .get(&format!(
            "/v1/mobile/store-visit-plans?date={d2}&lat=-7.75&lng=110.3"
        ))
        .token(&c.manager.token)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(
        r.json["summary"],
        json!({ "planned": 1, "fulfilled": 0, "remaining": 1 })
    );
    assert!(r.data()[0]["store"]["distanceMeters"].is_number());
    let r = app
        .get(&format!("/v1/mobile/store-visit-plans?date={d2}&lat=-7.75"))
        .token(&c.manager.token)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        "lat and lng must be sent together, or not at all"
    );
    let r = app
        .get(&format!(
            "/v1/mobile/store-visit-plans/summary?startDate={d1}&endDate={d2}"
        ))
        .token(&c.manager.token)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(
        r.json["data"],
        json!([
            { "date": d1, "planned": 1, "fulfilled": 0, "remaining": 1 },
            { "date": d2, "planned": 1, "fulfilled": 0, "remaining": 1 }
        ])
    );
    app.get("/v1/mobile/store-visit-plans/summary?startDate=2026-01-01&endDate=2025-01-01")
        .token(&c.manager.token)
        .send()
        .await
        .assert_status(400);
}
