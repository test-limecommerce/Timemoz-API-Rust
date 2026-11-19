//! Uji integrasi 10 route `/leave-holidays*` terhadap Postgres + Redis nyata (database
//! `mz_test_it_leave_hol_routes`). Tiap test memakai tahun sendiri (2091..2093) supaya
//! berjalan paralel di company fixture yang sama.
//! Jalankan: `MZ_TEST_DATABASE_URL=... cargo test -p mz-leave-holidays --test it_leave_hol_routes`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use axum::Router;
use fred::interfaces::HashesInterface;
use mz_core::AppState;
use mz_core::queue::{get, jobs_key};
use mz_leave::jobs::LEAVE_SCHEDULE_QUEUE;
use mz_testkit::{TestApp, TestResponse};
use serde_json::{Value, json};

fn build(state: &AppState) -> Router {
    mz_leave_holidays::router(state).with_state(state.clone())
}

/// Job `holiday-sync:<id>:<ms>` yang ada di antrean untuk libur `id`.
async fn sync_jobs(app: &TestApp, holiday_id: i64) -> Vec<String> {
    let keys: Vec<String> = app
        .state
        .redis
        .next()
        .hkeys(jobs_key(LEAVE_SCHEDULE_QUEUE))
        .await
        .unwrap();
    let prefix = format!("holiday-sync:{holiday_id}:");
    keys.into_iter()
        .filter(|k| k.starts_with(&prefix))
        .collect()
}

/// Payload job (`data`) pertama untuk libur `id`.
async fn sync_payload(app: &TestApp, holiday_id: i64) -> Vec<Value> {
    let mut out = Vec::new();
    for id in sync_jobs(app, holiday_id).await {
        let job = get(app.state(), LEAVE_SCHEDULE_QUEUE, &id)
            .await
            .unwrap()
            .unwrap();
        out.push(job.data);
    }
    out
}

fn keys_of(v: &Value) -> Vec<&str> {
    v.as_object().unwrap().keys().map(String::as_str).collect()
}

const HOLIDAY_KEYS: [&str; 10] = [
    "id",
    "companyId",
    "name",
    "date",
    "type",
    "isActive",
    "deductsLeave",
    "parentHolidayId",
    "createdAt",
    "updatedAt",
];

async fn post_json(app: &TestApp, token: &str, body: &Value) -> TestResponse {
    app.post("/leave-holidays")
        .token(token)
        .json(body)
        .send()
        .await
}

async fn clean_year(app: &TestApp, company: i32, year: i32) {
    app.exec(&format!(
        r#"DELETE FROM "companyHoliday" WHERE "companyId" = {company}
             AND date >= '{year}-01-01' AND date <= '{year}-12-31'"#
    ))
    .await;
}

