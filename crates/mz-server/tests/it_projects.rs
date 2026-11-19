//! Uji integrasi 8 route `/projects` (mz-projects) terhadap Postgres nyata.
//! Setiap test membuat datanya sendiri (kode & nama unik); fixture user/company
//! tidak pernah dinonaktifkan atau dihapus.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use mz_testkit::{Company, TestApp};
use serde_json::{Value, json};

static SEQ: AtomicU64 = AtomicU64::new(0);

/// Kode project unik, maksimal 6 karakter (validasi `code`).
fn unique_code() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    let n = nanos.wrapping_add(SEQ.fetch_add(1, Ordering::Relaxed).wrapping_mul(7919));
    format!("P{:05x}", n & 0xf_ffff)
}

fn unique_name(prefix: &str) -> String {
    format!("{prefix} {}", unique_code())
}

/// Body `POST /projects` valid untuk `usernames` sebagai anggota.
fn project_body(code: &str, name: &str, type_code: &str, usernames: &[&str]) -> Value {
    json!({
        "code": code,
        "name": name,
        "screenshotPerHour": 3,
        "projectTypeCode": type_code,
        "startDate": "2026-01-15",
        "usersUserName": usernames,
        "clientsCode": [],
        "totalHour": 0,
        "billable": true,
    })
}

/// Buat project lewat API sebagai super admin; kembalikan uuid-nya.
async fn create_project(
    app: &TestApp,
    fx: &Company,
    type_code: &str,
    usernames: &[&str],
) -> String {
    let code = unique_code();
    let body = project_body(&code, &unique_name("Proj"), type_code, usernames);
    let res = app
        .post("/projects")
        .token(&fx.super_admin.token)
        .json(&body)
        .send()
        .await;
    res.assert_ok();
    res.data()["project"]["uuid"]
        .as_str()
        .unwrap_or_else(|| panic!("uuid project tidak ada: {}", res.text()))
        .to_owned()
}

/// Ambil uuid project pertama yang dimiliki company (untuk uji daftar).
async fn fresh_client(app: &TestApp) -> String {
    let code = unique_code();
    app.exec(&format!(
        "INSERT INTO client (code, name, status) VALUES ('{code}', 'Client {code}', 1)"
    ))
    .await;
    code
}

// ───────────────────────── GET /projects ─────────────────────────

#[tokio::test]
async fn list_projects_super_admin_default_returns_all_newest_first() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let uuid = create_project(&app, &fx, "FIXED_HOURS", &[&fx.super_admin.username]).await;

    let res = app
        .get("/projects")
        .token(&fx.super_admin.token)
        .send()
        .await;
    res.assert_ok();
    res.assert_no_sql_error();
    let data = res.data();
    let total = data["total"].as_i64().unwrap();
    let rows = data["result"].as_array().unwrap();
    assert!(total >= 1);
    assert_eq!(
        rows.len() as i64,
        total,
        "tanpa paginasi semua baris dikembalikan"
    );
    assert_eq!(data["itemsPerPage"].as_i64().unwrap(), total);
    assert_eq!(data["pageNumber"].as_i64().unwrap(), 1);
    // Urut createdAt desc (test lain bisa membuat project bersamaan, jadi jangan
    // mengandalkan project kita di urutan pertama).
    let created: Vec<&str> = rows
        .iter()
        .map(|r| r["createdAt"].as_str().unwrap())
        .collect();
    assert!(
        created.windows(2).all(|w| w[0] >= w[1]),
        "bukan createdAt desc"
    );
    let first = rows
        .iter()
        .find(|r| r["uuid"].as_str() == Some(uuid.as_str()))
        .expect("project baru ada di daftar");
    assert_eq!(first["projectType"]["code"], "FIXED_HOURS");
    assert!(first["projectUser"].is_array());
    assert!(first["projectClient"].is_array());
    assert!(first["projectCompany"].is_array());
    assert!(first["name"].as_str().unwrap().chars().count() <= 25);
}

