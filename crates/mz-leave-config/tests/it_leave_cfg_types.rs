//! Uji integrasi `/leave-types*` (7 route) terhadap Postgres nyata: jalur sukses, validasi,
//! 404/400 domain, role, isolasi company, dan efek samping kuota cuti sakit ke kebijakan/saldo.
//! Jalankan: `MZ_TEST_DATABASE_URL=... MZ_TEST_REQUIRE_DB=1 cargo test -p mz-leave-config --test it_leave_cfg_types`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use common::*;
use mz_testkit::TestApp;
use serde_json::{Value, json};

const TYPE_KEYS: &[&str] = &[
    "id",
    "companyId",
    "name",
    "code",
    "deductBalance",
    "balanceSource",
    "requiresDocument",
    "annualQuota",
    "isActive",
    "isDefault",
    "createdAt",
    "updatedAt",
];

async fn create(app: &TestApp, fx: &Fx, body: Value) -> Value {
    let r = app
        .post("/leave-types")
        .token(&fx.admin.token)
        .json(&body)
        .send()
        .await;
    r.assert_status(201).assert_no_sql_error();
    r.json
}

#[tokio::test]
async fn create_list_get_shape_and_order() {
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let fx = fixture(&app).await;

    let made = create(&app, &fx, json!({"name": "Cuti Khusus", "annualQuota": 5})).await;
    assert_keys(&made, TYPE_KEYS);
    assert_eq!(made["code"], "CUTI_KHUSUS");
    assert_eq!(made["companyId"], fx.id);
    assert_eq!(made["deductBalance"], true);
    assert_eq!(made["balanceSource"], "ANNUAL");
    assert_eq!(made["requiresDocument"], false);
    assert_eq!(made["annualQuota"], 5);
    assert_eq!(made["isActive"], true);
    assert_eq!(made["isDefault"], false);
    assert!(made["updatedAt"].is_null());
    assert!(made["createdAt"].as_str().unwrap().ends_with('Z'));

    // Kode eksplisit + nilai bukan bawaan; huruf kecil di `name` tidak memengaruhi `code` eksplisit.
    let b = create(
        &app,
        &fx,
        json!({"name": "Alpha", "code": "A_1", "deductBalance": false, "balanceSource": "SICK",
               "requiresDocument": true, "isActive": false}),
    )
    .await;
    assert_eq!(b["code"], "A_1");
    assert_eq!(b["balanceSource"], "SICK");
    assert_eq!(b["isActive"], false);

    // Seed → baris isDefault diurutkan DULU di list (isDefault desc, name asc).
    app.post("/leave-types/seed")
        .token(&fx.admin.token)
        .send()
        .await
        .assert_status(201);
    let list = app.get("/leave-types").token(&fx.staff.token).send().await;
    list.assert_status(200).assert_no_sql_error();
    let names: Vec<&str> = list
        .json
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "Annual Leave",
            "Bereavement Leave",
            "Marriage Leave",
            "Maternity Leave",
            "Sick Leave",
            "Unpaid Leave",
            "Alpha",
            "Cuti Khusus"
        ]
    );

    // `isActive` lewat `@Type(() => Boolean)`: "false" → true (Boolean("false")), "" → false.
    let active = app
        .get("/leave-types?isActive=true")
        .token(&fx.admin.token)
        .send()
        .await;
    assert_eq!(active.json.as_array().unwrap().len(), 7);
    let quirk = app
        .get("/leave-types?isActive=false")
        .token(&fx.admin.token)
        .send()
        .await;
    assert_eq!(quirk.json.as_array().unwrap().len(), 7);
    let empty = app
        .get("/leave-types?isActive=")
        .token(&fx.admin.token)
        .send()
        .await;
    assert_eq!(empty.json.as_array().unwrap().len(), 1);
    let arr = app
        .get("/leave-types?isActive=a&isActive=b")
        .token(&fx.admin.token)
        .send()
        .await;
    arr.assert_status(400);
    assert_eq!(messages(&arr), ["isActive must be a boolean value"]);

    let one = app
        .get(&format!("/leave-types/{}", made["id"]))
        .token(&fx.staff.token)
        .send()
        .await;
    one.assert_status(200);
    assert_eq!(one.json, made);
}