fn multipart(field: &str, filename: &str, ctype: &str, data: &[u8]) -> (String, Vec<u8>) {
    let boundary = "----ITBOUNDARY7MA4YWxk";
    let mut body = Vec::new();
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{field}\"; filename=\"{filename}\"\r\nContent-Type: {ctype}\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(data);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

async fn upload(app: &TestApp, uri: &str, token: &str, csv: &str) -> TestResponse {
    let (ct, body) = multipart("file", "libur.csv", "text/csv", csv.as_bytes());
    app.post(uri).token(token).raw(&ct, body).send().await
}

#[tokio::test]
async fn gerbang_dan_validasi() {
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let fx = app.company().await;

    for (method, uri) in [
        ("GET", "/leave-holidays"),
        ("POST", "/leave-holidays"),
        ("GET", "/leave-holidays/1"),
        ("PUT", "/leave-holidays/1"),
        ("DELETE", "/leave-holidays/1"),
        ("PATCH", "/leave-holidays/1/toggle"),
        ("GET", "/leave-holidays/import/template"),
        ("POST", "/leave-holidays/import"),
        ("POST", "/leave-holidays/import/preview"),
        ("GET", "/leave-holidays/next-year-reminder"),
    ] {
        let m = http::Method::from_bytes(method.as_bytes()).unwrap();
        let res = app.request(m, uri).send().await;
        res.assert_status(401);
    }

    // STAFF: boleh baca (list/detail), dilarang menulis dan semua route impor/pengingat.
    app.get("/leave-holidays?year=2090")
        .token(&fx.staff.token)
        .send()
        .await
        .assert_status(200)
        .assert_no_sql_error();
    let res = app
        .post("/leave-holidays")
        .token(&fx.staff.token)
        .json(&json!({ "name": "x", "date": "2090-01-01" }))
        .send()
        .await;
    // `RolesGuard`: role tidak diizinkan = 401 (bukan 403), seperti Nest.
    res.assert_status(401);
    for (method, uri) in [
        ("PUT", "/leave-holidays/1"),
        ("DELETE", "/leave-holidays/1"),
        ("PATCH", "/leave-holidays/1/toggle"),
        ("GET", "/leave-holidays/import/template"),
        ("POST", "/leave-holidays/import"),
        ("POST", "/leave-holidays/import/preview"),
        ("GET", "/leave-holidays/next-year-reminder"),
    ] {
        let m = http::Method::from_bytes(method.as_bytes()).unwrap();
        app.request(m, uri)
            .token(&fx.staff.token)
            .send()
            .await
            .assert_status(401);
    }
    // MANAGER boleh menulis.
    app.get("/leave-holidays?year=2090")
        .token(&fx.manager.token)
        .send()
        .await
        .assert_status(200);

    // Validasi query.
    let res = app
        .get("/leave-holidays?year=1999&type=x&deductsLeave=ya")
        .token(&fx.admin.token)
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        json!([
            "year must not be less than 2000",
            "type must be one of the following values: NATIONAL_HOLIDAY, JOINT_LEAVE",
            "deductsLeave must be a boolean value"
        ])
    );
    assert_eq!(res.json["error"], "Bad Request");

    // Validasi body.
    let res = post_json(&app, &fx.admin.token, &json!({})).await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        json!([
            "name must be shorter than or equal to 200 characters",
            "name must be a string",
            "date must be a valid ISO 8601 date string"
        ])
    );
    let res = post_json(
        &app,
        &fx.admin.token,
        &json!({ "name": "a", "date": "2090-02-30" }),
    )
    .await;
    res.assert_status(400);
    // Tanggal cuti bersama sama dengan tanggal libur.
    let res = post_json(
        &app,
        &fx.admin.token,
        &json!({ "name": "a", "date": "2090-02-20", "jointLeaveDates": ["2090-02-20"] }),
    )
    .await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        "Tanggal cuti bersama tidak boleh sama dengan tanggal liburnya"
    );
    // Cuti bersama tidak boleh punya cuti bersama sendiri.
    let res = post_json(
        &app,
        &fx.admin.token,
        &json!({ "name": "a", "date": "2090-02-20", "type": "JOINT_LEAVE", "jointLeaveDates": ["2090-02-21"] }),
    )
    .await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        "Cuti bersama tidak bisa memiliki cuti bersama sendiri"
    );
    let n: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "companyHoliday" WHERE "companyId" = {} AND date >= '2090-01-01' AND date <= '2090-12-31'"#,
            fx.id
        ))
        .await;
    assert_eq!(n, 0, "validasi gagal tidak boleh menulis");

    // Id bukan angka / di luar Int32 = galat Prisma = 500; id tak ada = 404.
    for id in ["abc", "99999999999"] {
        let res = app
            .get(&format!("/leave-holidays/{id}"))
            .token(&fx.admin.token)
            .send()
            .await;
        res.assert_status(500);
        assert_eq!(
            res.json,
            json!({ "statusCode": 500, "message": "Internal server error" })
        );
    }
    let res = app
        .get("/leave-holidays/0")
        .token(&fx.admin.token)
        .send()
        .await;
    res.assert_status(404);
    assert_eq!(
        res.json,
        json!({ "message": "Holiday not found", "error": "Not Found", "statusCode": 404 })
    );
    // Path statis yang bukan GET-nya jatuh ke `:id`.
    app.get("/leave-holidays/import")
        .token(&fx.admin.token)
        .send()
        .await
        .assert_status(500);
}