#[tokio::test]
async fn list_projects_paginated_and_ordered() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    create_project(&app, &fx, "ROLLING_HOURS", &[&fx.super_admin.username]).await;
    create_project(&app, &fx, "ROLLING_HOURS", &[&fx.super_admin.username]).await;

    let res = app
        .get("/projects?pageNumber=1&itemsPerPage=1&orderBy=asc&orderColumn=code")
        .token(&fx.super_admin.token)
        .send()
        .await;
    res.assert_ok();
    res.assert_no_sql_error();
    let data = res.data();
    assert_eq!(data["itemsPerPage"], 1);
    assert_eq!(data["pageNumber"], 1);
    assert_eq!(data["result"].as_array().unwrap().len(), 1);
    assert!(data["total"].as_i64().unwrap() >= 2);

    // Halaman kedua, urutan name desc: tetap sah dan tidak kosong.
    let res = app
        .get("/projects?pageNumber=2&itemsPerPage=1&orderBy=desc&orderColumn=name")
        .token(&fx.super_admin.token)
        .send()
        .await;
    res.assert_ok();
    res.assert_no_sql_error();
    assert_eq!(res.data()["result"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn list_projects_search_matches_code_or_name() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let code = unique_code();
    let name = unique_name("Searchable");
    let body = project_body(&code, &name, "FIXED_MILESTONE", &[&fx.super_admin.username]);
    app.post("/projects")
        .token(&fx.super_admin.token)
        .json(&body)
        .send()
        .await
        .assert_ok();

    // Cari berdasarkan bagian nama (ILIKE, tanpa peduli huruf besar/kecil).
    let res = app
        .get("/projects?search=searchable")
        .token(&fx.super_admin.token)
        .send()
        .await;
    res.assert_ok();
    res.assert_no_sql_error();
    let rows = res.data()["result"].as_array().unwrap().clone();
    assert!(rows.iter().any(|r| r["code"] == code.as_str()));

    // Cari berdasarkan kode.
    let res = app
        .get(&format!("/projects?search={code}"))
        .token(&fx.super_admin.token)
        .send()
        .await;
    res.assert_ok();
    assert_eq!(res.data()["total"].as_i64().unwrap(), 1);
}

#[tokio::test]
async fn list_projects_staff_scoped_to_membership() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    // Project yang hanya beranggotakan staff (bukan staff2).
    let uuid = create_project(&app, &fx, "FIXED_HOURS", &[&fx.staff.username]).await;

    let res = app.get("/projects").token(&fx.staff.token).send().await;
    res.assert_ok();
    res.assert_no_sql_error();
    let rows = res.data()["result"].as_array().unwrap().clone();
    assert!(rows.iter().any(|r| r["uuid"] == uuid.as_str()));
    // Staff tidak boleh melihat project tanpa keanggotaannya.
    let res2 = app.get("/projects").token(&fx.staff2.token).send().await;
    res2.assert_ok();
    let rows2 = res2.data()["result"].as_array().unwrap().clone();
    assert!(!rows2.iter().any(|r| r["uuid"] == uuid.as_str()));
}

#[tokio::test]
async fn list_projects_invalid_order_is_rejected() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    // orderColumn di luar whitelist: Prisma menolak → galat server (parity Node).
    let res = app
        .get("/projects?pageNumber=1&itemsPerPage=5&orderColumn=password")
        .token(&fx.super_admin.token)
        .send()
        .await;
    assert!(!res.status.is_success(), "{}", res.text());
}

/// Token desktop: membawa `sid` yang aktif di `userConnected`.
async fn device_token(app: &TestApp, user_id: i32) -> String {
    let sid: String = app.scalar("SELECT gen_random_uuid()::text").await;
    let mac = format!("mac-{}", unique_code());
    app.exec(&format!(
        r#"INSERT INTO "userConnected" ("userId", status, os, "macAddress", sid)
           VALUES ({user_id}, 1, 'test', '{mac}', '{sid}'::uuid)"#
    ))
    .await;
    app.token_with(json!({ "userId": user_id, "sid": sid }))
}

/// Query yang dipakai klien desktop (`Timemoz-Desktop/src/ipc/index.ts`).
const DESKTOP_LIST: &str = "/projects?pageNumber=1&itemsPerPage=100&orderBy=asc&orderColumn=name";

/// Baris daftar milik project `uuid`.
fn row_of<'a>(res: &'a Value, uuid: &str) -> &'a Value {
    res["data"]["result"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["uuid"] == uuid)
        .unwrap_or_else(|| panic!("project {uuid} tidak ada di daftar: {res}"))
}

#[tokio::test]
async fn list_projects_device_gets_lite_rows() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let uuid = create_project(
        &app,
        &fx,
        "FIXED_HOURS",
        &[&fx.staff.username, &fx.staff2.username],
    )
    .await;
    let token = device_token(&app, fx.staff.id).await;

    // DB dipakai bersama uji lain di binary ini yang membuat project di company fixture
    // yang sama secara paralel; ulangi sampai tidak ada project baru di antara dua request.
    let fetch = |tok: String| {
        let app = &app;
        async move {
            let res = app.get(DESKTOP_LIST).token(&tok).send().await;
            res.assert_ok();
            res.assert_no_sql_error();
            serde_json::from_str::<Value>(&res.text()).unwrap()
        }
    };
    let (mut full, mut lite) = (Value::Null, Value::Null);
    for _ in 0..10 {
        full = fetch(fx.staff.token.clone()).await;
        lite = fetch(token.clone()).await;
        let again = fetch(fx.staff.token.clone()).await;
        if again["data"]["total"] == full["data"]["total"] {
            break;
        }
    }

    // Paginasi & isi daftar sama; hanya relasi yang dikosongkan.
    for k in ["total", "itemsPerPage", "pageNumber"] {
        assert_eq!(lite["data"][k], full["data"][k], "{k}");
    }
    let uuids = |v: &Value| -> Vec<Value> {
        v["data"]["result"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["uuid"].clone())
            .collect()
    };
    assert_eq!(uuids(&lite), uuids(&full), "urutan/isi daftar harus sama");

    let (f, l) = (row_of(&full, &uuid), row_of(&lite, &uuid));
    assert_eq!(f["projectUser"].as_array().unwrap().len(), 2);
    for k in ["id", "uuid", "code", "name", "createdAt", "projectType"] {
        assert_eq!(l[k], f[k], "{k}");
    }
    assert_eq!(l["projectType"]["name"], f["projectType"]["name"]);
    assert_eq!(l["projectUser"], json!([]));
    assert_eq!(l["projectClient"], json!([]));
    assert_eq!(l["projectCompany"], json!([]));
    assert!(l["budgetSummary"].is_null());
    assert!(l["lastTracked"].is_null());

    // Token desktop yang dipakai dari browser (ada Origin) tetap dapat respons lengkap.
    let res = app
        .get(DESKTOP_LIST)
        .token(&token)
        .header("origin", "https://dashboard.example")
        .send()
        .await;
    res.assert_ok();
    let v: Value = serde_json::from_str(&res.text()).unwrap();
    assert_eq!(
        row_of(&v, &uuid)["projectUser"].as_array().unwrap().len(),
        2
    );
}

