//! Uji integrasi `POST /admin/national-holidays/apply` (operator) terhadap Postgres + Redis
//! nyata (database `mz_test_it_leave_hol_apply`): gerbang operator, validasi, dry run,
//! salin per company (idempoten, tanpa duplikat), job `leave.holiday-sync`, `operatorAuditLog`,
//! jalur `all: true` (admin job `APPLY_HOLIDAYS`) + handler job.
//! Jalankan: `MZ_TEST_DATABASE_URL=... cargo test -p mz-leave-holidays --test it_leave_hol_apply`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use axum::Router;
use fred::interfaces::HashesInterface;
use mz_core::AppState;
use mz_core::queue::jobs_key;
use mz_leave::jobs::LEAVE_SCHEDULE_QUEUE;
use mz_testkit::TestApp;
use serde_json::{Value, json};

fn build(state: &AppState) -> Router {
    mz_leave_holidays::router(state).with_state(state.clone())
}

fn uniq() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

async fn operator(app: &TestApp, role: &str) -> (i32, String) {
    let u = uniq();
    let id: i32 = app
        .scalar(&format!(
            r#"INSERT INTO operators (username, password, name, status, role)
               VALUES ('op{u}', 'x', 'IT Operator', 1, '{role}') RETURNING id"#
        ))
        .await;
    (
        id,
        app.token_with(json!({ "sub": id, "username": format!("op{u}") })),
    )
}

async fn company(app: &TestApp) -> (i32, String) {
    sqlx::query_as("INSERT INTO company (name, status) VALUES ($1, 1) RETURNING id, uuid::text")
        .bind(format!("it-leave-hol-{}", uniq()))
        .fetch_one(app.db())
        .await
        .unwrap()
}

async fn seed_national(app: &TestApp, year: i32) {
    app.exec(&format!(
        r#"INSERT INTO "nationalHoliday" (date, name, type, year, "isEstimated") VALUES
             ('{year}-01-01', 'Tahun Baru {year}', 'NATIONAL_HOLIDAY', {year}, false),
             ('{year}-03-10', 'Cuti Bersama Nyepi {year}', 'JOINT_LEAVE', {year}, true),
             ('{year}-08-17', 'Kemerdekaan {year}', 'NATIONAL_HOLIDAY', {year}, false)
           ON CONFLICT DO NOTHING"#
    ))
    .await;
}

type HolidayRow = (i32, String, String, String, bool);

async fn holiday_rows(app: &TestApp, company: i32, year: i32) -> Vec<HolidayRow> {
    sqlx::query_as(&format!(
        r#"SELECT id, to_char(date, 'YYYY-MM-DD'), name, type::text, "deductsLeave"
             FROM "companyHoliday" WHERE "companyId" = {company}
              AND date >= '{year}-01-01' AND date <= '{year}-12-31' ORDER BY date, id"#
    ))
    .fetch_all(app.db())
    .await
    .unwrap()
}

async fn sync_jobs(app: &TestApp, holiday_id: i32) -> usize {
    let keys: Vec<String> = app
        .state
        .redis
        .next()
        .hkeys(jobs_key(LEAVE_SCHEDULE_QUEUE))
        .await
        .unwrap();
    let prefix = format!("holiday-sync:{holiday_id}:");
    keys.iter().filter(|k| k.starts_with(&prefix)).count()
}

async fn audit(app: &TestApp, action: &str, op: i32) -> Vec<(Option<Value>, Option<String>)> {
    sqlx::query_as(
        r#"SELECT after, reason FROM "operatorAuditLog"
            WHERE action = $1 AND "operatorId" = $2 ORDER BY id"#,
    )
    .bind(action)
    .bind(op)
    .fetch_all(app.db())
    .await
    .unwrap()
}