#[tokio::test]
async fn create_validation_and_conflicts() {
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let fx = fixture(&app).await;
    let post = |body: Value| {
        let app = &app;
        let token = fx.admin.token.clone();
        async move {
            app.post("/leave-types")
                .token(&token)
                .json(&body)
                .send()
                .await
        }
    };

    let r = post(json!({})).await;
    r.assert_status(400);
    assert_eq!(
        messages(&r),
        [
            "name must be shorter than or equal to 100 characters",
            "name must be a string"
        ]
    );
    assert_eq!(r.json["error"], "Bad Request");

    let r = post(
        json!({"name": "x", "code": "abc", "annualQuota": 0, "balanceSource": "X",
                        "deductBalance": "yes", "isActive": 1}),
    )
    .await;
    r.assert_status(400);
    assert_eq!(
        messages(&r),
        [
            "code hanya boleh huruf kapital, angka, dan underscore",
            "deductBalance must be a boolean value",
            "balanceSource must be one of the following values: ANNUAL, SICK",
            "annualQuota must not be less than 1",
            "isActive must be a boolean value",
        ]
    );

    // annualQuota string numerik diterima (`@Type(() => Number)`), pecahan ditolak.
    post(json!({"name": "Q", "annualQuota": "3"}))
        .await
        .assert_status(201);
    let r = post(json!({"name": "Q2", "annualQuota": 1.5})).await;
    assert_eq!(messages(&r), ["annualQuota must be an integer number"]);

    // Kode kembar (juga kode hasil generate dari name).
    post(json!({"name": "Dup One"})).await.assert_status(201);
    let r = post(json!({"name": "dup one"})).await;
    r.assert_status(400);
    assert_eq!(r.json["message"], "Kode 'DUP_ONE' sudah digunakan");

    // Auth & role.
    app.post("/leave-types")
        .json(&json!({"name": "x"}))
        .send()
        .await
        .assert_status(401);
    app.post("/leave-types")
        .token(&fx.staff.token)
        .json(&json!({"name": "x"}))
        .send()
        .await
        .assert_status(401);
    app.post("/leave-types/seed")
        .token(&fx.staff.token)
        .send()
        .await
        .assert_status(401);
    // Manager boleh menulis.
    app.post("/leave-types")
        .token(&fx.manager.token)
        .json(&json!({"name": "Dari Manager"}))
        .send()
        .await
        .assert_status(201);
}

#[tokio::test]
async fn seed_is_idempotent_and_preserves_existing() {
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let fx = fixture(&app).await;
    // Tipe bawaan yang sudah ada tidak ditimpa (skipDuplicates).
    let pre = create(
        &app,
        &fx,
        json!({"name": "Custom Annual", "code": "ANNUAL_LEAVE", "annualQuota": 9}),
    )
    .await;
    let r = app
        .post("/leave-types/seed")
        .token(&fx.admin.token)
        .send()
        .await;
    r.assert_status(201).assert_no_sql_error();
    let rows = r.json.as_array().unwrap();
    // Hanya isDefault = true dikembalikan: 5 baru; baris 'ANNUAL_LEAVE' buatan sendiri bukan default.
    assert_eq!(rows.len(), 5, "{rows:?}");
    let names: Vec<&str> = rows.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        [
            "Bereavement Leave",
            "Marriage Leave",
            "Maternity Leave",
            "Sick Leave",
            "Unpaid Leave"
        ]
    );
    let marriage = rows.iter().find(|t| t["code"] == "MARRIAGE_LEAVE").unwrap();
    assert_eq!(marriage["annualQuota"], 3);
    assert_eq!(marriage["requiresDocument"], true);
    assert_eq!(marriage["deductBalance"], false);
    assert_eq!(
        rows.iter().find(|t| t["code"] == "SICK_LEAVE").unwrap()["balanceSource"],
        "SICK"
    );
    let again = app
        .post("/leave-types/seed")
        .token(&fx.admin.token)
        .send()
        .await;
    again.assert_status(201);
    assert_eq!(again.json, r.json);
    let got = app
        .get(&format!("/leave-types/{}", pre["id"]))
        .token(&fx.admin.token)
        .send()
        .await;
    assert_eq!(got.json["annualQuota"], 9);
}