#[tokio::test]
async fn list_projects_device_lite_can_be_disabled() {
    let Some(app) =
        TestApp::with_vars(mz_server::app, &[("RS_PROJECTS_DEVICE_LITE", "false")]).await
    else {
        return;
    };
    let fx = app.company().await;
    let uuid = create_project(&app, &fx, "FIXED_HOURS", &[&fx.staff.username]).await;
    let token = device_token(&app, fx.staff.id).await;

    let res = app.get(DESKTOP_LIST).token(&token).send().await;
    res.assert_ok();
    let v: Value = serde_json::from_str(&res.text()).unwrap();
    let row = row_of(&v, &uuid);
    assert_eq!(
        row["projectUser"][0]["user"]["username"],
        fx.staff.username.as_str()
    );
    assert_eq!(row["projectCompany"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn list_projects_page_past_end_keeps_total() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    create_project(&app, &fx, "FIXED_HOURS", &[&fx.staff.username]).await;

    let all = app.get("/projects").token(&fx.staff.token).send().await;
    all.assert_ok();
    let total = all.data()["total"].as_i64().unwrap();
    assert!(total >= 1);

    // Halaman di luar jangkauan: kosong, tetapi `total` tetap jumlah sebenarnya.
    let res = app
        .get("/projects?pageNumber=999&itemsPerPage=10")
        .token(&fx.staff.token)
        .send()
        .await;
    res.assert_ok();
    assert_eq!(res.data()["total"].as_i64().unwrap(), total);
    assert_eq!(res.data()["result"], json!([]));

    // Hanya `pageNumber` > 1 tanpa `itemsPerPage` (= total): selalu kosong.
    let res = app
        .get("/projects?pageNumber=2")
        .token(&fx.staff.token)
        .send()
        .await;
    res.assert_ok();
    assert_eq!(res.data()["total"].as_i64().unwrap(), total);
    assert_eq!(res.data()["itemsPerPage"].as_i64().unwrap(), total);
    assert_eq!(res.data()["result"], json!([]));
}

// ───────────────────────── GET /projects/:uuid ─────────────────────────

#[tokio::test]
async fn find_one_super_admin_returns_relations_and_totals() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let uuid = create_project(&app, &fx, "FIXED_HOURS", &[&fx.staff.username]).await;

    let res = app
        .get(&format!("/projects/{uuid}"))
        .token(&fx.super_admin.token)
        .send()
        .await;
    res.assert_ok();
    res.assert_no_sql_error();
    let d = res.data();
    assert_eq!(d["uuid"], uuid.as_str());
    assert_eq!(d["projectType"]["code"], "FIXED_HOURS");
    assert!(d["hours"].is_array());
    assert!(d["manualTimeApprovers"].is_array());
    assert_eq!(
        d["projectUser"][0]["user"]["username"],
        fx.staff.username.as_str()
    );
    assert!(d["total_used"].is_number());
    assert!(d["projectCompany"].is_array());
}

#[tokio::test]
async fn find_one_device_poll_uses_sid_without_origin() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let uuid = create_project(&app, &fx, "FIXED_HOURS", &[&fx.super_admin.username]).await;

    // Token desktop: membawa `sid` (harus ada di userConnected) dan request tanpa Origin.
    let sid: String = app.scalar("SELECT gen_random_uuid()::text").await;
    let mac = format!("mac-{}", unique_code());
    app.exec(&format!(
        r#"INSERT INTO "userConnected" ("userId", status, os, "macAddress", sid)
           VALUES ({}, 1, 'test', '{mac}', '{sid}'::uuid)"#,
        fx.super_admin.id
    ))
    .await;
    let token = app.token_with(json!({ "userId": fx.super_admin.id, "sid": sid }));
    let res = app
        .get(&format!("/projects/{uuid}"))
        .token(&token)
        .send()
        .await;
    res.assert_ok();
    res.assert_no_sql_error();
    let d = res.data();
    assert_eq!(d["uuid"], uuid.as_str());
    assert_eq!(d["projectClient"], json!([]));
    assert_eq!(d["projectUser"], json!([]));
    assert_eq!(d["manualTimeApprovers"], json!([]));
    assert!(d["budget"].is_null());
    assert!(d["total_used"].is_number());

    // Poll kedua (diambil dari cache Redis bila tersedia) tetap sah.
    let res = app
        .get(&format!("/projects/{uuid}"))
        .token(&token)
        .send()
        .await;
    res.assert_ok();
    res.assert_no_sql_error();
}

#[tokio::test]
async fn find_one_bad_uuid_is_400() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let res = app
        .get("/projects/not-a-uuid")
        .token(&fx.super_admin.token)
        .send()
        .await;
    res.assert_status(400);
}

#[tokio::test]
async fn find_one_unknown_uuid_returns_not_found_body() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let res = app
        .get("/projects/00000000-0000-4000-8000-000000000000")
        .token(&fx.super_admin.token)
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(res.json["success"], false);
    assert_eq!(res.json["message"], "Data project not found");
    assert!(res.json["data"].is_null());
}