#[tokio::test]
async fn crud_cuti_bersama_dan_sinkron_saldo() {
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let fx = app.company().await;
    let tok = &fx.admin.token;
    clean_year(&app, fx.id, 2091).await;

    let res = post_json(
        &app,
        tok,
        &json!({
            "name": "Idul Fitri", "date": "2091-03-20",
            "jointLeaveDates": ["2091-03-24", "2091-03-23"]
        }),
    )
    .await;
    res.assert_status(201).assert_no_sql_error();
    let parent = res.json.clone();
    let mut want = HOLIDAY_KEYS.to_vec();
    want.push("jointLeaves");
    assert_eq!(keys_of(&parent), want);
    assert_eq!(parent["companyId"], fx.id);
    assert_eq!(parent["type"], "NATIONAL_HOLIDAY");
    assert_eq!(parent["isActive"], true);
    assert_eq!(parent["deductsLeave"], false);
    assert_eq!(parent["parentHolidayId"], Value::Null);
    assert_eq!(parent["updatedAt"], Value::Null);
    assert_eq!(parent["date"], "2091-03-20T00:00:00.000Z");
    let pid = parent["id"].as_i64().unwrap();
    let kids = parent["jointLeaves"].as_array().unwrap().clone();
    assert_eq!(kids.len(), 2);
    assert_eq!(kids[0]["date"], "2091-03-23T00:00:00.000Z", "urut tanggal");
    assert_eq!(kids[0]["name"], "Cuti Bersama Idul Fitri");
    assert_eq!(kids[0]["type"], "JOINT_LEAVE");
    assert_eq!(kids[0]["deductsLeave"], true);
    assert_eq!(kids[0]["parentHolidayId"], pid);
    assert_eq!(keys_of(&kids[0]), HOLIDAY_KEYS.to_vec());
    let (k23, k24) = (
        kids[0]["id"].as_i64().unwrap(),
        kids[1]["id"].as_i64().unwrap(),
    );

    // Efek samping: satu job `leave.holiday-sync` per baris (sesudah commit).
    // (Redis uji dipakai bersama antar-run dan id baris dipakai ulang: cek `contains`.)
    assert!(
        sync_payload(&app, pid)
            .await
            .contains(&json!({"name": "leave.holiday-sync",
                "data": {"companyId": fx.id, "holidayId": pid, "currentDate": "2091-03-20"}}))
    );
    assert!(!sync_jobs(&app, k23).await.is_empty());
    assert!(!sync_jobs(&app, k24).await.is_empty());

    // Libur berdiri sendiri: default deductsLeave mengikuti type.
    let res = post_json(
        &app,
        tok,
        &json!({ "name": "Bridge", "date": "2091-05-02", "type": "JOINT_LEAVE" }),
    )
    .await;
    res.assert_status(201);
    assert_eq!(res.json["deductsLeave"], true);
    assert_eq!(res.json["jointLeaves"], json!([]));

    // List: baris tetap datar, induk membawa cuti bersamanya.
    let res = app.get("/leave-holidays?year=2091").token(tok).send().await;
    res.assert_status(200).assert_no_sql_error();
    let rows = res.json.as_array().unwrap();
    assert_eq!(rows.len(), 4);
    let dates: Vec<&str> = rows.iter().map(|r| r["date"].as_str().unwrap()).collect();
    assert!(dates.windows(2).all(|w| w[0] <= w[1]), "urut tanggal");
    assert_eq!(rows[0]["id"], pid);
    assert_eq!(rows[0]["jointLeaves"].as_array().unwrap().len(), 2);
    assert_eq!(rows[1]["jointLeaves"], json!([]));
    let count = |q: &str| {
        let app = &app;
        let q = q.to_owned();
        async move {
            let r = app
                .get(&format!("/leave-holidays?year=2091{q}"))
                .token(tok)
                .send()
                .await;
            r.assert_status(200);
            r.json.as_array().unwrap().len()
        }
    };
    assert_eq!(count("&type=JOINT_LEAVE").await, 3);
    assert_eq!(count("&type=NATIONAL_HOLIDAY").await, 1);
    assert_eq!(count("&deductsLeave=true").await, 3);
    assert_eq!(count("&deductsLeave=false").await, 1);
    // `@Type(() => Boolean)`: "false" → true (perilaku Node dipertahankan); kosong → false.
    assert_eq!(count("&isActive=false").await, 4);
    assert_eq!(count("&isActive=").await, 0);
    assert_eq!(count("&isActive=true").await, 4);

    // Detail.
    let res = app
        .get(&format!("/leave-holidays/{pid}"))
        .token(&fx.staff.token)
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(res.json["id"], pid);
    assert_eq!(res.json["jointLeaves"].as_array().unwrap().len(), 2);

    // Update: ganti nama + geser tanggal + ganti daftar cuti bersama (24 dipertahankan,
    // 23 dihapus, 25 ditambah).
    let res = app
        .put(&format!("/leave-holidays/{pid}"))
        .token(tok)
        .json(&json!({
            "name": "Idul Fitri 2091", "date": "2091-03-21",
            "jointLeaveDates": ["2091-03-24", "2091-03-25"]
        }))
        .send()
        .await;
    res.assert_status(200).assert_no_sql_error();
    assert_eq!(res.json["name"], "Idul Fitri 2091");
    assert_eq!(res.json["date"], "2091-03-21T00:00:00.000Z");
    assert!(res.json["updatedAt"].is_string());
    let kids = res.json["jointLeaves"].as_array().unwrap().clone();
    assert_eq!(kids.len(), 2);
    assert_eq!(
        kids[0]["id"], k24,
        "tanggal yang dipertahankan = baris yang sama"
    );
    assert_eq!(kids[0]["name"], "Cuti Bersama Idul Fitri 2091");
    assert_eq!(kids[1]["date"], "2091-03-25T00:00:00.000Z");
    assert_eq!(kids[1]["deductsLeave"], true);
    let k25 = kids[1]["id"].as_i64().unwrap();
    let gone: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "companyHoliday" WHERE id = {k23}"#
        ))
        .await;
    assert_eq!(gone, 0);
    // Job: induk membawa tanggal lama; yang dihapus membawa tanggalnya sendiri.
    let payloads = sync_payload(&app, pid).await;
    assert!(payloads.contains(&json!({"name": "leave.holiday-sync",
        "data": {"companyId": fx.id, "holidayId": pid, "previousDate": "2091-03-20", "currentDate": "2091-03-21"}})));
    assert!(
        sync_payload(&app, k23).await.contains(&json!({"name": "leave.holiday-sync",
        "data": {"companyId": fx.id, "holidayId": k23, "previousDate": "2091-03-23", "currentDate": "2091-03-23"}}))
    );
    assert!(!sync_jobs(&app, k25).await.is_empty());

    // Aturan penolakan.
    let res = app
        .put(&format!("/leave-holidays/{pid}"))
        .token(tok)
        .json(&json!({ "jointLeaveDates": ["2091-03-21"] }))
        .send()
        .await;
    res.assert_status(400);
    let res = app
        .put(&format!("/leave-holidays/{k24}"))
        .token(tok)
        .json(&json!({ "jointLeaveDeductsLeave": false }))
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        "Cuti bersama tidak bisa memiliki cuti bersama sendiri"
    );
    let res = app
        .put(&format!("/leave-holidays/{pid}"))
        .token(tok)
        .json(&json!({ "type": "JOINT_LEAVE" }))
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        "Hapus dulu cuti bersama libur ini sebelum mengubah jenisnya"
    );
    app.put("/leave-holidays/0")
        .token(tok)
        .json(&json!({ "name": "x" }))
        .send()
        .await
        .assert_status(404);
    let res = app
        .put(&format!("/leave-holidays/{pid}"))
        .token(tok)
        .json(&json!({ "date": "2091-02-30" }))
        .send()
        .await;
    res.assert_status(400);

    // jointLeaveDeductsLeave mengubah cuti bersama yang dipertahankan.
    let res = app
        .put(&format!("/leave-holidays/{pid}"))
        .token(tok)
        .json(&json!({ "jointLeaveDeductsLeave": false }))
        .send()
        .await;
    res.assert_status(200);
    for k in res.json["jointLeaves"].as_array().unwrap() {
        assert_eq!(k["deductsLeave"], false);
    }

    // Toggle: cuti bersama ikut; respons tanpa jointLeaves.
    let res = app
        .patch(&format!("/leave-holidays/{pid}/toggle"))
        .token(tok)
        .send()
        .await;
    res.assert_status(200).assert_no_sql_error();
    assert_eq!(keys_of(&res.json), HOLIDAY_KEYS.to_vec());
    assert_eq!(res.json["isActive"], false);
    let active: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "companyHoliday" WHERE "parentHolidayId" = {pid} AND "isActive""#
        ))
        .await;
    assert_eq!(active, 0);
    let res = app
        .patch(&format!("/leave-holidays/{pid}/toggle"))
        .token(tok)
        .send()
        .await;
    assert_eq!(res.json["isActive"], true);
    let active: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "companyHoliday" WHERE "parentHolidayId" = {pid} AND "isActive""#
        ))
        .await;
    assert_eq!(active, 2);
    app.patch("/leave-holidays/0/toggle")
        .token(tok)
        .send()
        .await
        .assert_status(404);

    // Hapus: cuti bersama ikut terhapus (FK cascade), job pembalikan dipasang.
    let res = app
        .delete(&format!("/leave-holidays/{pid}"))
        .token(tok)
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(res.json, json!({ "success": true }));
    let left: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "companyHoliday" WHERE id = {pid} OR "parentHolidayId" = {pid}"#
        ))
        .await;
    assert_eq!(left, 0);
    assert!(
        sync_payload(&app, k24).await.contains(&json!({"name": "leave.holiday-sync",
        "data": {"companyId": fx.id, "holidayId": k24, "previousDate": "2091-03-24", "currentDate": "2091-03-24"}}))
    );
    app.get(&format!("/leave-holidays/{pid}"))
        .token(tok)
        .send()
        .await
        .assert_status(404);
    app.delete(&format!("/leave-holidays/{pid}"))
        .token(tok)
        .send()
        .await
        .assert_status(404);
}