/// Test yang membuat company / menjalankan job semua-company diserialkan.
static GLOBAL_JOB: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test]
async fn gerbang_operator_dan_validasi() {
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let fx = app.company().await;
    let (_, finance) = operator(&app, "FINANCE").await;
    let (_, owner) = operator(&app, "OWNER").await;
    let uri = "/admin/national-holidays/apply";

    let res = app.post(uri).json(&json!({})).send().await;
    res.assert_status(401);
    app.post(uri)
        .user(fx.super_admin.id)
        .json(&json!({}))
        .send()
        .await
        .assert_status(401);
    let res = app.post(uri).token(&finance).json(&json!({})).send().await;
    res.assert_status(403);
    assert_eq!(res.json["code"], "OPERATOR_ROLE_FORBIDDEN");

    // Validasi: pesan & urutan ValidationPipe.
    let res = app.post(uri).token(&owner).json(&json!({})).send().await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        json!([
            "year must not be greater than 2100",
            "year must not be less than 2000",
            "year must be an integer number",
            "dryRun must be a boolean value",
            "reason must be longer than or equal to 5 characters",
            "reason must be a string"
        ])
    );
    let res = app
        .post(uri)
        .token(&owner)
        .json(&json!({ "year": 2094, "dryRun": false, "reason": "abcde", "x": 1 }))
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(res.json["message"], json!(["property x should not exist"]));

    // Target: tepat satu dari companyUuids / all.
    let base = |extra: Value| {
        let mut b = json!({ "year": 2094, "dryRun": false, "reason": "Salin libur" });
        b.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        b
    };
    for body in [
        base(json!({})),
        base(json!({ "all": false })),
        base(json!({ "all": true, "companyUuids": [uuid::Uuid::new_v4().to_string()] })),
    ] {
        let res = app.post(uri).token(&owner).json(&body).send().await;
        res.assert_status(400);
        assert_eq!(
            res.json,
            json!({ "code": "INVALID_TARGET", "message": "provide exactly one of companyUuids or all: true" })
        );
    }
    let res = app
        .post(uri)
        .token(&owner)
        .json(&base(json!({ "all": true, "dryRun": true })))
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(res.json["code"], "DRY_RUN_NOT_SUPPORTED_FOR_ALL");
    let res = app
        .post(uri)
        .token(&owner)
        .json(&base(
            json!({ "companyUuids": [uuid::Uuid::new_v4().to_string()] }),
        ))
        .send()
        .await;
    res.assert_status(404);
    assert_eq!(
        res.json,
        json!({ "code": "COMPANY_NOT_FOUND", "message": "one or more companies were not found" })
    );

    // PATCH/DELETE pada path statis jatuh ke `:uuid` (ParseUUIDPipe) seperti Nest.
    for m in ["PATCH", "DELETE"] {
        let m = http::Method::from_bytes(m.as_bytes()).unwrap();
        let res = app
            .request(m.clone(), uri)
            .token(&owner)
            .json(&json!({ "reason": "Hapus libur" }))
            .send()
            .await;
        res.assert_status(400);
        assert_eq!(res.json["message"], "Validation failed (uuid is expected)");
        app.request(m, uri)
            .token(&finance)
            .json(&json!({}))
            .send()
            .await
            .assert_status(403);
    }
}