#[tokio::test]
async fn find_one_non_member_staff_gets_not_found_body() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let uuid = create_project(&app, &fx, "FIXED_MILESTONE", &[&fx.staff.username]).await;
    // staff2 bukan anggota: filter membership di query → tidak ditemukan.
    let res = app
        .get(&format!("/projects/{uuid}"))
        .token(&fx.staff2.token)
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(res.json["success"], false);
    assert!(res.json["data"].is_null());

    // Anggota (staff) bisa melihatnya.
    let res = app
        .get(&format!("/projects/{uuid}"))
        .token(&fx.staff.token)
        .send()
        .await;
    res.assert_ok();
    assert_eq!(res.data()["uuid"], uuid.as_str());
}

/// Bug A (diperbaiki 2026-11-17, deviasi 6): admin company lain tidak boleh membaca
/// detail project company fixture; jawabannya sama dengan project tidak ditemukan.
#[tokio::test]
async fn find_one_admin_of_other_company_gets_not_found_body() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let uuid = create_project(&app, &fx, "FIXED_HOURS", &[&fx.staff.username]).await;
    let other_token = app.other_company_admin().await.admin.token;

    for suffix in ["", "?x=1"] {
        let res = app
            .get(&format!("/projects/{uuid}{suffix}"))
            .token(&other_token)
            .send()
            .await;
        res.assert_status(200);
        assert_eq!(res.json["success"], false, "{}", res.text());
        assert_eq!(res.json["message"], "Data project not found");
        assert!(res.json["data"].is_null());
    }

    // Admin company pemilik tetap melihatnya.
    let res = app
        .get(&format!("/projects/{uuid}"))
        .token(&fx.admin.token)
        .send()
        .await;
    res.assert_ok();
    assert_eq!(res.data()["uuid"], uuid.as_str());
}

// ───────────────────────── POST /projects ─────────────────────────

#[tokio::test]
async fn create_project_with_budget_approvers_and_client() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let client_code = fresh_client(&app).await;

    let code = unique_code();
    let mut body = project_body(
        &code,
        &unique_name("Budgeted"),
        "FIXED_HOURS",
        &[&fx.super_admin.username, &fx.staff.username],
    );
    body["clientsCode"] = json!([client_code]);
    body["totalHour"] = json!(12);
    body["budget"] = json!({
        "isEnabled": true,
        "budgetType": "HOURS",
        "budgetHours": 40,
        "resetCycle": "MONTHLY",
        "alertThreshold": 80,
        "enforcement": "ALERT_ONLY",
        "countNonBillable": false,
    });
    body["manualTimeApprovers"] = json!([fx.super_admin.uuid]);

    let res = app
        .post("/projects")
        .token(&fx.super_admin.token)
        .json(&body)
        .send()
        .await;
    res.assert_ok();
    res.assert_no_sql_error();
    let d = res.data();
    assert_eq!(d["project"]["code"], code.as_str());
    assert_eq!(d["project"]["totalHour"], 12);
    assert_eq!(d["client"][0]["code"], client_code.as_str());
    assert_eq!(d["user"].as_array().unwrap().len(), 2);
    assert_eq!(d["company"].as_array().unwrap().len(), 1);

    let uuid = d["project"]["uuid"].as_str().unwrap().to_owned();
    // Jam awal `projectHour` dibuat karena totalHour > 0.
    let hours: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "projectHour" h JOIN project p ON p.id = h."projectId" WHERE p.uuid = '{uuid}'::uuid"#
        ))
        .await;
    assert_eq!(hours, 1);
    let budget_enabled: bool = app
        .scalar(&format!(
            r#"SELECT b."isEnabled" FROM "projectBudget" b JOIN project p ON p.id = b."projectId" WHERE p.uuid = '{uuid}'::uuid"#
        ))
        .await;
    assert!(budget_enabled);

    // Detail menampilkan budget, approver, dan klien.
    let res = app
        .get(&format!("/projects/{uuid}"))
        .token(&fx.super_admin.token)
        .send()
        .await;
    res.assert_ok();
    res.assert_no_sql_error();
    assert!(res.data()["budget"].is_object(), "budget: {}", res.text());
    // `{ ...budget, budgetType, usedHours, ... }`: skalar menurut urutan model `projectBudget`
    // (bukan urutan jsonb), lalu kunci hitungan HOURS.
    let budget_keys: Vec<&str> = res.data()["budget"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        budget_keys,
        [
            "id",
            "uuid",
            "projectId",
            "isEnabled",
            "budgetType",
            "billableRate",
            "budgetAmount",
            "countNonBillable",
            "currentBurnedCost",
            "budgetHours",
            "resetCycle",
            "alertThreshold",
            "enforcement",
            "cycleStartAt",
            "currentBurnedSeconds",
            "thresholdAlertSentAt",
            "budgetReachedSentAt",
            "lastResetAt",
            "createdAt",
            "updatedAt",
            "usedHours",
            "remainingHours",
            "overBudgetHours",
            "overBudgetPercent",
            "percentUsed",
            "status",
            "burnRatePerWeek",
            "projection",
            "summary",
        ]
    );
    assert_eq!(
        res.data()["manualTimeApprovers"][0]["user"]["username"],
        fx.super_admin.username.as_str()
    );
    assert_eq!(
        res.data()["projectClient"][0]["client"]["code"],
        client_code.as_str()
    );

    // Daftar juga memuat budgetSummary untuk project ini.
    let res = app
        .get(&format!("/projects?search={code}"))
        .token(&fx.super_admin.token)
        .send()
        .await;
    res.assert_ok();
    res.assert_no_sql_error();
    let row = &res.data()["result"][0];
    assert_eq!(row["uuid"], uuid.as_str());
}