#[tokio::test]
async fn libur_company_lain_tidak_terlihat() {
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let fx = app.company().await;
    let other = app.other_company_admin().await;
    clean_year(&app, fx.id, 2092).await;

    let res = post_json(
        &app,
        &fx.admin.token,
        &json!({ "name": "Rahasia", "date": "2092-06-01" }),
    )
    .await;
    res.assert_status(201);
    let id = res.json["id"].as_i64().unwrap();

    let t = &other.admin.token;
    app.get(&format!("/leave-holidays/{id}"))
        .token(t)
        .send()
        .await
        .assert_status(404);
    app.put(&format!("/leave-holidays/{id}"))
        .token(t)
        .json(&json!({ "name": "x" }))
        .send()
        .await
        .assert_status(404);
    app.patch(&format!("/leave-holidays/{id}/toggle"))
        .token(t)
        .send()
        .await
        .assert_status(404);
    app.delete(&format!("/leave-holidays/{id}"))
        .token(t)
        .send()
        .await
        .assert_status(404);
    let res = app.get("/leave-holidays?year=2092").token(t).send().await;
    res.assert_status(200);
    assert_eq!(res.json, json!([]));
    let still: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "companyHoliday" WHERE id = {id} AND name = 'Rahasia' AND "isActive""#
        ))
        .await;
    assert_eq!(still, 1);
}