#[tokio::test]
async fn salin_libur_nasional_dry_run_dan_sungguhan() {
    // Membuat company baru: jangan berbarengan dengan job semua-company.
    let _g = GLOBAL_JOB.lock().await;
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let fx = app.company().await;
    let (op_id, owner) = operator(&app, "SUPPORT").await;
    seed_national(&app, 2094).await;
    app.exec(&format!(
        r#"DELETE FROM "companyHoliday" WHERE "companyId" = {} AND date >= '2094-01-01' AND date <= '2094-12-31'"#,
        fx.id
    ))
    .await;
    let (b_id, b_uuid) = company(&app).await;
    // B sudah punya salah satunya (nama beda huruf besar: pembanding tanpa peka huruf).
    app.exec(&format!(
        r#"INSERT INTO "companyHoliday" ("companyId", name, date, type)
           VALUES ({b_id}, 'TAHUN BARU 2094', '2094-01-01', 'NATIONAL_HOLIDAY')"#
    ))
    .await;
    let uri = "/admin/national-holidays/apply";

    // Dry run: tidak menulis, tidak memasang job, tetap diaudit.
    let res = app
        .post(uri)
        .token(&owner)
        .json(&json!({
            "year": 2094, "companyUuids": [b_uuid], "dryRun": true, "reason": "Pratinjau libur"
        }))
        .send()
        .await;
    res.assert_status(200).assert_no_sql_error();
    assert_eq!(
        res.json,
        json!({ "data": [{
            "companyUuid": b_uuid,
            "created": [
                { "date": "2094-03-10", "name": "Cuti Bersama Nyepi 2094", "type": "JOINT_LEAVE" },
                { "date": "2094-08-17", "name": "Kemerdekaan 2094", "type": "NATIONAL_HOLIDAY" }
            ],
            "skipped": 1
        }] })
    );
    assert_eq!(holiday_rows(&app, b_id, 2094).await.len(), 1);
    let a = audit(&app, "national_holiday.apply", op_id).await;
    assert_eq!(a.len(), 1);
    assert_eq!(
        a[0].0,
        Some(json!({ "year": 2094, "dryRun": true, "companies": 1, "created": 2, "skipped": 1 }))
    );
    assert_eq!(a[0].1.as_deref(), Some("Pratinjau libur"));

    // Sungguhan, dua company (urutan respons = urutan id, bukan urutan permintaan).
    let res = app
        .post(uri)
        .token(&owner)
        .json(&json!({
            "year": 2094, "companyUuids": [b_uuid, fx.uuid, b_uuid], "dryRun": false,
            "reason": "Salin libur 2094"
        }))
        .send()
        .await;
    res.assert_status(200).assert_no_sql_error();
    let data = res.json["data"].as_array().unwrap().clone();
    assert_eq!(data.len(), 2);
    assert_eq!(data[0]["companyUuid"], fx.uuid);
    assert_eq!(data[0]["created"].as_array().unwrap().len(), 3);
    assert_eq!(data[0]["skipped"], 0);
    assert_eq!(data[1]["companyUuid"], b_uuid);
    assert_eq!(data[1]["created"].as_array().unwrap().len(), 2);
    assert_eq!(data[1]["skipped"], 1);
    assert_eq!(
        keys(&data[1]["created"][0]),
        ["date", "name", "type"],
        "id tidak ikut respons"
    );

    let rows = holiday_rows(&app, fx.id, 2094).await;
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[1].2, "Cuti Bersama Nyepi 2094");
    assert_eq!(rows[1].3, "JOINT_LEAVE");
    assert!(rows[1].4, "cuti bersama memotong cuti");
    assert!(!rows[0].4 && !rows[2].4, "libur nasional tidak memotong");
    for (id, ..) in &rows {
        // Redis uji dipakai bersama antar-run (id baris dipakai ulang): minimal satu.
        assert!(
            sync_jobs(&app, *id).await >= 1,
            "job sinkron per baris baru"
        );
    }
    assert_eq!(holiday_rows(&app, b_id, 2094).await.len(), 3);

    // Idempoten.
    let res = app
        .post(uri)
        .token(&owner)
        .json(&json!({
            "year": 2094, "companyUuids": [fx.uuid], "dryRun": false, "reason": "Ulang salin"
        }))
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(res.json["data"][0]["created"], json!([]));
    assert_eq!(res.json["data"][0]["skipped"], 3);
    assert_eq!(holiday_rows(&app, fx.id, 2094).await.len(), 3);

    let a = audit(&app, "national_holiday.apply", op_id).await;
    assert_eq!(a.len(), 3);
    assert_eq!(
        a[1].0,
        Some(json!({ "year": 2094, "dryRun": false, "companies": 2, "created": 5, "skipped": 1 }))
    );
}