#[tokio::test]
async fn create_project_duplicate_code_is_rejected() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let code = unique_code();
    let body = project_body(
        &code,
        &unique_name("Dup"),
        "FIXED_HOURS",
        &[&fx.super_admin.username],
    );
    app.post("/projects")
        .token(&fx.super_admin.token)
        .json(&body)
        .send()
        .await
        .assert_ok();
    let res = app
        .post("/projects")
        .token(&fx.super_admin.token)
        .json(&body)
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(res.json["success"], false);
    assert_eq!(res.json["message"], "Project code is already exists");
}

#[tokio::test]
async fn create_project_validation_errors_are_400() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    // Kode terlalu panjang (>6), tanggal tidak valid, daftar user kosong.
    let body = json!({
        "code": "TOOLONGCODE",
        "name": "x",
        "screenshotPerHour": 3,
        "projectTypeCode": "FIXED_HOURS",
        "startDate": "bukan-tanggal",
        "usersUserName": [],
        "clientsCode": [],
        "totalHour": 0,
    });
    let res = app
        .post("/projects")
        .token(&fx.super_admin.token)
        .json(&body)
        .send()
        .await;
    res.assert_status(400);
}

#[tokio::test]
async fn create_project_staff_is_forbidden() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let body = project_body(
        &unique_code(),
        "Staff try",
        "FIXED_HOURS",
        &[&fx.staff.username],
    );
    // RolesGuard Node melempar 401 "Access denied: insufficient permissions".
    let res = app
        .post("/projects")
        .token(&fx.staff.token)
        .json(&body)
        .send()
        .await;
    res.assert_status(401);
}

// ───────────────────────── PATCH /projects/:uuid ─────────────────────────

#[tokio::test]
async fn update_project_fields_budget_and_members() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let client_code = fresh_client(&app).await;
    let uuid = create_project(&app, &fx, "ROLLING_HOURS", &[&fx.staff.username]).await;

    let new_name = unique_name("Renamed");
    let patch = json!({
        "name": new_name,
        "screenshotPerHour": 5,
        "projectTypeCode": "FIXED_MILESTONE",
        "billable": false,
        "clientsCode": [client_code],
        "usersUserName": [fx.staff.username, fx.staff2.username],
        "budget": {
            "isEnabled": true,
            "budgetType": "COST",
            "budgetAmount": 1000,
            "billableRate": 25,
            "resetCycle": "NEVER",
            "alertThreshold": 90,
            "enforcement": "STOP_TRACKING",
        },
        "manualTimeApprovers": [fx.admin.uuid],
    });
    let res = app
        .patch(&format!("/projects/{uuid}"))
        .token(&fx.admin.token)
        .json(&patch)
        .send()
        .await;
    res.assert_ok();
    res.assert_no_sql_error();
    let d = res.data();
    assert_eq!(d["project"]["name"], new_name.as_str());
    assert_eq!(d["project"]["screenshotPerHour"], 5);
    assert_eq!(d["project"]["billable"], false);
    assert_eq!(d["client"][0]["code"], client_code.as_str());
    assert_eq!(d["user"].as_array().unwrap().len(), 2);

    let detail = app
        .get(&format!("/projects/{uuid}"))
        .token(&fx.super_admin.token)
        .send()
        .await;
    detail.assert_ok();
    detail.assert_no_sql_error();
    assert_eq!(detail.data()["projectType"]["code"], "FIXED_MILESTONE");
    assert_eq!(detail.data()["budget"]["budgetType"], "COST");
}

#[tokio::test]
async fn update_project_by_staff_is_forbidden() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let uuid = create_project(&app, &fx, "FIXED_HOURS", &[&fx.staff.username]).await;
    let res = app
        .patch(&format!("/projects/{uuid}"))
        .token(&fx.staff.token)
        .json(&json!({ "name": "nope" }))
        .send()
        .await;
    res.assert_status(401);
}

#[tokio::test]
async fn update_project_bad_uuid_and_bad_body() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    app.patch("/projects/xyz")
        .token(&fx.super_admin.token)
        .json(&json!({ "name": "x" }))
        .send()
        .await
        .assert_status(400);

    let uuid = create_project(&app, &fx, "FIXED_HOURS", &[&fx.super_admin.username]).await;
    app.patch(&format!("/projects/{uuid}"))
        .token(&fx.super_admin.token)
        .json(&json!({ "name": "x".repeat(60) }))
        .send()
        .await
        .assert_status(400);
}

#[tokio::test]
async fn update_project_code_collision_is_rejected() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let taken = unique_code();
    app.post("/projects")
        .token(&fx.super_admin.token)
        .json(&project_body(
            &taken,
            &unique_name("Taken"),
            "FIXED_HOURS",
            &[&fx.super_admin.username],
        ))
        .send()
        .await
        .assert_ok();
    let uuid = create_project(&app, &fx, "FIXED_HOURS", &[&fx.super_admin.username]).await;
    let res = app
        .patch(&format!("/projects/{uuid}"))
        .token(&fx.super_admin.token)
        .json(&json!({ "code": taken }))
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(res.json["success"], false);
    assert_eq!(res.json["message"], "Project code is already exists");
}

// ───────────────────────── DELETE /projects/:uuid ─────────────────────────