#[tokio::test]
async fn impor_csv_pratinjau_dan_simpan() {
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let fx = app.company().await;
    let tok = &fx.admin.token;
    clean_year(&app, fx.id, 2093).await;

    // Contoh CSV (BOM + header).
    let res = app
        .get("/leave-holidays/import/template")
        .token(tok)
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(res.headers["content-type"], "text/csv; charset=utf-8");
    assert_eq!(
        res.headers["content-disposition"],
        "attachment; filename=\"contoh-impor-libur.csv\""
    );
    assert_eq!(
        res.bytes,
        include_bytes!("../assets/holiday-import.template.csv")
    );
    assert_eq!(&res.bytes[..3], &[0xEF, 0xBB, 0xBF]);

    let csv = "tanggal,nama,jenis,potong_cuti\n\
2093-01-01,Tahun Baru,Libur Nasional,tidak\n\
2093-02-08,Cuti Bersama Imlek,Cuti Bersama,ya\n\
bad,Baris Rusak,x,y\n\
2093-01-01,Dobel,Libur Nasional,tidak\n";

    // Libur aktif yang sudah ada di 2093-01-01 dilewati.
    let existing = post_json(
        &app,
        tok,
        &json!({ "name": "Sudah Ada", "date": "2093-01-01" }),
    )
    .await;
    existing.assert_status(201);
    let existing_id = existing.json["id"].as_i64().unwrap();

    let res = upload(&app, "/leave-holidays/import/preview", tok, csv).await;
    res.assert_status(200).assert_no_sql_error();
    let data = &res.json["data"];
    assert_eq!(
        keys_of(data),
        [
            "toAdd",
            "skippedExisting",
            "invalid",
            "deductingJointLeaveCount"
        ]
    );
    assert_eq!(
        data["skippedExisting"],
        json!([{ "row": 2, "date": "2093-01-01", "name": "Tahun Baru", "existingName": "Sudah Ada" }])
    );
    assert_eq!(
        data["toAdd"],
        json!([{ "row": 3, "date": "2093-02-08", "name": "Cuti Bersama Imlek",
                 "type": "JOINT_LEAVE", "deductsLeave": true }])
    );
    assert_eq!(data["invalid"].as_array().unwrap().len(), 2);
    assert_eq!(data["invalid"][0]["row"], 4);
    assert_eq!(
        data["invalid"][0]["raw"],
        json!({ "tanggal": "bad", "nama": "Baris Rusak", "jenis": "x", "potong_cuti": "y" })
    );
    assert_eq!(
        data["invalid"][1]["errors"],
        json!(["tanggal duplikat dengan baris 2"])
    );
    assert_eq!(data["deductingJointLeaveCount"], 1);
    let n: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "companyHoliday" WHERE "companyId" = {} AND date >= '2093-01-01' AND date <= '2093-12-31'"#,
            fx.id
        ))
        .await;
    assert_eq!(n, 1, "pratinjau tidak menulis");

    // Impor sungguhan: hanya yang belum punya libur aktif.
    let res = upload(&app, "/leave-holidays/import", tok, csv).await;
    res.assert_status(201).assert_no_sql_error();
    let data = &res.json["data"];
    assert_eq!(data["created"], 1);
    assert_eq!(data["skippedExisting"], 1);
    assert_eq!(data["invalid"], 2);
    let h = &data["holidays"][0];
    assert_eq!(keys_of(h), HOLIDAY_KEYS.to_vec());
    assert_eq!(h["type"], "JOINT_LEAVE");
    assert_eq!(h["deductsLeave"], true);
    assert_eq!(h["parentHolidayId"], Value::Null);
    assert_eq!(h["date"], "2093-02-08T00:00:00.000Z");
    let new_id = h["id"].as_i64().unwrap();
    assert!(
        sync_payload(&app, new_id)
            .await
            .contains(&json!({"name": "leave.holiday-sync",
                "data": {"companyId": fx.id, "holidayId": new_id, "currentDate": "2093-02-08"}}))
    );

    // Impor ulang: tidak ada yang baru.
    let res = upload(&app, "/leave-holidays/import", tok, csv).await;
    res.assert_status(201);
    assert_eq!(res.json["data"]["created"], 0);
    assert_eq!(res.json["data"]["skippedExisting"], 2);
    assert_eq!(res.json["data"]["holidays"], json!([]));

    // Libur nonaktif tidak menghalangi.
    app.exec(&format!(
        r#"UPDATE "companyHoliday" SET "isActive" = false WHERE id = {existing_id}"#
    ))
    .await;
    let res = upload(&app, "/leave-holidays/import/preview", tok, csv).await;
    assert_eq!(res.json["data"]["toAdd"].as_array().unwrap().len(), 1);
    assert_eq!(res.json["data"]["toAdd"][0]["row"], 2);
    assert_eq!(
        res.json["data"]["skippedExisting"][0]["existingName"], "Cuti Bersama Imlek",
        "tanggal 02-08 sekarang sudah ada"
    );

    // Penolakan berkas.
    let invalid = |msg: &str| json!({ "code": "IMPORT_INVALID_FILE", "message": msg });
    for uri in ["/leave-holidays/import/preview", "/leave-holidays/import"] {
        let res = app.post(uri).token(tok).send().await;
        res.assert_status(422);
        assert_eq!(res.json, invalid("No file uploaded"));

        let (ct, body) = multipart("file", "libur.xlsx", "text/csv", b"a");
        let res = app.post(uri).token(tok).raw(&ct, body).send().await;
        res.assert_status(422);
        assert_eq!(res.json, invalid("File must have a .csv extension"));

        let (ct, body) = multipart("file", "libur.csv", "image/png", b"a");
        let res = app.post(uri).token(tok).raw(&ct, body).send().await;
        res.assert_status(422);
        assert_eq!(
            res.json,
            invalid("File type is not supported, upload a CSV file")
        );

        let res = upload(
            &app,
            uri,
            tok,
            "tanggal,nama,jenis\n2093-01-01,A,Libur Nasional\n",
        )
        .await;
        res.assert_status(422);
        assert_eq!(res.json, invalid("Kolom wajib tidak ada: potong_cuti"));

        let res = upload(
            &app,
            uri,
            tok,
            "tanggal,nama,jenis,potong_cuti\n2093-01-01,A\n",
        )
        .await;
        res.assert_status(422);
        assert_eq!(
            res.json,
            invalid("Invalid Record Length: columns length is 4, got 2 on line 2")
        );

        let (ct, body) = multipart("lain", "libur.csv", "text/csv", b"a");
        let res = app.post(uri).token(tok).raw(&ct, body).send().await;
        res.assert_status(400);
        assert_eq!(
            res.json,
            json!({ "message": "Unexpected field", "error": "Bad Request", "statusCode": 400 })
        );

        let big = vec![b'x'; 300 * 1024];
        let (ct, body) = multipart("file", "libur.csv", "text/csv", &big);
        let res = app.post(uri).token(tok).raw(&ct, body).send().await;
        res.assert_status(413);
        assert_eq!(
            res.json,
            json!({ "code": "IMPORT_FILE_TOO_LARGE", "message": "File exceeds the 256 KB limit" })
        );
    }
    // 367 baris data ditolak.
    let mut many = String::from("tanggal,nama,jenis,potong_cuti\n");
    for i in 0..367 {
        many.push_str(&format!("2093-01-01,N{i},Libur Nasional,tidak\n"));
    }
    let res = upload(&app, "/leave-holidays/import/preview", tok, &many).await;
    res.assert_status(422);
    assert_eq!(res.json, invalid("Maksimal 366 baris data per file"));
}