fn keys(v: &Value) -> Vec<&str> {
    v.as_object().unwrap().keys().map(String::as_str).collect()
}

#[tokio::test]
async fn semua_company_lewat_admin_job() {
    let _g = GLOBAL_JOB.lock().await;
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let fx = app.company().await;
    let (op_id, owner) = operator(&app, "OWNER").await;
    seed_national(&app, 2095).await;
    let uri = "/admin/national-holidays/apply";
    // Pastikan tidak ada job global aktif sisa uji lain.
    app.exec(
        r#"UPDATE "adminJob" SET status = 'FAILED' WHERE "companyId" IS NULL AND status IN ('QUEUED', 'RUNNING')"#,
    )
    .await;
    let body = json!({ "year": 2095, "all": true, "dryRun": false, "reason": "Salin semua libur" });

    let res = app.post(uri).token(&owner).json(&body).send().await;
    res.assert_status(202).assert_no_sql_error();
    let job = res.json["data"]["jobUuid"].as_str().unwrap().to_owned();
    assert_eq!(keys(&res.json["data"]), ["jobUuid"]);
    let row: (String, String, Option<i32>, Value, bool, i32) = sqlx::query_as(
        r#"SELECT type::text, status::text, "companyId", params, "dryRun", "operatorId"
             FROM "adminJob" WHERE uuid = $1::uuid"#,
    )
    .bind(&job)
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!(row.0, "APPLY_HOLIDAYS");
    assert_eq!(row.1, "QUEUED");
    assert_eq!(row.2, None);
    assert_eq!(row.3, json!({ "year": 2095 }));
    assert!(!row.4);
    assert_eq!(row.5, op_id);
    let created = audit(&app, "admin_job.create", op_id).await;
    assert_eq!(created.len(), 1);
    assert_eq!(created[0].1.as_deref(), Some("Salin semua libur"));
    // Tanpa baris `national_holiday.apply` (Node langsung kembali sesudah createJob).
    assert!(
        audit(&app, "national_holiday.apply", op_id)
            .await
            .is_empty()
    );

    // Job kedua selagi yang pertama aktif = 409.
    let res = app.post(uri).token(&owner).json(&body).send().await;
    res.assert_status(409);

    // Handler job: semua company aktif, urut id, log per company, hasil ringkas.
    let logs = std::sync::Mutex::new(Vec::<String>::new());
    let log = |l: &str| logs.lock().unwrap().push(l.to_owned());
    let params = json!({ "year": 2095 }).as_object().unwrap().clone();
    let out = mz_leave_holidays::run_apply_holidays_job(app.state(), &params, &log)
        .await
        .unwrap();
    let active: i64 = app
        .scalar("SELECT COUNT(*) FROM company WHERE status <> 0")
        .await;
    assert_eq!(out["companies"], active);
    assert_eq!(
        out["created"].as_i64().unwrap() + out["skipped"].as_i64().unwrap(),
        active * 3
    );
    let logs = logs.into_inner().unwrap();
    assert_eq!(logs.len() as i64, active);
    assert!(
        logs.iter()
            .any(|l| l.starts_with(&format!("company {}: created ", fx.uuid)))
    );
    assert_eq!(holiday_rows(&app, fx.id, 2095).await.len(), 3);
    // Jalan ulang tidak membuat apa pun.
    let out = mz_leave_holidays::run_apply_holidays_job(app.state(), &params, &|_| {})
        .await
        .unwrap();
    assert_eq!(out["created"], 0);
    assert_eq!(out["skipped"].as_i64().unwrap(), active * 3);
    assert!(
        mz_leave_holidays::run_apply_holidays_job(app.state(), &serde_json::Map::new(), &|_| {})
            .await
            .is_err()
    );
    app.exec(&format!(
        r#"UPDATE "adminJob" SET status = 'DONE' WHERE uuid = '{job}'"#
    ))
    .await;
}