#[tokio::test]
async fn get_one_errors_and_company_isolation() {
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let fx = fixture(&app).await;
    let fx2 = fixture(&app).await;
    let made = create(&app, &fx, json!({"name": "Milik A"})).await;
    let id = made["id"].as_i64().unwrap();

    app.get("/leave-types/99999999")
        .token(&fx.admin.token)
        .send()
        .await
        .assert_status(404);
    let r = app
        .get(&format!("/leave-types/{id}"))
        .token(&fx2.admin.token)
        .send()
        .await;
    r.assert_status(404);
    assert_eq!(r.json["message"], "Jenis cuti tidak ditemukan");
    // Company lain tidak bisa mengubah/menghapus/toggle.
    app.put(&format!("/leave-types/{id}"))
        .token(&fx2.admin.token)
        .json(&json!({"name": "x"}))
        .send()
        .await
        .assert_status(404);
    app.delete(&format!("/leave-types/{id}"))
        .token(&fx2.admin.token)
        .send()
        .await
        .assert_status(404);
    app.patch(&format!("/leave-types/{id}/toggle"))
        .token(&fx2.admin.token)
        .send()
        .await
        .assert_status(404);
    // Id bukan angka: `parseInt` NaN → galat Prisma → 500 (statis `seed` juga jatuh ke `:id`).
    for p in ["/leave-types/abc", "/leave-types/seed"] {
        let r = app.get(p).token(&fx.admin.token).send().await;
        assert_eq!(r.status.as_u16(), 500, "{p}: {}", r.text());
        assert_eq!(
            r.json,
            json!({"statusCode": 500, "message": "Internal server error"})
        );
    }
    let r = app
        .delete("/leave-types/seed")
        .token(&fx.admin.token)
        .send()
        .await;
    assert_eq!(r.status.as_u16(), 500);
    let r = app
        .put("/leave-types/seed")
        .token(&fx.admin.token)
        .json(&json!({}))
        .send()
        .await;
    assert_eq!(r.status.as_u16(), 500);
    // `12abc` → parseInt = 12 (bukan milik company ini → 404).
    app.get("/leave-types/12abc")
        .token(&fx.admin.token)
        .send()
        .await
        .assert_status(404);
    // Tanpa company → 404 'User is not associated with any company'.
    let loner: i32 = app
        .scalar(
            r#"INSERT INTO users (username, password, name, status)
               VALUES ('lc-loner-' || gen_random_uuid() || '@test.local', 'x', 'Loner', 1) RETURNING id"#,
        )
        .await;
    let r = app.get("/leave-types").user(loner).send().await;
    assert!(
        r.status.as_u16() == 404 || r.status.as_u16() == 403,
        "{}",
        r.text()
    );
}