#[tokio::test]
async fn pengingat_libur_tahun_depan() {
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let fx = app.company().await;
    clean_year(&app, fx.id, 2096).await;
    let at = chrono_utc;
    let st = app.state();

    // November: tidak tampil.
    let r = mz_leave_holidays::next_year_reminder(st, fx.id, at("2095-11-20 12:00:00"))
        .await
        .unwrap();
    assert_eq!(r, json!({ "show": false, "year": 2096, "reason": null }));
    // Desember tanpa libur tahun depan: tampil.
    let r = mz_leave_holidays::next_year_reminder(st, fx.id, at("2095-12-15 12:00:00"))
        .await
        .unwrap();
    assert_eq!(
        r,
        json!({ "show": true, "year": 2096, "reason": "NEXT_YEAR_EMPTY" })
    );
    // Ada libur aktif tahun depan: tidak tampil; nonaktif tidak dihitung.
    let res = post_json(
        &app,
        &fx.admin.token,
        &json!({ "name": "Tahun Depan", "date": "2096-05-01" }),
    )
    .await;
    res.assert_status(201);
    let id = res.json["id"].as_i64().unwrap();
    let r = mz_leave_holidays::next_year_reminder(st, fx.id, at("2095-12-15 12:00:00"))
        .await
        .unwrap();
    assert_eq!(r, json!({ "show": false, "year": 2096, "reason": null }));
    app.exec(&format!(
        r#"UPDATE "companyHoliday" SET "isActive" = false WHERE id = {id}"#
    ))
    .await;
    let r = mz_leave_holidays::next_year_reminder(st, fx.id, at("2095-12-15 12:00:00"))
        .await
        .unwrap();
    assert_eq!(r["show"], true);

    // HTTP: bentuk `{ data: { show, year, reason } }`.
    let res = app
        .get("/leave-holidays/next-year-reminder")
        .token(&fx.admin.token)
        .send()
        .await;
    res.assert_status(200).assert_no_sql_error();
    assert_eq!(keys_of(&res.json["data"]), ["show", "year", "reason"]);
}

fn chrono_utc(s: &str) -> chrono::NaiveDateTime {
    chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").unwrap()
}