#[tokio::test]
async fn delete_project_soft_deletes_and_hides_from_list() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let uuid = create_project(&app, &fx, "FIXED_HOURS", &[&fx.super_admin.username]).await;

    let res = app
        .delete(&format!("/projects/{uuid}"))
        .token(&fx.super_admin.token)
        .send()
        .await;
    res.assert_ok();
    assert_eq!(res.data()["isDeleted"], true);

    let status: i32 = app
        .scalar(&format!(
            "SELECT status FROM project WHERE uuid = '{uuid}'::uuid"
        ))
        .await;
    assert_eq!(status, 0);

    let list = app
        .get(&format!("/projects?search={uuid}"))
        .token(&fx.super_admin.token)
        .send()
        .await;
    list.assert_ok();
    assert_eq!(list.data()["total"], 0);
}

#[tokio::test]
async fn delete_project_by_staff_is_forbidden_and_bad_uuid_400() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let uuid = create_project(&app, &fx, "FIXED_HOURS", &[&fx.staff.username]).await;
    app.delete(&format!("/projects/{uuid}"))
        .token(&fx.staff.token)
        .send()
        .await
        .assert_status(401);
    // Delete tanpa ParseUUIDPipe di Node: uuid tidak valid → galat DB → 500.
    app.delete("/projects/abc")
        .token(&fx.super_admin.token)
        .send()
        .await
        .assert_status(500);
}

// ───────────────────────── project hours ─────────────────────────

/// Ambil uuid `projectHour` pertama milik project.
async fn first_hour_uuid(app: &TestApp, project_uuid: &str) -> String {
    app.scalar::<String>(&format!(
        r#"SELECT h.uuid::text FROM "projectHour" h JOIN project p ON p.id = h."projectId"
           WHERE p.uuid = '{project_uuid}'::uuid ORDER BY h.id LIMIT 1"#
    ))
    .await
}

#[tokio::test]
async fn project_hour_create_update_delete_sync_total() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let uuid = create_project(&app, &fx, "FIXED_HOURS", &[&fx.super_admin.username]).await;

    // Tambah jam: total project = jumlah seluruh projectHour.
    let res = app
        .post(&format!("/projects/hour/{uuid}"))
        .token(&fx.super_admin.token)
        .json(&json!({ "totalHour": 10 }))
        .send()
        .await;
    res.assert_ok();
    res.assert_no_sql_error();
    assert_eq!(res.data()["isCreated"], true);
    let total: i32 = app
        .scalar(&format!(
            "SELECT \"totalHour\" FROM project WHERE uuid = '{uuid}'::uuid"
        ))
        .await;
    assert_eq!(total, 10);

    let res = app
        .post(&format!("/projects/hour/{uuid}"))
        .token(&fx.super_admin.token)
        .json(&json!({ "totalHour": 5 }))
        .send()
        .await;
    res.assert_ok();
    let total: i32 = app
        .scalar(&format!(
            "SELECT \"totalHour\" FROM project WHERE uuid = '{uuid}'::uuid"
        ))
        .await;
    assert_eq!(total, 15);

    // Ubah jam pertama: 10 → 20, total menjadi 25.
    let hour = first_hour_uuid(&app, &uuid).await;
    let res = app
        .patch(&format!("/projects/hour/{hour}"))
        .token(&fx.super_admin.token)
        .json(&json!({ "totalHour": 20 }))
        .send()
        .await;
    res.assert_ok();
    res.assert_no_sql_error();
    assert_eq!(res.data()["isUpdated"], true);
    let total: i32 = app
        .scalar(&format!(
            "SELECT \"totalHour\" FROM project WHERE uuid = '{uuid}'::uuid"
        ))
        .await;
    assert_eq!(total, 25);

    // Hapus jam pertama: total menjadi 5.
    let res = app
        .delete(&format!("/projects/hour/{hour}"))
        .token(&fx.super_admin.token)
        .send()
        .await;
    res.assert_ok();
    res.assert_no_sql_error();
    assert_eq!(res.data()["isDeleted"], true);
    let total: i32 = app
        .scalar(&format!(
            "SELECT \"totalHour\" FROM project WHERE uuid = '{uuid}'::uuid"
        ))
        .await;
    assert_eq!(total, 5);
}

/// Bug B (diperbaiki 2026-11-17, deviasi 5): menghapus jam terakhir -> sukses, totalHour 0.
#[tokio::test]
async fn project_hour_delete_last_hour_succeeds_with_zero_total() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let uuid = create_project(&app, &fx, "FIXED_HOURS", &[&fx.super_admin.username]).await;
    let res = app
        .post(&format!("/projects/hour/{uuid}"))
        .token(&fx.super_admin.token)
        .json(&json!({ "totalHour": 7 }))
        .send()
        .await;
    res.assert_ok();
    let hour = first_hour_uuid(&app, &uuid).await;

    let res = app
        .delete(&format!("/projects/hour/{hour}"))
        .token(&fx.super_admin.token)
        .send()
        .await;
    res.assert_ok();
    res.assert_no_sql_error();
    assert_eq!(res.json["success"], true, "{}", res.text());
    assert_eq!(res.json["message"], "delete project hour success");
    assert_eq!(res.data()["isDeleted"], true);
    let total: i32 = app
        .scalar(&format!(
            "SELECT \"totalHour\" FROM project WHERE uuid = '{uuid}'::uuid"
        ))
        .await;
    assert_eq!(total, 0);
    let rows: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "projectHour" WHERE uuid = '{hour}'::uuid"#
        ))
        .await;
    assert_eq!(rows, 0);
}