#[tokio::test]
async fn update_toggle_delete() {
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let fx = fixture(&app).await;
    let a = create(&app, &fx, json!({"name": "Satu"})).await;
    let b = create(&app, &fx, json!({"name": "Dua"})).await;
    let id = a["id"].as_i64().unwrap();

    // Update parsial: hanya field yang dikirim berubah, `updatedAt` terisi.
    let r = app
        .put(&format!("/leave-types/{id}"))
        .token(&fx.admin.token)
        .json(&json!({"name": "Satu Baru", "requiresDocument": true, "annualQuota": 4}))
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert_keys(&r.json, TYPE_KEYS);
    assert_eq!(r.json["name"], "Satu Baru");
    assert_eq!(r.json["code"], "SATU");
    assert_eq!(r.json["requiresDocument"], true);
    assert_eq!(r.json["annualQuota"], 4);
    assert!(r.json["updatedAt"].is_string());
    // annualQuota null dibolehkan (kolom nullable).
    let r = app
        .put(&format!("/leave-types/{id}"))
        .token(&fx.admin.token)
        .json(&json!({"annualQuota": null}))
        .send()
        .await;
    r.assert_status(200);
    assert!(r.json["annualQuota"].is_null());

    // Ganti kode ke kode milik tipe lain → 400; ke kode sendiri → ok; kode baru → ok.
    let r = app
        .put(&format!("/leave-types/{id}"))
        .token(&fx.admin.token)
        .json(&json!({"code": "DUA"}))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(r.json["message"], "Kode 'DUA' sudah digunakan");
    app.put(&format!("/leave-types/{id}"))
        .token(&fx.admin.token)
        .json(&json!({"code": "SATU"}))
        .send()
        .await
        .assert_status(200);
    let r = app
        .put(&format!("/leave-types/{id}"))
        .token(&fx.admin.token)
        .json(&json!({"code": "TIGA"}))
        .send()
        .await;
    assert_eq!(r.json["code"], "TIGA");
    // Validasi (semua opsional di update).
    let r = app
        .put(&format!("/leave-types/{id}"))
        .token(&fx.admin.token)
        .json(&json!({"name": 5, "annualQuota": 0}))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        messages(&r),
        [
            "name must be shorter than or equal to 100 characters",
            "name must be a string",
            "annualQuota must not be less than 1"
        ]
    );
    // `name: null` lolos validasi (IsOptional) lalu Prisma menolak → 500.
    let r = app
        .put(&format!("/leave-types/{id}"))
        .token(&fx.admin.token)
        .json(&json!({"name": null}))
        .send()
        .await;
    assert_eq!(r.status.as_u16(), 500);

    // Toggle.
    let r = app
        .patch(&format!("/leave-types/{id}/toggle"))
        .token(&fx.admin.token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json["isActive"], false);
    let r = app
        .patch(&format!("/leave-types/{id}/toggle"))
        .token(&fx.manager.token)
        .send()
        .await;
    assert_eq!(r.json["isActive"], true);
    app.patch(&format!("/leave-types/{id}/toggle"))
        .token(&fx.staff.token)
        .send()
        .await
        .assert_status(401);
    app.patch("/leave-types/99999999/toggle")
        .token(&fx.admin.token)
        .send()
        .await
        .assert_status(404);

    // Hapus: ditolak bila dipakai pengajuan cuti, sukses bila tidak.
    let bid = b["id"].as_i64().unwrap();
    let user = employee(&app, fx.id, &format!("Pemohon {}", bid)).await;
    app.exec(&format!(
        r#"INSERT INTO "leaveRequest" ("userId", "companyId", "leaveTypeId", "startDate", "endDate", "totalDays", reason, status)
           VALUES ({user}, {}, {bid}, '2026-01-05', '2026-01-06', 2, 'x', 'PENDING'),
                  ({user}, {}, {bid}, '2026-02-05', '2026-02-06', 2, 'x', 'PENDING')"#,
        fx.id, fx.id
    ))
    .await;
    let r = app
        .delete(&format!("/leave-types/{bid}"))
        .token(&fx.admin.token)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        "Tidak bisa dihapus: 2 pengajuan cuti menggunakan jenis ini. Nonaktifkan saja."
    );
    app.delete(&format!("/leave-types/{bid}"))
        .token(&fx.staff.token)
        .send()
        .await
        .assert_status(401);
    let r = app
        .delete(&format!("/leave-types/{id}"))
        .token(&fx.admin.token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json, json!({"success": true}));
    app.get(&format!("/leave-types/{id}"))
        .token(&fx.admin.token)
        .send()
        .await
        .assert_status(404);
    app.delete(&format!("/leave-types/{id}"))
        .token(&fx.admin.token)
        .send()
        .await
        .assert_status(404);
}

/// Kuota jenis cuti bersumber SICK = sumber blok `sickLeave` di kebijakan & saldo.
#[tokio::test]
async fn sick_quota_propagates_to_policy_and_balances() {
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let fx = fixture(&app).await;
    let year: i32 = app
        .scalar("SELECT EXTRACT(YEAR FROM NOW() AT TIME ZONE 'UTC')::int")
        .await;
    let (e1, e2) = (
        employee(&app, fx.id, &format!("E1 {}", fx.id)).await,
        employee(&app, fx.id, &format!("E2 {}", fx.id)).await,
    );
    let b1: i32 = app
        .scalar(&format!(
            r#"INSERT INTO "leaveBalance" ("userId", "companyId", year, "sickLeaveTotal") VALUES ({e1}, {}, {year}, 0) RETURNING id"#,
            fx.id
        ))
        .await;
    let b2: i32 = app
        .scalar(&format!(
            r#"INSERT INTO "leaveBalance" ("userId", "companyId", year, "sickLeaveTotal") VALUES ({e2}, {}, {year}, 0) RETURNING id"#,
            fx.id
        ))
        .await;
    // b2 pernah di-adjust manual → tidak ditimpa.
    app.exec(&format!(
        r#"INSERT INTO "leaveBalanceAdjustment" ("balanceId", "userId", "companyId", field, delta, reason, source)
           VALUES ({b2}, {e2}, {}, 'sickLeaveTotal', 3, 'manual', 'ADJUSTMENT')"#,
        fx.id
    ))
    .await;
    app.exec(&format!(
        r#"UPDATE "leaveBalance" SET "sickLeaveTotal" = 3 WHERE id = {b2}"#
    ))
    .await;

    let sick = create(
        &app,
        &fx,
        json!({"name": "Sakit", "code": "SAKIT", "balanceSource": "SICK", "deductBalance": true}),
    )
    .await;
    let sid = sick["id"].as_i64().unwrap();
    // Membuat tipe tidak menyentuh kebijakan.
    let n: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "leavePolicy" WHERE "companyId" = {}"#,
            fx.id
        ))
        .await;
    assert_eq!(n, 0);

    // Kuota angka → LIMITED + jatah; saldo yang belum di-adjust ikut.
    app.put(&format!("/leave-types/{sid}"))
        .token(&fx.admin.token)
        .json(&json!({"annualQuota": 8}))
        .send()
        .await
        .assert_status(200);
    let (mode, days): (String, Option<i32>) = sqlx::query_as(&format!(
        r#"SELECT "sickLeaveMode"::text, "sickLeaveDays" FROM "leavePolicy" WHERE "companyId" = {}"#,
        fx.id
    ))
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!((mode.as_str(), days), ("LIMITED", Some(8)));
    let t1: f64 = app
        .scalar(&format!(
            r#"SELECT "sickLeaveTotal" FROM "leaveBalance" WHERE id = {b1}"#
        ))
        .await;
    let t2: f64 = app
        .scalar(&format!(
            r#"SELECT "sickLeaveTotal" FROM "leaveBalance" WHERE id = {b2}"#
        ))
        .await;
    assert_eq!((t1, t2), (8.0, 3.0));

    // Kuota null → UNLIMITED, jatah null, saldo tidak disentuh.
    app.put(&format!("/leave-types/{sid}"))
        .token(&fx.admin.token)
        .json(&json!({"annualQuota": null}))
        .send()
        .await
        .assert_status(200);
    let (mode, days): (String, Option<i32>) = sqlx::query_as(&format!(
        r#"SELECT "sickLeaveMode"::text, "sickLeaveDays" FROM "leavePolicy" WHERE "companyId" = {}"#,
        fx.id
    ))
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!((mode.as_str(), days), ("UNLIMITED", None));
    let t1: f64 = app
        .scalar(&format!(
            r#"SELECT "sickLeaveTotal" FROM "leaveBalance" WHERE id = {b1}"#
        ))
        .await;
    assert_eq!(t1, 8.0);

    // Update yang tidak menyentuh kuota/sumber/deduct/aktif tidak memengaruhi kebijakan.
    app.put(&format!("/leave-types/{sid}"))
        .token(&fx.admin.token)
        .json(&json!({"annualQuota": 5}))
        .send()
        .await
        .assert_status(200);
    app.put(&format!("/leave-types/{sid}"))
        .token(&fx.admin.token)
        .json(&json!({"name": "Sakit 2"}))
        .send()
        .await
        .assert_status(200);
    let days: Option<i32> = sqlx::query_scalar(&format!(
        r#"SELECT "sickLeaveDays" FROM "leavePolicy" WHERE "companyId" = {}"#,
        fx.id
    ))
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!(days, Some(5));
    // Tipe nonaktif / tidak memotong saldo tidak mempropagasi.
    app.put(&format!("/leave-types/{sid}"))
        .token(&fx.admin.token)
        .json(&json!({"isActive": false, "annualQuota": 2}))
        .send()
        .await
        .assert_status(200);
    let days: Option<i32> = sqlx::query_scalar(&format!(
        r#"SELECT "sickLeaveDays" FROM "leavePolicy" WHERE "companyId" = {}"#,
        fx.id
    ))
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!(days, Some(5));
    // Tipe ANNUAL tidak pernah menyentuh blok sakit.
    let annual = create(&app, &fx, json!({"name": "Tahunan X"})).await;
    app.put(&format!("/leave-types/{}", annual["id"]))
        .token(&fx.admin.token)
        .json(&json!({"annualQuota": 1}))
        .send()
        .await
        .assert_status(200);
    let days: Option<i32> = sqlx::query_scalar(&format!(
        r#"SELECT "sickLeaveDays" FROM "leavePolicy" WHERE "companyId" = {}"#,
        fx.id
    ))
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!(days, Some(5));
}