#[tokio::test]
async fn project_hour_bad_uuid_and_unknown_project() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    app.post("/projects/hour/not-uuid")
        .token(&fx.super_admin.token)
        .json(&json!({ "totalHour": 1 }))
        .send()
        .await
        .assert_status(400);
    app.patch("/projects/hour/not-uuid")
        .token(&fx.super_admin.token)
        .json(&json!({ "totalHour": 1 }))
        .send()
        .await
        .assert_status(400);
    // Node `deleteHour` tanpa ParseUUIDPipe: guard `@UUID` (2026-11-17) mencari baris dengan
    // uuid rusak -> galat Prisma (cast uuid) -> 500, seperti `DELETE /projects/:uuid`.
    app.delete("/projects/hour/not-uuid")
        .token(&fx.super_admin.token)
        .send()
        .await
        .assert_status(500);

    // Sejak guard company (2026-11-17), project/jam yang tidak ada dijawab 404 `Data not found`
    // oleh guard (sama dengan milik company lain), bukan body `success:false` dari service.
    for res in [
        app.post("/projects/hour/00000000-0000-4000-8000-000000000001")
            .token(&fx.super_admin.token)
            .json(&json!({ "totalHour": 1 }))
            .send()
            .await,
        app.patch("/projects/hour/00000000-0000-4000-8000-000000000002")
            .token(&fx.super_admin.token)
            .json(&json!({ "totalHour": 1 }))
            .send()
            .await,
        app.delete("/projects/hour/00000000-0000-4000-8000-000000000003")
            .token(&fx.super_admin.token)
            .send()
            .await,
    ] {
        res.assert_status(404);
        assert_eq!(res.json["message"], "Data not found", "{}", res.text());
    }
}

/// Celah lintas-tenant `@UUID` (diperbaiki 2026-11-17, deviasi 6): admin company LAIN tidak boleh
/// mengubah/menghapus project maupun jam project company fixture; jawabannya 404 `Data not found`
/// (sama dengan uuid yang tidak ada) dan data tidak berubah. Admin company sendiri tetap berhasil.
#[tokio::test]
async fn project_writes_by_admin_of_other_company_get_not_found() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let other = app.other_company_admin().await;
    let uuid = create_project(&app, &fx, "FIXED_HOURS", &[&fx.staff.username]).await;
    let res = app
        .post(&format!("/projects/hour/{uuid}"))
        .token(&fx.super_admin.token)
        .json(&json!({ "totalHour": 10 }))
        .send()
        .await;
    res.assert_ok();
    let hour = first_hour_uuid(&app, &uuid).await;
    let token = &other.admin.token;

    let denied = [
        app.patch(&format!("/projects/{uuid}"))
            .token(token)
            .json(&json!({ "name": "Hijacked" }))
            .send()
            .await,
        app.delete(&format!("/projects/{uuid}"))
            .token(token)
            .send()
            .await,
        app.post(&format!("/projects/hour/{uuid}"))
            .token(token)
            .json(&json!({ "totalHour": 99 }))
            .send()
            .await,
        app.patch(&format!("/projects/hour/{hour}"))
            .token(token)
            .json(&json!({ "totalHour": 99 }))
            .send()
            .await,
        app.delete(&format!("/projects/hour/{hour}"))
            .token(token)
            .send()
            .await,
    ];
    for res in &denied {
        res.assert_status(404);
        assert_eq!(res.json["message"], "Data not found", "{}", res.text());
    }

    // Data utuh: nama, status aktif, satu jam bernilai 10.
    let (name, status, total): (String, i32, i32) = sqlx::query_as(&format!(
        r#"SELECT name, status, "totalHour" FROM project WHERE uuid = '{uuid}'::uuid"#
    ))
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_ne!(name, "Hijacked");
    assert_eq!((status, total), (1, 10));
    let hours: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "projectHour" h JOIN project p ON p.id = h."projectId"
               WHERE p.uuid = '{uuid}'::uuid"#
        ))
        .await;
    assert_eq!(hours, 1);

    // Admin company sendiri tetap berhasil di setiap route yang sama.
    let own = &fx.admin.token;
    app.patch(&format!("/projects/{uuid}"))
        .token(own)
        .json(&json!({ "name": "Renamed by owner" }))
        .send()
        .await
        .assert_ok();
    app.post(&format!("/projects/hour/{uuid}"))
        .token(own)
        .json(&json!({ "totalHour": 5 }))
        .send()
        .await
        .assert_ok();
    let res = app
        .patch(&format!("/projects/hour/{hour}"))
        .token(own)
        .json(&json!({ "totalHour": 20 }))
        .send()
        .await;
    res.assert_ok();
    assert_eq!(res.data()["isUpdated"], true);
    let res = app
        .delete(&format!("/projects/hour/{hour}"))
        .token(own)
        .send()
        .await;
    res.assert_ok();
    assert_eq!(res.data()["isDeleted"], true);
    let res = app
        .delete(&format!("/projects/{uuid}"))
        .token(own)
        .send()
        .await;
    res.assert_ok();
    assert_eq!(res.data()["isDeleted"], true);
}

// ───────────────────────── urutan kunci = Prisma ─────────────────────────

fn keys(v: &Value) -> Vec<&str> {
    v.as_object()
        .unwrap_or_else(|| panic!("bukan objek: {v}"))
        .keys()
        .map(String::as_str)
        .collect()
}

const PROJECT_SCALARS: [&str; 13] = [
    "id",
    "uuid",
    "code",
    "name",
    "screenshotPerHour",
    "projectTypeId",
    "startDate",
    "createdAt",
    "updatedAt",
    "status",
    "totalHour",
    "billable",
    "clickupListId",
];

/// Objek relasi/baris yang dibangun di SQL tidak boleh berurutan jsonb (panjang lalu abjad):
/// urutan kunci = `select`/`include` Node (`projects.service.ts`).
#[tokio::test]
async fn project_json_keys_follow_node_order() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    let client_code = fresh_client(&app).await;
    let code = unique_code();
    let mut body = project_body(
        &code,
        &unique_name("Order"),
        "FIXED_HOURS",
        &[&fx.staff.username, &fx.super_admin.username],
    );
    body["clientsCode"] = json!([client_code]);
    body["totalHour"] = json!(12);
    body["manualTimeApprovers"] = json!([fx.super_admin.uuid]);

    // POST: { project, client, company, user }.
    let res = app
        .post("/projects")
        .token(&fx.super_admin.token)
        .json(&body)
        .send()
        .await;
    res.assert_ok();
    assert_eq!(res.json["success"], true, "{}", res.text());
    let d = res.data();
    assert_eq!(keys(d), ["project", "client", "company", "user"]);
    assert_eq!(keys(&d["project"]), PROJECT_SCALARS);
    assert_eq!(keys(&d["client"][0]), ["uuid", "code", "name"]);
    assert_eq!(keys(&d["user"][0]), ["uuid", "username", "name"]);
    let company = keys(&d["company"][0]);
    assert_eq!(company.get(..4), Some(&["id", "uuid", "name", "telp"][..]));
    assert!(!company.contains(&"profilePictureFullPath"));
    let uuid = d["project"]["uuid"].as_str().unwrap().to_owned();

    // GET /projects/:uuid: skalar, lalu relasi menurut urutan `include`, lalu total_used/budget.
    let res = app
        .get(&format!("/projects/{uuid}"))
        .token(&fx.super_admin.token)
        .send()
        .await;
    res.assert_ok();
    res.assert_no_sql_error();
    let d = res.data();
    let mut expected: Vec<&str> = PROJECT_SCALARS.to_vec();
    expected.extend([
        "projectType",
        "hours",
        "projectClient",
        "projectUser",
        "projectCompany",
        "manualTimeApprovers",
        "total_used",
        "budget",
    ]);
    assert_eq!(keys(d), expected, "{}", res.text());
    assert_eq!(
        keys(&d["projectType"]),
        [
            "id",
            "uuid",
            "code",
            "name",
            "createdAt",
            "updatedAt",
            "status"
        ]
    );
    assert_eq!(
        keys(&d["hours"][0]),
        [
            "id",
            "uuid",
            "projectId",
            "totalHour",
            "createdAt",
            "updatedAt"
        ]
    );
    assert_eq!(keys(&d["projectClient"][0]), ["client"]);
    assert_eq!(
        keys(&d["projectClient"][0]["client"]),
        ["uuid", "code", "name"]
    );
    assert_eq!(
        keys(&d["projectUser"][0]["user"]),
        ["uuid", "username", "name"]
    );
    assert_eq!(keys(&d["projectCompany"][0]["company"]), ["uuid", "name"]);
    assert_eq!(
        keys(&d["manualTimeApprovers"][0]),
        ["uuid", "createdAt", "user"]
    );
    assert_eq!(
        keys(&d["manualTimeApprovers"][0]["user"]),
        ["uuid", "name", "username"]
    );
    // Format tanggal tetap `Date.toJSON()`.
    let created = d["hours"][0]["createdAt"].as_str().unwrap();
    assert!(
        created.len() == 24 && created.ends_with('Z'),
        "createdAt: {created}"
    );

    // GET /projects: id, uuid, name, code, createdAt, projectType, relasi; lalu budgetSummary, lastTracked.
    let res = app
        .get(&format!("/projects?search={code}"))
        .token(&fx.super_admin.token)
        .send()
        .await;
    res.assert_ok();
    let row = &res.data()["result"][0];
    assert_eq!(
        keys(row),
        [
            "id",
            "uuid",
            "name",
            "code",
            "createdAt",
            "projectType",
            "projectClient",
            "projectUser",
            "projectCompany",
            "budgetSummary",
            "lastTracked",
        ]
    );
    assert_eq!(
        keys(&row["projectType"]),
        [
            "id",
            "uuid",
            "code",
            "name",
            "createdAt",
            "updatedAt",
            "status"
        ]
    );
    assert_eq!(
        keys(&row["projectClient"][0]["client"]),
        ["uuid", "code", "name"]
    );
    assert_eq!(
        keys(&row["projectUser"][0]["user"]),
        ["uuid", "username", "name"]
    );
    assert_eq!(keys(&row["projectCompany"][0]["company"]), ["uuid", "name"]);

    // PATCH memakai bentuk yang sama dengan POST.
    let res = app
        .patch(&format!("/projects/{uuid}"))
        .token(&fx.super_admin.token)
        .json(&json!({ "name": unique_name("Order2") }))
        .send()
        .await;
    res.assert_ok();
    assert_eq!(keys(res.data()), ["project", "client", "company", "user"]);
    assert_eq!(keys(&res.data()["project"]), PROJECT_SCALARS);
}
